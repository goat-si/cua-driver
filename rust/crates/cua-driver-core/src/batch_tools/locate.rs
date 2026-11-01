//! Find windows and elements by description, wait for them, and check them.
//!
//! Used by `run_steps` steps that name their target (`role`, `name`, `app`,
//! `window`) instead of carrying a token from an earlier read, and by the
//! per-step `wait_for` / `expect` checks. Every read goes through the same
//! [`ToolRegistry`] entry point a direct call uses (`list_windows`,
//! `list_apps`, `get_window_state`), so a lookup is held to the same session,
//! policy and approval checks as the reads a model would make itself.
//!
//! Lookup reads ask for `full_output`, which the `since` snapshot memory does
//! not remember, so they never become the baseline of the caller's next
//! `since:"latest"` diff.
//!
//! `incomplete_observation` means a read cannot prove absence or uniqueness.
//! Verified closed-menu exclusions guard menu content, and verified collapsed
//! list exclusions guard list rows and cells. Both guard role-less and textual
//! content selectors. Unverified content makes the entire read incomplete;
//! unknown exclusion kinds guard every selector. Owner indices cannot scope
//! these checks because selectors do not encode ancestry.
//! Raw markdown fallback lines come from producer-sanitized markdown; structured
//! candidate descriptions independently redact secure names and values.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::{
    protocol::{Content, ToolResult},
    tool::ToolRegistry,
};

/// Keys a step may add to an action's arguments to name its target. They are
/// removed before the action tool runs.
pub(crate) const SELECTOR_KEYS: &[&str] = &["role", "name", "label", "nth", "app", "window"];
/// Keys a `wait_for` / `expect` check accepts.
pub(crate) const CHECK_KEYS: &[&str] = &[
    "role",
    "name",
    "label",
    "text",
    "nth",
    "app",
    "window",
    "pid",
    "window_id",
    "gone",
    "value",
    "value_contains",
    "enabled",
    "selected",
    "timeout_ms",
];

/// Default wait for a named target to appear before its action runs.
pub(crate) const DEFAULT_FIND_TIMEOUT_MS: u64 = 3_000;
/// Default wait for a `wait_for` check.
pub(crate) const DEFAULT_WAIT_TIMEOUT_MS: u64 = 5_000;
/// Default wait for an `expect` check.
pub(crate) const DEFAULT_EXPECT_TIMEOUT_MS: u64 = 2_000;
/// Longest wait any single check may ask for.
pub(crate) const MAX_CHECK_TIMEOUT_MS: u64 = 10_000;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const CANDIDATES_SHOWN: usize = 5;
/// Most windows of one app a lookup reads per poll.
const MAX_WINDOWS_SEARCHED: usize = 4;

tokio::task_local! {
    static LOCATOR_READ_DEADLINE: Duration;
}

struct LocatorClock(Instant);

impl crate::actionability::Clock for LocatorClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(
        &self,
        duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

fn locator_clock(
    deadline: Instant,
) -> (Arc<dyn crate::actionability::Clock + Send + Sync>, Duration) {
    let clock: Arc<dyn crate::actionability::Clock + Send + Sync> =
        Arc::new(LocatorClock(Instant::now()));
    let remaining = deadline.saturating_duration_since(Instant::now());
    let deadline = clock.now().saturating_add(remaining);
    (clock, deadline)
}

/// Which window a step or check addresses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WindowSpec {
    pub pid: Option<i64>,
    pub window_id: Option<u64>,
    /// App name (case-insensitive, exact or substring) or bundle id.
    pub app: Option<String>,
    /// Case-insensitive substring of the window title.
    pub title: Option<String>,
}

impl WindowSpec {
    pub fn is_empty(&self) -> bool {
        self.pid.is_none() && self.window_id.is_none() && self.app.is_none() && self.title.is_none()
    }

    /// A spec that names one exact window needs no lookup.
    fn exact(&self) -> Option<(i64, u64)> {
        match (self.pid, self.window_id, &self.app, &self.title) {
            (Some(pid), Some(window_id), None, None) => Some((pid, window_id)),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(app) = &self.app {
            parts.push(format!("app \"{app}\""));
        }
        if let Some(title) = &self.title {
            parts.push(format!("window \"{title}\""));
        }
        if let Some(pid) = self.pid {
            parts.push(format!("pid {pid}"));
        }
        if let Some(window_id) = self.window_id {
            parts.push(format!("window_id {window_id}"));
        }
        if parts.is_empty() {
            "no window".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

/// Which element inside the window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ElementSpec {
    pub role: Option<String>,
    pub name: Option<String>,
    /// Any row of the tree containing this text, display-only rows included.
    pub text: Option<String>,
    pub nth: Option<usize>,
    /// The tool that will act on the element, when known. Breaks a tie
    /// between same-named rows: set_value means the field, not its label.
    pub action: Option<&'static str>,
}

/// `max_elements` of a visible-window lookup read: the platform default walk
/// budget. An `observe` (or script read) of the same plain view would share
/// its `since` history, so a batch whose observation baseline uses this
/// budget skips the visible read and keeps the full one.
const VISIBLE_READ_MAX_ELEMENTS: u64 = 2000;

impl ElementSpec {
    /// Whether a unique match in the visible window can stand for the full
    /// read: a role is named and is not a menu role (closed menus are only in
    /// the full read), and no `nth` or `text` asks for a position or a row
    /// among all of them.
    fn visible_lookup_suffices(&self) -> bool {
        self.nth.is_none()
            && self.text.is_none()
            && self.name.is_some()
            && self
                .role
                .as_deref()
                .is_some_and(|role| !role.to_ascii_lowercase().contains("menu"))
    }

    /// Whether `found`'s label is this spec's name, case aside.
    fn names_exactly(&self, found: &Found) -> bool {
        let label = found
            .element
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.name
            .as_deref()
            .is_some_and(|name| name.trim().to_lowercase() == label.trim().to_lowercase())
    }

    pub fn is_empty(&self) -> bool {
        self.role.is_none() && self.name.is_none() && self.text.is_none()
    }

    pub fn describe(&self) -> String {
        let mut out = String::new();
        if let Some(role) = &self.role {
            out.push_str(role);
        } else {
            out.push_str("element");
        }
        if let Some(name) = &self.name {
            out.push_str(&format!(" \"{name}\""));
        }
        if let Some(text) = &self.text {
            out.push_str(&format!(" with text \"{text}\""));
        }
        if let Some(nth) = self.nth {
            out.push_str(&format!(" (nth {nth})"));
        }
        out
    }
}

/// A `wait_for` or `expect` predicate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Check {
    pub window: WindowSpec,
    pub element: ElementSpec,
    pub gone: bool,
    pub value: Option<String>,
    pub value_contains: Option<String>,
    pub enabled: Option<bool>,
    pub selected: Option<bool>,
    pub timeout: Duration,
}

impl Check {
    pub fn parse(raw: &Value, default_timeout_ms: u64) -> Result<Self, String> {
        let object = raw
            .as_object()
            .ok_or("must be an object such as {\"role\":\"button\",\"name\":\"Save\"}")?;
        if let Some(key) = object
            .keys()
            .find(|key| !CHECK_KEYS.contains(&key.as_str()))
        {
            return Err(format!(
                "unknown field `{key}`; a check takes {}",
                CHECK_KEYS.join(", ")
            ));
        }
        let element = element_spec(object)?;
        if element.is_empty() {
            return Err("needs `role`, `name` or `text` to say which element".to_owned());
        }
        let window = window_spec(object)?;
        let gone = opt_bool(object, "gone")?.unwrap_or(false);
        let value = opt_string(object, "value")?;
        let value_contains = opt_string(object, "value_contains")?;
        let enabled = opt_bool(object, "enabled")?;
        let selected = opt_bool(object, "selected")?;
        if gone
            && (value.is_some()
                || value_contains.is_some()
                || enabled.is_some()
                || selected.is_some())
        {
            return Err("`gone:true` cannot be combined with value/enabled/selected".to_owned());
        }
        if element.text.is_some()
            && element.role.is_none()
            && element.name.is_none()
            && (value.is_some()
                || value_contains.is_some()
                || enabled.is_some()
                || selected.is_some())
        {
            return Err(
                "value/enabled/selected need `role` or `name`; `text` alone only checks that the text is shown"
                    .to_owned(),
            );
        }
        let timeout_ms = match object.get("timeout_ms") {
            None | Some(Value::Null) => default_timeout_ms,
            Some(value) => value
                .as_u64()
                .filter(|ms| *ms <= MAX_CHECK_TIMEOUT_MS)
                .ok_or_else(|| {
                    format!("`timeout_ms` must be an integer 0 to {MAX_CHECK_TIMEOUT_MS}")
                })?,
        };
        Ok(Self {
            window,
            element,
            gone,
            value,
            value_contains,
            enabled,
            selected,
            timeout: Duration::from_millis(timeout_ms),
        })
    }

    pub fn describe(&self) -> String {
        let mut out = self.element.describe();
        if self.gone {
            out.push_str(" is gone");
        } else {
            out.push_str(" is shown");
        }
        if let Some(value) = &self.value {
            out.push_str(&format!(", value \"{value}\""));
        }
        if let Some(value) = &self.value_contains {
            out.push_str(&format!(", value contains \"{value}\""));
        }
        if let Some(enabled) = self.enabled {
            out.push_str(if enabled { ", enabled" } else { ", disabled" });
        }
        if let Some(selected) = self.selected {
            out.push_str(if selected {
                ", selected"
            } else {
                ", not selected"
            });
        }
        out
    }
}

/// Split the selector keys out of an action's arguments.
pub(crate) fn take_selector(
    args: &mut Map<String, Value>,
) -> Result<(WindowSpec, ElementSpec), String> {
    let mut picked = Map::new();
    for key in SELECTOR_KEYS {
        if let Some(value) = args.remove(*key) {
            picked.insert((*key).to_owned(), value);
        }
    }
    let element = element_spec(&picked)?;
    if element.text.is_some() {
        return Err(
            "`text` only works in wait_for/expect; target an element by role and name".to_owned(),
        );
    }
    let mut window = window_spec(&picked)?;
    // A malformed pid/window_id is left to the tool's schema check, which
    // names the field the same way a direct call would.
    window.pid = args.get("pid").and_then(Value::as_i64);
    window.window_id = args.get("window_id").and_then(Value::as_u64);
    Ok((window, element))
}

fn element_spec(object: &Map<String, Value>) -> Result<ElementSpec, String> {
    let name = match (opt_string(object, "name")?, opt_string(object, "label")?) {
        (Some(_), Some(_)) => return Err("give `name` or `label`, not both".to_owned()),
        (name, label) => name.or(label),
    };
    let nth = match object.get("nth") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or("`nth` must be a non-negative integer (0 is the first match)")?
                as usize,
        ),
    };
    Ok(ElementSpec {
        role: opt_string(object, "role")?,
        name,
        text: opt_string(object, "text")?,
        nth,
        action: None,
    })
}

fn window_spec(object: &Map<String, Value>) -> Result<WindowSpec, String> {
    Ok(WindowSpec {
        pid: opt_i64(object, "pid")?,
        window_id: opt_u64(object, "window_id")?,
        app: opt_string(object, "app")?,
        title: opt_string(object, "window")?,
    })
}

fn opt_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.trim().to_owned())),
        Some(_) => Err(format!("`{key}` must be a non-empty string")),
    }
}

fn opt_bool(object: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(format!("`{key}` must be true or false")),
    }
}

fn opt_i64(object: &Map<String, Value>, key: &str) -> Result<Option<i64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be an integer")),
    }
}

fn opt_u64(object: &Map<String, Value>, key: &str) -> Result<Option<u64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
    }
}

/// One concrete window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub pid: i64,
    pub window_id: u64,
    pub app: Option<String>,
    pub title: Option<String>,
    pub on_screen: bool,
}

impl Window {
    pub fn describe(&self) -> String {
        let mut out = String::new();
        if let Some(app) = &self.app {
            out.push_str(app);
            out.push(' ');
        }
        out.push_str(&format!("window {}", self.window_id));
        if let Some(title) = self.title.as_deref().filter(|title| !title.is_empty()) {
            out.push_str(&format!(" \"{}\"", clip(title, 60)));
        }
        out
    }

    pub fn to_json(&self) -> Value {
        json!({
            "pid": self.pid,
            "window_id": self.window_id,
            "app": self.app,
            "title": self.title,
        })
    }
}

/// One accessibility read of a window.
pub(crate) struct Read {
    pub window: Window,
    pub elements: Vec<Value>,
    pub markdown: String,
    completeness: Completeness,
    scope_exclusions: Vec<ScopeExclusion>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopeExclusion {
    ClosedMenus,
    CollapsedLists,
    Unknown,
}

fn scope_exclusions(state: &Value) -> Vec<ScopeExclusion> {
    match state.get("observation_scope_exclusions") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item.get("kind").and_then(Value::as_str) {
                Some("closed_menus_not_expanded") => ScopeExclusion::ClosedMenus,
                Some("collapsed_lists_not_expanded") => ScopeExclusion::CollapsedLists,
                _ => ScopeExclusion::Unknown,
            })
            .collect(),
        Some(_) => vec![ScopeExclusion::Unknown],
    }
}

/// Classify content for exclusion guards without changing role matching.
fn is_content_role(role: &str) -> bool {
    use cua_driver_contract::Role;
    // AT-SPI uses `text` for editable fields, while other providers can use
    // the same token for static content. Guard the token conservatively.
    if normalized_role(role) == "text" {
        return true;
    }
    match Role::from_token(role) {
        Role::StaticText | Role::Heading | Role::Paragraph | Role::Image | Role::Link => true,
        Role::Unknown => matches!(
            role_family(role).as_str(),
            "statictext" | "heading" | "paragraph" | "image" | "link"
        ),
        _ => false,
    }
}

fn exclusion_guards(exclusion: ScopeExclusion, spec: &ElementSpec) -> bool {
    use cua_driver_contract::Role;
    if exclusion == ScopeExclusion::Unknown || spec.text.is_some() {
        return true;
    }
    let Some(role) = spec.role.as_deref() else {
        return true;
    };
    if is_content_role(role) {
        return true;
    }
    match Role::from_token(role) {
        Role::Menu | Role::MenuItem => exclusion == ScopeExclusion::ClosedMenus,
        Role::Option | Role::Row | Role::Cell => exclusion == ScopeExclusion::CollapsedLists,
        Role::Unknown => match role_family(role).as_str() {
            "menu" | "menuitem" => exclusion == ScopeExclusion::ClosedMenus,
            "row" | "cell" | "option" => exclusion == ScopeExclusion::CollapsedLists,
            _ => false,
        },
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Completeness {
    Complete,
    Unasserted,
    Incomplete(Vec<String>),
}

impl Read {
    fn incomplete_for(&self, spec: &ElementSpec) -> Option<Vec<String>> {
        let mut details = match &self.completeness {
            Completeness::Incomplete(reasons) => reasons.clone(),
            _ => Vec::new(),
        };
        for exclusion in &self.scope_exclusions {
            if exclusion_guards(*exclusion, spec) {
                let detail = match exclusion {
                    ScopeExclusion::ClosedMenus => "closed_menus_not_expanded",
                    ScopeExclusion::CollapsedLists => "collapsed_lists_not_expanded",
                    ScopeExclusion::Unknown => "unknown_scope_exclusion",
                };
                if !details.iter().any(|item| item == detail) {
                    details.push(detail.to_owned());
                }
            }
        }
        if self.folded_children_may_match(spec) {
            details.push("folded_children".to_owned());
        }
        if details.is_empty() {
            None
        } else {
            Some(details)
        }
    }

    fn folded_children_may_match(&self, spec: &ElementSpec) -> bool {
        let Some(wanted_role) = spec.role.as_deref() else {
            return false;
        };
        if !crate::snapshot_compaction::absorbed_role_tokens()
            .any(|role| role_matches(wanted_role, role))
        {
            return false;
        }
        let text_matches_spec = |text: &str| {
            let text = fold(text);
            spec.name
                .as_ref()
                .is_some_and(|name| has_word_start(&text, &fold(name)))
                || spec
                    .text
                    .as_ref()
                    .is_some_and(|needle| text.contains(&fold(needle)))
        };
        if self.elements.iter().any(|element| {
            if !has_folded_children_boundary(element) {
                return false;
            }
            if spec.name.is_none() && spec.text.is_none() {
                return true;
            }
            if element_role_matches(wanted_role, element) {
                return false;
            }
            let fields = [
                element.get("label").and_then(Value::as_str),
                element.get("value").and_then(Value::as_str),
            ];
            if fields.iter().flatten().any(|text| text_matches_spec(text)) {
                return true;
            }
            if fields.iter().flatten().any(|text| !text.trim().is_empty()) {
                return false;
            }
            // If the structured row has no text, a corresponding marked
            // markdown row can still establish what it displayed.
            let represented = element
                .get("element_index")
                .and_then(Value::as_u64)
                .is_some_and(|index| {
                    let prefix = format!("- [{index}] ");
                    self.markdown.lines().map(str::trim).any(|line| {
                        line.starts_with(&prefix) && folded_children_row(line).is_some()
                    })
                });
            !represented
        }) {
            return true;
        }
        self.markdown
            .lines()
            .filter_map(|line| {
                let row = folded_children_row(line)?;
                // Both fields together supply the structured row's label and
                // folded value; otherwise markdown remains necessary evidence.
                let complete_structured = markdown_row_index(line).is_some_and(|index| {
                    self.elements.iter().any(|element| {
                        element.get("element_index").and_then(Value::as_u64) == Some(index)
                            && has_folded_children_boundary(element)
                            && ["label", "value"].into_iter().all(|key| {
                                element
                                    .get(key)
                                    .and_then(Value::as_str)
                                    .is_some_and(|text| !text.trim().is_empty())
                            })
                    })
                });
                (!complete_structured).then_some(row)
            })
            .any(|(role, text)| {
                if spec.name.is_none() && spec.text.is_none() {
                    return true;
                }
                !role_matches(wanted_role, role) && text_matches_spec(text)
            })
    }

    fn incomplete_miss(
        &self,
        spec: &ElementSpec,
        details: &[String],
        candidates: &[&Value],
    ) -> Miss {
        let listed = candidates
            .iter()
            .take(CANDIDATES_SHOWN)
            .map(|e| describe_element(e))
            .collect::<Vec<_>>()
            .join("; ");
        Miss::new("incomplete_observation", format!(
            "cannot establish {} in {}: {}; observed candidates: {}; target by element_token or narrow the window/app",
            spec.describe(), self.window.describe(), details.join(", "), listed
        )).with_reasons(details.to_vec()).in_window(&self.window)
    }
}

fn has_folded_children_boundary(element: &Value) -> bool {
    element
        .get("boundary")
        .and_then(|boundary| boundary.get("reasons"))
        .and_then(Value::as_array)
        .is_some_and(|reasons| {
            reasons.iter().any(|reason| {
                reason.get("kind").and_then(Value::as_str) == Some("policy_omission")
                    && reason.get("detail").and_then(Value::as_str) == Some("folded_children")
            })
        })
}

fn markdown_row_index(line: &str) -> Option<u64> {
    line.trim()
        .strip_prefix("- [")?
        .split_once(']')?
        .0
        .parse()
        .ok()
}

/// Identify a folded row and exclude its rendered boundary metadata.
fn strip_folded_children_marker(line: &str) -> Option<&str> {
    let (before_kinds, kinds) = line.trim().rsplit_once(" [reason kinds: ")?;
    if !kinds
        .strip_suffix(']')?
        .split(", ")
        .any(|kind| kind == "folded_children=policy_omission")
    {
        return None;
    }
    let boundary = before_kinds
        .rfind(" [more: ")
        .max(before_kinds.rfind(" [+"))?;
    let marker = before_kinds[boundary + 1..].strip_suffix(']')?;
    let rest = if let Some(rest) = marker.strip_prefix("[more: unknown") {
        rest
    } else {
        let (count, rest) = marker.strip_prefix("[+")?.split_once(" more")?;
        if count.is_empty() || !count.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        rest
    };
    let (reason_part, descendant_part) = rest
        .split_once(';')
        .map_or((rest, None), |(reasons, descendants)| {
            (reasons, Some(descendants))
        });
    if !matches!(
        descendant_part,
        None | Some(" descendants omitted") | Some(" descendants not omitted")
    ) {
        return None;
    }
    let reasons = reason_part.strip_prefix(": ")?;
    if !reasons
        .split(", ")
        .any(|reason| reason == "folded_children")
        || !reasons.split(", ").all(|reason| {
            !reason.is_empty()
                && reason
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
    {
        return None;
    }
    Some(&before_kinds[..boundary])
}

/// Return the role and actual row text, excluding the boundary marker.
fn folded_children_row(line: &str) -> Option<(&str, &str)> {
    let mut row = strip_folded_children_marker(line)?.strip_prefix("- ")?;
    if let Some(indexed) = row.strip_prefix('[') {
        let (index, rest) = indexed.split_once(']')?;
        index.parse::<u64>().ok()?;
        row = rest.trim_start();
    }
    let role_end = row.find(char::is_whitespace).unwrap_or(row.len());
    let (role, text) = row.split_at(role_end);
    Some((role, text.trim_start()))
}

fn completeness(state: &Value) -> Completeness {
    use cua_driver_contract::{IncompleteReason, IncompleteReasonDetail, IncompleteReasonKind};

    let mut malformed = false;
    let flag = match state.get("observation_complete") {
        None => None,
        Some(Value::Bool(flag)) => Some(*flag),
        Some(_) => {
            malformed = true;
            None
        }
    };
    let mut reasons = Vec::new();
    if let Some(value) = state.get("observation_incomplete_reasons") {
        if let Some(items) = value.as_array() {
            for item in items {
                match serde_json::from_value::<IncompleteReason>(item.clone()) {
                    Ok(reason) => reasons.push(reason),
                    Err(_) => malformed = true,
                }
            }
        } else {
            malformed = true;
        }
    }
    let substantive = reasons.iter().any(|reason| {
        reason.kind != IncompleteReasonKind::Unknown
            || reason.detail != IncompleteReasonDetail::NotAsserted
    });
    let inconsistent = !substantive
        && ((flag == Some(true) && !reasons.is_empty())
            || (flag == Some(false) && reasons.is_empty()));
    if malformed || substantive || inconsistent {
        let mut details = reasons
            .iter()
            .map(|reason| {
                let item = serde_json::to_value(reason).expect("typed reason serializes");
                format!(
                    "{}/{}",
                    item["kind"]
                        .as_str()
                        .expect("typed reason kind is a string"),
                    item["detail"]
                        .as_str()
                        .expect("typed reason detail is a string")
                )
            })
            .collect::<Vec<_>>();
        if malformed {
            details.push("malformed_completeness".to_owned());
        }
        if inconsistent {
            details.push("inconsistent_completeness".to_owned());
        }
        Completeness::Incomplete(details)
    } else if flag == Some(true) {
        Completeness::Complete
    } else {
        Completeness::Unasserted
    }
}

pub(crate) fn malformed_completeness_details(state: &Value) -> Option<Vec<String>> {
    let mut details = match completeness(state) {
        Completeness::Incomplete(details)
            if details
                .iter()
                .any(|detail| detail == "malformed_completeness") =>
        {
            details
        }
        _ => Vec::new(),
    };
    if scope_exclusions(state).contains(&ScopeExclusion::Unknown) {
        details.push("unknown_scope_exclusion".to_owned());
    }
    (!details.is_empty()).then_some(details)
}

/// An element found by description.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub window: Window,
    pub element: Value,
}

impl Found {
    pub fn token(&self) -> Option<&str> {
        self.element.get("element_token").and_then(Value::as_str)
    }

    pub fn describe(&self) -> String {
        describe_element(&self.element)
    }
}

/// Why a lookup or check did not succeed.
#[derive(Clone, Debug)]
pub(crate) struct Miss {
    /// Stable machine-readable reason.
    pub code: &'static str,
    pub message: String,
    pub window: Option<Window>,
    safe_reasons: Vec<String>,
}

impl Miss {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            window: None,
            safe_reasons: Vec::new(),
        }
    }

    /// Whether polling again can change the answer. A refused or failed
    /// read, or an ambiguous match, will not fix itself, and retrying a
    /// refused read would only repeat the refusal.
    fn worth_waiting(&self) -> bool {
        matches!(
            self.code,
            "not_found"
                | "app_not_found"
                | "window_not_found"
                | "still_present"
                | "gone_unconfirmed"
                | "unexpected_state"
                | "incomplete_observation"
        )
    }

    fn in_window(mut self, window: &Window) -> Self {
        self.window = Some(window.clone());
        self
    }

    fn with_reasons(mut self, reasons: Vec<String>) -> Self {
        self.safe_reasons = reasons;
        self
    }

    fn late<T>(operation: &str, result: &Result<T, Miss>) -> Self {
        let mut message = format!("{operation} completed after its deadline");
        if let Err(miss) = result {
            message.push_str(&format!("; last miss: {}", miss.code));
            if !miss.safe_reasons.is_empty() {
                message.push_str(&format!("; reasons: {}", miss.safe_reasons.join(", ")));
            }
        }
        Self::new("timeout", message)
    }
}

enum CallError {
    Deadline,
    MalformedCompleteness(Vec<String>),
    Failed(String),
}

impl CallError {
    fn into_miss(self, code: &'static str) -> Miss {
        match self {
            Self::Deadline => Miss::new("timeout", "locator deadline expired"),
            Self::MalformedCompleteness(details) => Miss::new(
                "incomplete_observation",
                format!("incomplete observation: {}", details.join(", ")),
            )
            .with_reasons(details),
            Self::Failed(message) => Miss::new(code, message),
        }
    }
}

/// Outcome of a satisfied check.
#[derive(Clone, Debug)]
pub(crate) struct Checked {
    pub window: Option<Window>,
    pub observed: Option<String>,
    pub waited: Duration,
}

/// Window and element lookup through the registry.
pub(crate) struct Locator<'a> {
    registry: LocatorRegistry<'a>,
    session: Option<String>,
    /// Hard stop for every poll loop (the batch's own time budget).
    clock: Arc<dyn crate::actionability::Clock + Send + Sync>,
    clock_deadline: Duration,
    /// When the batch ends with a default `observe`, the `max_elements` of
    /// that read: the first lookup of each window also makes one plain
    /// read with that budget, which `since:"latest"` remembers, so the end
    /// observation is a diff of what the batch changed rather than a full
    /// read of a window the caller never read.
    baseline_max_elements: Option<u64>,
    baselined: std::sync::Mutex<std::collections::HashSet<(i64, u64)>>,
    /// The code of the last read a batch scope refused, so a failed lookup
    /// reports `outside_scope` / `foreground_not_allowed` rather than a
    /// plain lookup failure.
    scope_refusal: std::sync::Mutex<Option<&'static str>>,
    /// Every driver call made through this locator since the last
    /// [`Self::take_journal`], in order: {tool, pid, window_id, ok, effect, code}.
    journal: std::sync::Mutex<Vec<Value>>,
    read_admission: Option<ReadAdmission>,
    read_outcome: Option<ReadOutcome>,
    read_closed: Option<ReadClosed>,
}

enum LocatorRegistry<'a> {
    Borrowed(&'a ToolRegistry),
    Owned(Arc<ToolRegistry>),
}

pub(crate) type ReadAdmission = Arc<dyn Fn() -> bool + Send + Sync>;
pub(crate) type ReadOutcome = Arc<dyn Fn(Option<&'static str>) + Send + Sync>;
pub(crate) type ReadClosed = Arc<dyn Fn() -> bool + Send + Sync>;

impl<'a> Locator<'a> {
    #[cfg(test)]
    pub(crate) fn with_clock(
        mut self,
        clock: Arc<dyn crate::actionability::Clock + Send + Sync>,
        remaining: Duration,
    ) -> Self {
        self.clock_deadline = clock.now().saturating_add(remaining);
        self.clock = clock;
        self
    }

    pub fn new(registry: &'a ToolRegistry, session: Option<String>, deadline: Instant) -> Self {
        let (clock, clock_deadline) = locator_clock(deadline);
        Self {
            registry: LocatorRegistry::Borrowed(registry),
            session,
            clock,
            clock_deadline,
            baseline_max_elements: None,
            baselined: Default::default(),
            scope_refusal: Default::default(),
            journal: Default::default(),
            read_admission: None,
            read_outcome: None,
            read_closed: None,
        }
    }

    pub fn new_owned(
        registry: Arc<ToolRegistry>,
        session: Option<String>,
        deadline: Instant,
        read_admission: Option<ReadAdmission>,
        read_outcome: Option<ReadOutcome>,
        read_closed: Option<ReadClosed>,
    ) -> Locator<'static> {
        let (clock, clock_deadline) = locator_clock(deadline);
        Locator {
            registry: LocatorRegistry::Owned(registry),
            session,
            clock,
            clock_deadline,
            baseline_max_elements: None,
            baselined: Default::default(),
            scope_refusal: Default::default(),
            journal: Default::default(),
            read_admission,
            read_outcome,
            read_closed,
        }
    }

    fn registry(&self) -> &ToolRegistry {
        match &self.registry {
            LocatorRegistry::Borrowed(registry) => registry,
            LocatorRegistry::Owned(registry) => registry.as_ref(),
        }
    }

    fn read_admission_closed(&self) -> bool {
        self.read_closed
            .as_ref()
            .or(self.read_admission.as_ref())
            .is_some_and(|closed| closed())
    }

    /// Add a driver call made outside the locator (a script's own action or
    /// read) to the journal, so it stays in dispatch order.
    pub fn record(&self, entry: Value) {
        self.journal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry);
    }

    /// The driver calls recorded since the last call.
    pub fn take_journal(&self) -> Vec<Value> {
        std::mem::take(
            &mut *self
                .journal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// The scope refusal code a lookup hit since the last call, if any.
    pub fn take_scope_refusal(&self) -> Option<&'static str> {
        self.scope_refusal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// The scope refusal code a lookup hit since the last take, left in place.
    pub fn scope_refusal(&self) -> Option<&'static str> {
        *self
            .scope_refusal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn with_observe_baseline(mut self, max_elements: u64) -> Self {
        self.baseline_max_elements = Some(max_elements);
        self
    }

    fn with_session(&self, mut args: Value) -> Value {
        if let (Some(session), Some(object)) = (&self.session, args.as_object_mut()) {
            object.insert("session".to_owned(), Value::String(session.clone()));
        }
        args
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, CallError> {
        let deadline = LOCATOR_READ_DEADLINE
            .try_with(|deadline| *deadline)
            .unwrap_or(self.clock_deadline);
        if self.clock.now() >= deadline {
            return Err(CallError::Deadline);
        }
        // Once a terminal refusal occurred in this step or script call,
        // the locator sends nothing more (a discarded baseline read must not
        // lead to a second attempt).
        if let Some(code) = self.scope_refusal() {
            return Err(CallError::Failed(format!(
                "{tool}: not sent, because an earlier read was refused ({code})"
            )));
        }
        if self.read_admission_closed() {
            return Err(CallError::Failed(format!("{tool}: read admission closed")));
        }
        if self.read_admission.as_ref().is_some_and(|admit| !admit()) {
            return Err(CallError::Failed(format!("{tool}: read admission closed")));
        }
        let (pid, window_id) = call_target(&args);
        let result = self.registry().invoke(tool, self.with_session(args)).await;
        let scope_code = result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("code").or_else(|| value.pointer("/refusal/code")))
            .and_then(Value::as_str)
            .and_then(|code| {
                [
                    crate::batch_scope::OUTSIDE_SCOPE,
                    crate::batch_scope::FOREGROUND_NOT_ALLOWED,
                    "process_instance_mismatch",
                    "process_instance_unverified",
                ]
                .into_iter()
                .find(|known| *known == code)
            });
        if let Some(outcome) = &self.read_outcome {
            outcome(scope_code);
        }
        self.record(journal_entry(tool, pid, window_id, &result));
        if result.is_error == Some(true) {
            if let Some(scope_code) = scope_code {
                *self
                    .scope_refusal
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(scope_code);
            }
            if let Some(details) = result
                .structured_content
                .as_ref()
                .filter(|content| {
                    tool == "get_window_state"
                        && content.get("code").and_then(Value::as_str)
                            == Some("typed_output_mismatch")
                })
                .and_then(|content| content.get("malformed_completeness_details"))
                .and_then(Value::as_array)
            {
                let mut safe = details
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|detail| {
                        if matches!(
                            *detail,
                            "malformed_completeness"
                                | "inconsistent_completeness"
                                | "unknown_scope_exclusion"
                        ) {
                            return true;
                        }
                        let Some((kind, detail)) = detail.split_once('/') else {
                            return false;
                        };
                        serde_json::from_value::<cua_driver_contract::IncompleteReason>(
                            json!({"kind":kind,"detail":detail}),
                        )
                        .is_ok()
                    })
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if safe.is_empty() {
                    safe.push("malformed_completeness".to_owned());
                }
                return Err(CallError::MalformedCompleteness(safe));
            }
            return Err(CallError::Failed(format!(
                "{tool}: {}",
                first_text(&result)
            )));
        }
        Ok(result.structured_content.unwrap_or(Value::Null))
    }

    /// Resolve a spec to one window. With only `app` or `pid`, the app's
    /// frontmost window wins, so a dialog that opens on top is found.
    pub async fn window(&self, spec: &WindowSpec) -> Result<Window, Miss> {
        if let Some((pid, window_id)) = spec.exact() {
            return Ok(Window {
                pid,
                window_id,
                app: None,
                title: None,
                on_screen: true,
            });
        }
        self.listed_window(spec).await
    }

    /// Like [`Self::window`], but always confirms the window through
    /// `list_windows`, so the result carries its app name and title.
    pub async fn listed_window(&self, spec: &WindowSpec) -> Result<Window, Miss> {
        let mut windows = self.listed_windows(spec).await?;
        Ok(windows.remove(0))
    }

    /// Every window matching `spec`, best first: on-screen before
    /// off-screen, then topmost (largest z_index) first. Never empty.
    pub async fn listed_windows(&self, spec: &WindowSpec) -> Result<Vec<Window>, Miss> {
        if spec.is_empty() {
            return Err(Miss::new(
                "no_window",
                "no window to look in: give `app` (or pid and window_id) on this step or an earlier one",
            ));
        }
        let mut list_args = json!({"on_screen_only": false});
        if let Some(pid) = spec.pid {
            list_args["pid"] = json!(pid);
        }
        let listed = self
            .call("list_windows", list_args)
            .await
            .map_err(|error| error.into_miss("lookup_failed"))?;
        let windows: Vec<Value> = listed
            .get("windows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let pids = match &spec.app {
            Some(app)
                if !windows.iter().any(|window| {
                    window.get("pid").and_then(Value::as_i64).is_some()
                        && text_matches(window.get("app_name"), app)
                }) =>
            {
                self.app_pids(app).await?
            }
            _ => Vec::new(),
        };
        filter_windows(&windows, spec, &pids)
    }

    /// The windows a lookup searches, best first. An exact pid/window_id is
    /// the only one; otherwise the app's on-screen windows in stacking
    /// order, so an element in a dialog on top is found first and one in
    /// the main window is still found when a transient popup is on top.
    pub async fn search_windows(&self, spec: &WindowSpec) -> Result<Vec<Window>, Miss> {
        if spec.exact().is_some() {
            return Ok(vec![self.window(spec).await?]);
        }
        let windows = self.listed_windows(spec).await?;
        let on_screen: Vec<Window> = windows.iter().filter(|w| w.on_screen).cloned().collect();
        let mut pool = if on_screen.is_empty() {
            windows
        } else {
            on_screen
        };
        pool.truncate(MAX_WINDOWS_SEARCHED);
        Ok(pool)
    }

    /// Pids of running apps whose name or bundle id matches.
    async fn app_pids(&self, app: &str) -> Result<Vec<i64>, Miss> {
        let listed = self
            .call("list_apps", json!({}))
            .await
            .map_err(|error| error.into_miss("lookup_failed"))?;
        Ok(listed
            .get("apps")
            .and_then(Value::as_array)
            .map(|apps| {
                apps.iter()
                    .filter(|entry| {
                        entry
                            .get("bundle_id")
                            .and_then(Value::as_str)
                            .is_some_and(|bundle| bundle.eq_ignore_ascii_case(app))
                            || text_matches(entry.get("name"), app)
                    })
                    .filter_map(|entry| entry.get("pid").and_then(Value::as_i64))
                    .filter(|pid| *pid > 0)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// One fresh accessibility read. It refreshes the window's element
    /// tokens, so the tokens it returns are the ones to act with.
    pub async fn read(&self, window: &Window) -> Result<Read, Miss> {
        self.read_view(window, false).await
    }

    /// A read of the window as a model sees it by default when `visible` is
    /// set: menus collapsed and off-screen dense rows left out, about a
    /// third of a full read (Calculator 90 against 210 ms, a GitHub page in
    /// Aside 55 against 205 ms). See [`VISIBLE_READ_MAX_ELEMENTS`] for how it
    /// stays out of the batch's `observe` diff.
    async fn read_view(&self, window: &Window, visible: bool) -> Result<Read, Miss> {
        if let Some(max_elements) = self.baseline_max_elements {
            let first = self
                .baselined
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((window.pid, window.window_id));
            if first {
                // Best effort: a failed baseline only means a full end read.
                let _ = self
                    .call(
                        "get_window_state",
                        json!({
                            "pid": window.pid,
                            "window_id": window.window_id,
                            "max_elements": max_elements,
                            "include_screenshot": false,
                        }),
                    )
                    .await;
            }
        }
        let args = if visible {
            json!({
                "pid": window.pid,
                "window_id": window.window_id,
                "tree_format": "both",
                "max_elements": VISIBLE_READ_MAX_ELEMENTS,
                "include_screenshot": false,
            })
        } else {
            json!({
                "pid": window.pid,
                "window_id": window.window_id,
                "full_output": true,
                "include_screenshot": false,
            })
        };
        let state = self
            .call("get_window_state", args)
            .await
            .map_err(|error| error.into_miss("read_failed").in_window(window))?;
        let elements = state
            .get("elements")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let markdown = state
            .get("tree_markdown")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok(Read {
            window: window.clone(),
            elements,
            markdown,
            completeness: completeness(&state),
            scope_exclusions: scope_exclusions(&state),
        })
    }

    /// Find exactly one element, waiting up to `timeout` for it to appear.
    pub async fn find(
        &self,
        window: &WindowSpec,
        element: &ElementSpec,
        timeout: Duration,
    ) -> Result<Found, Miss> {
        let started = self.clock.now();
        let deadline = started.saturating_add(timeout).min(self.clock_deadline);
        if started >= self.clock_deadline {
            return Err(Miss::new("timeout", "enclosing deadline expired"));
        }
        loop {
            let read_deadline = if timeout.is_zero() {
                self.clock_deadline
            } else {
                deadline
            };
            let attempt = LOCATOR_READ_DEADLINE
                .scope(read_deadline, self.find_once(window, element))
                .await;
            if self.read_admission_closed() {
                return Err(Miss::new("step_timeout", "read admission closed"));
            }
            let now = self.clock.now();
            if now > self.clock_deadline || (!timeout.is_zero() && now > deadline) {
                return Err(Miss::late("lookup", &attempt));
            }
            match attempt {
                Ok(found) => return Ok(found),
                Err(miss) if !miss.worth_waiting() => return Err(miss),
                Err(miss) => {
                    if timeout.is_zero() || now >= deadline {
                        let waited = now.saturating_sub(started).as_millis();
                        let mut miss = miss;
                        if waited > 0 {
                            miss.message = format!("{} (waited {waited} ms)", miss.message);
                        }
                        return Err(miss);
                    }
                    if self.read_admission_closed() {
                        return Err(Miss::new("step_timeout", "read admission closed"));
                    }
                    self.clock
                        .sleep(POLL_INTERVAL.min(deadline.saturating_sub(now)))
                        .await;
                    if self.clock.now() >= deadline {
                        let waited = self.clock.now().saturating_sub(started).as_millis();
                        let mut miss = miss;
                        if waited > 0 {
                            miss.message = format!("{} (waited {waited} ms)", miss.message);
                        }
                        return Err(miss);
                    }
                }
            }
        }
    }

    /// One pass over the windows `spec` names: the first window holding a
    /// unique match wins. When none does, the miss comes from the window
    /// with the most elements, whose nearest matches are the useful ones.
    async fn find_once(&self, spec: &WindowSpec, element: &ElementSpec) -> Result<Found, Miss> {
        let windows = self.search_windows(spec).await?;
        let mut best_miss: Option<(usize, Miss)> = None;
        for window in &windows {
            // A control named by role is first looked up in the visible
            // window; only when that has no unique match does the full read
            // (every menu, every row) decide, as before.
            if element.visible_lookup_suffices()
                && self.baseline_max_elements != Some(VISIBLE_READ_MAX_ELEMENTS)
            {
                if let Ok(read) = self.read_view(window, true).await {
                    // Only an exact name stands for the full read: a
                    // substring match could hide an exact one off screen.
                    if let Some(found) = pick(&read, element)
                        .ok()
                        .filter(|found| element.names_exactly(found))
                    {
                        return Ok(found);
                    }
                }
            }
            let read = self.read(window).await?;
            match pick(&read, element) {
                Ok(found) => return Ok(found),
                Err(miss) if miss.code == "ambiguous" || miss.code == "not_addressable" => {
                    return Err(miss.in_window(window))
                }
                Err(miss) => {
                    let size = read.elements.len();
                    if best_miss.as_ref().is_none_or(|(best, previous)| {
                        (miss.code == "incomplete_observation"
                            && previous.code != "incomplete_observation")
                            || ((miss.code == "incomplete_observation")
                                == (previous.code == "incomplete_observation")
                                && size > *best)
                    }) {
                        best_miss = Some((size, miss.in_window(window)));
                    }
                }
            }
        }
        let (_, mut miss) = best_miss.expect("search_windows is never empty");
        if windows.len() > 1 {
            miss.message = format!(
                "{} (searched {} windows of the app)",
                miss.message,
                windows.len()
            );
        }
        Err(miss)
    }

    /// Classify closure from the same complete listings that supply candidates.
    ///
    /// Only a pid-scoped listing can prove a window closed: it must report a
    /// complete enumeration and a readable authoritative id set. A pid-less
    /// listing (macOS: layer 0 only, no id set) proves nothing. Without a
    /// window_id the spec matches by app or title, which a window left out
    /// of `windows` (an AX-less or helper window) cannot be checked against,
    /// so any authoritative id missing from `windows` leaves closure unproven.
    async fn closure_listing(&self, spec: &WindowSpec) -> Result<Closure, Miss> {
        let mut resolved_app_pids = None;
        let pids = match (spec.pid, &spec.app) {
            (Some(pid), _) => vec![pid],
            (None, Some(app)) => {
                let pids = self.app_pids(app).await?;
                if pids.is_empty() {
                    return Ok(Closure::Closed);
                }
                let targets = pids.clone();
                resolved_app_pids = Some(pids);
                targets
            }
            (None, None) => return Ok(Closure::Incomplete),
        };
        let mut windows = Vec::new();
        let mut hidden = false;
        for pid in pids {
            let listed = self
                .call("list_windows", json!({"pid": pid, "on_screen_only": false}))
                .await
                .map_err(|error| error.into_miss("lookup_failed"))?;
            if listed.get("enumeration_complete").and_then(Value::as_bool) != Some(true) {
                return Ok(Closure::Incomplete);
            }
            let Some(ids) = listed
                .get("authoritative_window_ids")
                .and_then(Value::as_array)
                .and_then(|ids| ids.iter().map(Value::as_u64).collect::<Option<Vec<_>>>())
            else {
                return Ok(Closure::Incomplete);
            };
            let Some(entries) = listed.get("windows").and_then(Value::as_array) else {
                return Ok(Closure::Incomplete);
            };
            hidden |= match spec.window_id {
                Some(window_id) => {
                    ids.contains(&window_id)
                        && !entries.iter().any(|entry| {
                            entry.get("window_id").and_then(Value::as_u64) == Some(window_id)
                        })
                }
                None => ids.iter().any(|id| {
                    !entries
                        .iter()
                        .any(|entry| entry.get("window_id").and_then(Value::as_u64) == Some(*id))
                }),
            };
            windows.extend(entries.iter().cloned());
        }
        if hidden {
            return Ok(Closure::HiddenWindow);
        }
        let pids = match (&spec.app, resolved_app_pids) {
            (_, Some(pids)) => pids,
            (Some(app), None)
                if !windows.iter().any(|window| {
                    window.get("pid").and_then(Value::as_i64).is_some()
                        && text_matches(window.get("app_name"), app)
                }) =>
            {
                self.app_pids(app).await?
            }
            _ => Vec::new(),
        };
        match filter_windows(&windows, spec, &pids) {
            Ok(windows) => Ok(Closure::Present(windows)),
            Err(_) => Ok(Closure::Closed),
        }
    }

    /// One pass of a check over the windows it names. A presence check
    /// holds when any window satisfies it; `gone` holds when none shows it.
    async fn check_once(
        &self,
        check: &Check,
        spec: &WindowSpec,
        started: Duration,
    ) -> Result<Checked, Miss> {
        let windows = if check.gone && spec.exact().is_none() {
            match self.closure_listing(spec).await? {
                    Closure::Present(windows) => windows,
                    Closure::Closed => return Ok(Checked {
                        window: None,
                        observed: Some(format!("{} is closed", spec.describe())),
                        waited: self.clock.now().saturating_sub(started),
                    }),
                    Closure::Incomplete => return Err(Miss::new(
                        "gone_unconfirmed",
                        format!("no window for {} is listed, but the window listing is incomplete, so its absence is unproven", spec.describe()),
                    )),
                    Closure::HiddenWindow => return Err(Miss::new(
                        "gone_unconfirmed",
                        format!("{} is absent from the window list but remains in the authoritative window ids", spec.describe()),
                    )),
                }
        } else {
            self.search_windows(spec).await?
        };
        let mut best_miss: Option<(usize, Miss)> = None;
        for window in &windows {
            let read = match self.read(window).await {
                Ok(read) => read,
                Err(miss) if miss.code == "timeout" => return Err(miss),
                Err(miss) if miss.code == "incomplete_observation" => return Err(miss),
                // A read the batch scope refused proves nothing about the
                // window, least of all that something is gone: fail now.
                Err(miss) if self.scope_refusal().is_some() => {
                    return Err(Miss::new(
                        self.scope_refusal()
                            .unwrap_or(crate::batch_scope::OUTSIDE_SCOPE),
                        miss.message,
                    ))
                }
                Err(miss) if check.gone => {
                    let listed = self
                        .call(
                            "list_windows",
                            json!({
                                "pid": window.pid,
                                "on_screen_only": false,
                            }),
                        )
                        .await
                        .map_err(|error| error.into_miss("lookup_failed"))?;
                    // The window list leaves out live helper and AX-less
                    // windows, so only the complete authoritative id set
                    // for the pid can prove the window closed.
                    if window_closed(&listed, window.window_id) {
                        continue;
                    }
                    return Err(Miss::new(
                        "gone_unconfirmed",
                        format!(
                            "{} could not be read and the window listing does not prove it closed: {}",
                            window.describe(),
                            miss.message
                        ),
                    )
                    .in_window(window));
                }
                Err(miss) => return Err(miss),
            };
            match evaluate(check, &read) {
                Ok(observed) if !check.gone => {
                    return Ok(Checked {
                        window: Some(window.clone()),
                        observed,
                        waited: self.clock.now().saturating_sub(started),
                    })
                }
                Ok(_) => {}
                Err(miss) if check.gone => return Err(miss),
                Err(miss) => {
                    let size = read.elements.len();
                    if best_miss.as_ref().is_none_or(|(best, previous)| {
                        (miss.code == "incomplete_observation"
                            && previous.code != "incomplete_observation")
                            || ((miss.code == "incomplete_observation")
                                == (previous.code == "incomplete_observation")
                                && size > *best)
                    }) {
                        best_miss = Some((size, miss));
                    }
                }
            }
        }
        match best_miss {
            None => Ok(Checked {
                window: windows.first().cloned(),
                observed: None,
                waited: self.clock.now().saturating_sub(started),
            }),
            Some((_, mut miss)) => {
                if windows.len() > 1 {
                    miss.message = format!(
                        "{} (searched {} windows of the app)",
                        miss.message,
                        windows.len()
                    );
                }
                Err(miss)
            }
        }
    }

    /// Poll until the check holds or its timeout passes.
    pub async fn check(&self, check: &Check, fallback: &WindowSpec) -> Result<Checked, Miss> {
        let window_spec = if check.window.is_empty() {
            fallback.clone()
        } else {
            check.window.clone()
        };
        let started = self.clock.now();
        let deadline = started
            .saturating_add(check.timeout)
            .min(self.clock_deadline);
        if started >= self.clock_deadline {
            return Err(Miss::new("timeout", "enclosing deadline expired"));
        }
        loop {
            let read_deadline = if check.timeout.is_zero() {
                self.clock_deadline
            } else {
                deadline
            };
            let outcome = LOCATOR_READ_DEADLINE
                .scope(read_deadline, self.check_once(check, &window_spec, started))
                .await;
            if self.read_admission_closed() {
                return Err(Miss::new("step_timeout", "read admission closed"));
            }
            let now = self.clock.now();
            if now > self.clock_deadline || (!check.timeout.is_zero() && now > deadline) {
                return Err(Miss::late("check", &outcome));
            }
            match outcome {
                Ok(checked) => return Ok(checked),
                Err(miss) if !miss.worth_waiting() => return Err(miss),
                Err(miss) => {
                    if check.timeout.is_zero() || now >= deadline {
                        let mut miss = miss;
                        miss.message = format!(
                            "{} (checked for {} ms)",
                            miss.message,
                            now.saturating_sub(started).as_millis()
                        );
                        return Err(miss);
                    }
                    if self.read_admission_closed() {
                        return Err(Miss::new("step_timeout", "read admission closed"));
                    }
                    self.clock
                        .sleep(POLL_INTERVAL.min(deadline.saturating_sub(now)))
                        .await;
                    if self.clock.now() >= deadline {
                        let mut miss = miss;
                        miss.message = format!(
                            "{} (checked for {} ms)",
                            miss.message,
                            self.clock.now().saturating_sub(started).as_millis()
                        );
                        return Err(miss);
                    }
                }
            }
        }
    }
}

/// Elements of a read that match `spec`. Exact name matches win over
/// substring matches; zero-size elements only count when nothing visible
/// matches.
pub(crate) fn matches<'r>(read: &'r Read, spec: &ElementSpec) -> Vec<&'r Value> {
    let by_role: Vec<&Value> = read
        .elements
        .iter()
        .filter(|element| element.get("display_only").and_then(Value::as_bool) != Some(true))
        .filter(|element| {
            spec.role
                .as_deref()
                .is_none_or(|role| element_role_matches(role, element))
        })
        .filter(|element| {
            spec.text.as_deref().is_none_or(|text| {
                [
                    "label",
                    "value",
                    "description",
                    "help",
                    "identifier",
                    "role",
                ]
                .into_iter()
                .any(|key| text_matches(element.get(key), text))
                    || element
                        .get("element_index")
                        .and_then(Value::as_u64)
                        .is_some_and(|index| {
                            let prefix = format!("- [{index}] ");
                            read.markdown
                                .lines()
                                .map(str::trim)
                                .any(|row| row.starts_with(&prefix) && row_matches(row, spec))
                        })
            })
        })
        .collect();
    let named: Vec<&Value> = match &spec.name {
        None => by_role,
        Some(name) => {
            let needle = fold(name);
            let exact: Vec<&Value> = by_role
                .iter()
                .copied()
                .filter(|element| label_of(element).is_some_and(|label| fold(label) == needle))
                .collect();
            if exact.is_empty() {
                by_role
                    .into_iter()
                    .filter(|element| {
                        label_of(element).is_some_and(|label| has_word_start(&fold(label), &needle))
                    })
                    .collect()
            } else {
                exact
            }
        }
    };
    let visible: Vec<&Value> = named
        .iter()
        .copied()
        .filter(|element| has_area(element))
        .collect();
    if visible.is_empty() {
        named
    } else {
        visible
    }
}

/// Lowercase role without the platform prefix: "AXTextField" -> "textfield".
fn bare_role(role: &str) -> String {
    let folded: String = role
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(|c| c.to_lowercase())
        .collect();
    folded
        .strip_prefix("ax")
        .map(str::to_owned)
        .unwrap_or(folded)
}

/// Narrow several matches to the ones the caller most plausibly meant: those
/// whose role is exactly the one named ("textfield" over a matching
/// AXTextArea), then, with no role named, those the acting tool can use (a
/// field for set_value rather than its label, a control for click rather
/// than static text). Returns the input unchanged when no rule narrows it.
fn narrow<'r>(found: Vec<&'r Value>, spec: &ElementSpec) -> Vec<&'r Value> {
    if found.len() < 2 || spec.nth.is_some() {
        return found;
    }
    let role_of = |element: &Value| {
        element
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let keep = |found: &Vec<&'r Value>, rule: &dyn Fn(&Value) -> bool| -> Option<Vec<&'r Value>> {
        let kept: Vec<&Value> = found.iter().copied().filter(|e| rule(e)).collect();
        (!kept.is_empty() && kept.len() < found.len()).then_some(kept)
    };
    let mut found = found;
    if let Some(role) = &spec.role {
        let wanted = bare_role(role);
        if let Some(kept) = keep(&found, &|e| bare_role(&role_of(e)) == wanted) {
            found = kept;
        }
    } else if let Some(action) = spec.action {
        let usable: &[&str] = match action {
            "set_value" | "type_text" => &["textfield", "popupbutton", "checkbox", "slider"],
            _ => &[],
        };
        let rule: Box<dyn Fn(&Value) -> bool> = if usable.is_empty() {
            Box::new(|e: &Value| {
                !matches!(
                    role_family(&role_of(e)).as_str(),
                    "statictext" | "image" | "group"
                )
            })
        } else {
            Box::new(move |e: &Value| usable.contains(&role_family(&role_of(e)).as_str()))
        };
        if let Some(kept) = keep(&found, &*rule) {
            found = kept;
        }
    }
    found
}

fn pick(read: &Read, spec: &ElementSpec) -> Result<Found, Miss> {
    let found = narrow(matches(read, spec), spec);
    if let (true, Some(details)) = (
        spec.nth.is_some() || found.len() < 2,
        read.incomplete_for(spec),
    ) {
        return Err(read.incomplete_miss(spec, &details, &found));
    }
    let chosen = match (found.len(), spec.nth) {
        (0, _) => {
            // The tree may show the element as a row without an index: it
            // exposes no AX actions (a LibreOffice sheet tab, for one), so no
            // name lookup can act on it. Say so instead of offering
            // look-alikes, so the next call clicks it by pixel.
            if let Some(row) = display_rows(read, spec).first() {
                let text = row.get("text").and_then(Value::as_str).unwrap_or_default();
                return Err(Miss::new(
                    "not_addressable",
                    format!(
                        "{} is in {} as `{}` but has no element index (it exposes no \
                         accessibility actions), so it cannot be targeted by name or token. \
                         Click it by pixel x,y read from the window's screenshot; toolkits \
                         that ignore background clicks also need delivery_mode:\"foreground\"",
                        spec.describe(),
                        read.window.describe(),
                        clip(text, 80)
                    ),
                ));
            }
            return Err(Miss::new(
                "not_found",
                format!(
                    "no {} in {}{}",
                    spec.describe(),
                    read.window.describe(),
                    near_misses(read, spec)
                ),
            ));
        }
        (count, Some(nth)) if nth >= count => {
            return Err(Miss::new(
                "not_found",
                format!(
                    "nth {nth} asked but only {count} {} in {}",
                    spec.describe(),
                    read.window.describe()
                ),
            ))
        }
        (_, Some(nth)) => found[nth],
        (1, None) => found[0],
        (count, None) => {
            let listed: Vec<String> = found
                .iter()
                .take(CANDIDATES_SHOWN)
                .enumerate()
                .map(|(nth, element)| format!("nth {nth}: {}", describe_element(element)))
                .collect();
            return Err(Miss::new(
                "ambiguous",
                format!(
                    "{count} elements match {} in {}; add `nth` or a more exact name: {}",
                    spec.describe(),
                    read.window.describe(),
                    listed.join("; ")
                ),
            ));
        }
    };
    if chosen
        .get("element_token")
        .and_then(Value::as_str)
        .is_none()
    {
        return Err(Miss::new(
            "not_addressable",
            format!(
                "{} in {} has no element_token to act on",
                describe_element(chosen),
                read.window.describe()
            ),
        ));
    }
    Ok(Found {
        window: read.window.clone(),
        element: chosen.clone(),
    })
}

/// `Ok(observed)` when the check holds against this read.
fn evaluate(check: &Check, read: &Read) -> Result<Option<String>, Miss> {
    let element_spec = ElementSpec {
        nth: None,
        ..check.element.clone()
    };
    let structured = if element_spec.role.is_some() || element_spec.name.is_some() {
        matches(read, &element_spec)
    } else {
        Vec::new()
    };
    // Static text and other display-only rows are only in the markdown.
    let rows: Vec<&str> = read
        .markdown
        .lines()
        .map(str::trim)
        .filter(|line| row_matches(line, &check.element))
        .collect();
    let by_text_only = element_spec.role.is_none() && element_spec.name.is_none();
    let present = if by_text_only {
        !rows.is_empty()
    } else {
        !structured.is_empty() || !rows.is_empty()
    };
    let window = &read.window;
    if let (true, Some(details)) = (
        check.element.nth.is_some() || !present,
        read.incomplete_for(&check.element),
    ) {
        return Err(read.incomplete_miss(&check.element, &details, &structured));
    }
    if check.gone {
        return if present {
            let shown = structured
                .first()
                .map(|element| describe_element(element))
                .or_else(|| rows.first().map(|row| clip(row, 120)))
                .unwrap_or_default();
            Err(Miss::new(
                "still_present",
                format!(
                    "{} is still shown in {}: {shown}",
                    check.element.describe(),
                    window.describe()
                ),
            )
            .in_window(window))
        } else {
            Ok(None)
        };
    }
    if !present {
        return Err(Miss::new(
            "not_found",
            format!(
                "no {} in {}{}",
                check.element.describe(),
                window.describe(),
                near_misses(read, &check.element)
            ),
        )
        .in_window(window));
    }
    let needs_fields = check.value.is_some()
        || check.value_contains.is_some()
        || check.enabled.is_some()
        || check.selected.is_some();
    if !needs_fields {
        let observed = structured
            .first()
            .map(|element| describe_element(element))
            .or_else(|| rows.first().map(|row| clip(row, 120)));
        return Ok(observed);
    }
    let pool: Vec<&Value> = match check.element.nth {
        Some(nth) => structured.get(nth).copied().into_iter().collect(),
        None => structured.clone(),
    };
    if let Some(element) = pool.iter().find(|element| fields_hold(check, element)) {
        return Ok(Some(describe_element(element)));
    }
    let display = if check.element.nth.is_none() {
        display_rows(read, &element_spec)
    } else {
        Vec::new()
    };
    if check.element.nth.is_none() {
        if let Some(row) = display.iter().find(|row| fields_hold(check, row)) {
            return Ok(Some(describe_element(row)));
        }
    }
    let unknown = pool.iter().copied().chain(display.iter()).any(|element| {
        ((check.value.is_some() || check.value_contains.is_some())
            && element.get("value").and_then(Value::as_str).is_none())
            || (check.enabled.is_some()
                && element.get("enabled").and_then(Value::as_bool).is_none())
            || (check.selected.is_some() && selected_state(element).is_none())
    });
    let shown = pool
        .iter()
        .take(CANDIDATES_SHOWN)
        .map(|element| describe_element(element))
        .collect::<Vec<_>>()
        .join("; ");
    Err(Miss::new(
        "unexpected_state",
        format!(
            "{} expected in {}, found {}{}",
            check.describe(),
            window.describe(),
            if shown.is_empty() {
                "no addressable match".to_owned()
            } else {
                shown
            },
            if unknown {
                " (state unknown for a requested field)"
            } else {
                ""
            }
        ),
    )
    .in_window(window))
}

enum Closure {
    Incomplete,
    Present(Vec<Window>),
    HiddenWindow,
    Closed,
}

/// Filter and rank window values from any listing using the same selector rules.
fn filter_windows(windows: &[Value], spec: &WindowSpec, pids: &[i64]) -> Result<Vec<Window>, Miss> {
    let mut candidates: Vec<&Value> = windows
        .iter()
        .filter(|window| window.get("pid").and_then(Value::as_i64).is_some())
        .collect();
    if let Some(app) = &spec.app {
        candidates.retain(|window| {
            text_matches(window.get("app_name"), app)
                || window
                    .get("pid")
                    .and_then(Value::as_i64)
                    .is_some_and(|pid| pids.contains(&pid))
        });
        if candidates.is_empty() {
            let mut apps: Vec<&str> = windows
                .iter()
                .filter_map(|window| window.get("app_name").and_then(Value::as_str))
                .collect();
            apps.sort_unstable();
            apps.dedup();
            return Err(Miss::new(
                "app_not_found",
                format!(
                    "no window of app \"{app}\" (apps with windows: {})",
                    if apps.is_empty() {
                        "none".to_owned()
                    } else {
                        apps.join(", ")
                    }
                ),
            ));
        }
    }
    if let Some(window_id) = spec.window_id {
        candidates
            .retain(|window| window.get("window_id").and_then(Value::as_u64) == Some(window_id));
    }
    if let Some(title) = &spec.title {
        let before: Vec<String> = candidates
            .iter()
            .filter_map(|window| window.get("title").and_then(Value::as_str))
            .map(|title| format!("\"{}\"", clip(title, 40)))
            .collect();
        candidates.retain(|window| text_matches(window.get("title"), title));
        if candidates.is_empty() {
            return Err(Miss::new(
                "window_not_found",
                format!(
                    "no window titled like \"{title}\" for {} (titles: {})",
                    spec.describe(),
                    if before.is_empty() {
                        "none".to_owned()
                    } else {
                        before.join(", ")
                    }
                ),
            ));
        }
    }
    if candidates.is_empty() {
        return Err(Miss::new(
            "window_not_found",
            format!("no window for {}", spec.describe()),
        ));
    }
    // On-screen windows first, then the topmost (largest z_index).
    let mut ranked: Vec<(usize, &Value)> = candidates.into_iter().enumerate().collect();
    ranked.sort_by_key(|(order, window)| {
        let on_screen = window.get("is_on_screen").and_then(Value::as_bool) != Some(false);
        let z = window
            .get("z_index")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MIN);
        std::cmp::Reverse((on_screen, z, std::cmp::Reverse(*order)))
    });
    Ok(ranked
        .into_iter()
        .map(|(_, window)| Window {
            pid: window
                .get("pid")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
            window_id: window
                .get("window_id")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            app: window
                .get("app_name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            title: window
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned),
            on_screen: window.get("is_on_screen").and_then(Value::as_bool) != Some(false),
        })
        .collect())
}

/// Whether a pid-scoped `list_windows` result proves `window_id` closed: the
/// enumeration is complete and its authoritative id set (every window the
/// window server reported for the pid, helper windows included) lacks it.
/// A listing without that evidence proves nothing.
fn window_closed(listed: &Value, window_id: u64) -> bool {
    if listed.get("enumeration_complete").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    let Some(ids) = listed
        .get("authoritative_window_ids")
        .and_then(Value::as_array)
    else {
        return false;
    };
    // An entry that is not a window id makes the set unreadable, not a
    // proof that the window is missing from it.
    let Some(ids) = ids.iter().map(Value::as_u64).collect::<Option<Vec<u64>>>() else {
        return false;
    };
    !ids.contains(&window_id)
}

fn fields_hold(check: &Check, element: &Value) -> bool {
    let value = element.get("value").and_then(Value::as_str);
    if let Some(expected) = &check.value {
        if !value.is_some_and(|value| value.trim() == expected.trim()) {
            return false;
        }
    }
    if let Some(expected) = &check.value_contains {
        if !value.is_some_and(|value| fold(value).contains(&fold(expected))) {
            return false;
        }
    }
    if let Some(expected) = check.enabled {
        // Elements report `enabled` only when the platform knows it.
        if element.get("enabled").and_then(Value::as_bool) != Some(expected) {
            return false;
        }
    }
    if let Some(expected) = check.selected {
        if selected_state(element) != Some(expected) {
            return false;
        }
    }
    true
}

/// An element's on/off state. For a toggle control (checkbox, radio,
/// switch) its checked value ("1"/"0", "true"/"false", "on"/"off") wins:
/// Chromium reports web checkboxes as value "1" with AXSelected false,
/// because AXSelected means list selection, not checked. Otherwise the
/// platform's `selected` flag, else that value.
fn selected_state(element: &Value) -> Option<bool> {
    let checked = element
        .get("value")
        .and_then(Value::as_str)
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "on" | "checked" => Some(true),
            "0" | "false" | "off" | "unchecked" => Some(false),
            _ => None,
        });
    let toggle = element
        .get("role")
        .and_then(Value::as_str)
        .map(role_family)
        .is_some_and(|family| matches!(family.as_str(), "checkbox" | "radiobutton" | "switch"));
    if toggle && checked.is_some() {
        return checked;
    }
    element.get("selected").and_then(Value::as_bool).or(checked)
}

/// Display-only rows (static text and other rows without an element index)
/// that match `spec`, parsed into {role, label, value, text}. Scripts read
/// values such as "Ticket: 4F2K" from these; they cannot be acted on.
pub(crate) fn display_rows(read: &Read, spec: &ElementSpec) -> Vec<Value> {
    if spec.role.is_none() && spec.name.is_none() && spec.text.is_none() {
        return Vec::new();
    }
    read.markdown
        .lines()
        .map(str::trim)
        .map(|line| strip_folded_children_marker(line).unwrap_or(line))
        .filter_map(|line| line.strip_prefix("- "))
        .filter(|rest| !rest.starts_with('['))
        .filter_map(|rest| {
            let role = rest.split_whitespace().next()?.to_owned();
            let after = rest[role.len()..].trim_start();
            let quoted = |text: &str| -> Option<String> {
                let inner = text.strip_prefix('"')?;
                inner.find('"').map(|end| inner[..end].to_owned())
            };
            let label = quoted(after).or_else(|| {
                let open = after.rfind('(')?;
                after[open + 1..].strip_suffix(')').map(str::to_owned)
            });
            let value = after.find("= ").and_then(|at| quoted(&after[at + 2..]));
            let folded = fold(rest);
            if let Some(role_wanted) = &spec.role {
                if !role_matches(role_wanted, &role) {
                    return None;
                }
            }
            if let Some(name) = &spec.name {
                if !has_word_start(&folded, &fold(name)) {
                    return None;
                }
            }
            if let Some(text) = &spec.text {
                if !folded.contains(&fold(text)) {
                    return None;
                }
            }
            Some(json!({
                "role": role,
                "label": label,
                "value": value,
                "text": rest,
                "display_only": true,
            }))
        })
        .collect()
}

fn row_matches(row: &str, spec: &ElementSpec) -> bool {
    let row = strip_folded_children_marker(row).unwrap_or(row);
    if !row.starts_with("- ") {
        return false;
    }
    let folded = fold(row);
    if let Some(text) = &spec.text {
        if !folded.contains(&fold(text)) {
            return false;
        }
    }
    if let Some(name) = &spec.name {
        if !has_word_start(&folded, &fold(name)) {
            return false;
        }
    }
    if let Some(role) = &spec.role {
        let row_role = row
            .trim_start_matches("- ")
            .trim_start_matches(|c: char| c == '[' || c.is_ascii_digit() || c == ']')
            .split_whitespace()
            .next()
            .unwrap_or("");
        if !role_matches(role, row_role) {
            return false;
        }
    }
    spec.role.is_some() || spec.text.is_some() || spec.name.is_some()
}

fn near_misses(read: &Read, spec: &ElementSpec) -> String {
    // Same role, any name: the usual slip is a near-miss label.
    let relaxed = ElementSpec {
        role: spec.role.clone(),
        ..Default::default()
    };
    let pool: Vec<&Value> = if spec.role.is_some() {
        matches(read, &relaxed)
    } else if spec.name.is_some() {
        // Any role: only elements sharing a word with the name (below).
        read.elements
            .iter()
            .filter(|element| element.get("display_only").and_then(Value::as_bool) != Some(true))
            .collect()
    } else {
        Vec::new()
    };
    // Rank by the words a label shares with the wanted name, so "Update
    // event" is offered for "Create event" ahead of unrelated buttons, and
    // look across roles too: the element may be a link, not a button.
    let shared = |element: &Value| -> usize {
        let Some(name) = &spec.name else {
            return 0;
        };
        let Some(label) = label_of(element) else {
            return 0;
        };
        let label_words = words(label);
        words(name)
            .iter()
            .filter(|word| label_words.contains(word))
            .count()
    };
    let mut ranked: Vec<(usize, &Value)> = pool
        .iter()
        .copied()
        .filter(|element| label_of(element).is_some())
        .map(|element| (shared(element), element))
        .collect();
    if spec.role.is_some() && spec.name.is_some() {
        for element in &read.elements {
            let score = shared(element);
            if score > 0
                && element.get("display_only").and_then(Value::as_bool) != Some(true)
                && !ranked.iter().any(|(_, seen)| std::ptr::eq(*seen, element))
            {
                ranked.push((score, element));
            }
        }
    }
    if spec.role.is_none() {
        ranked.retain(|(score, _)| *score > 0);
    }
    ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    let named: Vec<String> = ranked
        .iter()
        .take(CANDIDATES_SHOWN)
        .map(|(_, element)| describe_element(element))
        .collect();
    if named.is_empty() {
        if read.elements.is_empty() {
            "; the window has no readable elements".to_owned()
        } else {
            String::new()
        }
    } else {
        format!("; nearest: {}", named.join("; "))
    }
}

pub(crate) fn describe_element(element: &Value) -> String {
    use crate::snapshot_compaction::{
        is_secure_text_field, redact_if_secure, redact_mirrored_secure_name, FieldSecurityMeta,
    };
    let role = element
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("element");
    let label = label_of(element);
    let value = element.get("value").and_then(Value::as_str);
    let meta = FieldSecurityMeta {
        role,
        subrole: element.get("subrole").and_then(Value::as_str),
        title: element.get("name").and_then(Value::as_str),
        description: label,
        placeholder: element.get("placeholder").and_then(Value::as_str),
        identifier: element.get("identifier").and_then(Value::as_str),
        help: element.get("help").and_then(Value::as_str),
        is_password_flag: element.get("secure").and_then(Value::as_bool) == Some(true)
            || element
                .get("states")
                .and_then(Value::as_array)
                .is_some_and(|states| states.iter().any(|state| state.as_str() == Some("secure"))),
    };
    let secure = is_secure_text_field(&meta);
    let safe_label = redact_mirrored_secure_name(secure, label.map(str::to_owned), value);
    let (safe_value, _) = redact_if_secure(&meta, value.map(str::to_owned), None);
    let mut out = match element.get("element_index").and_then(Value::as_u64) {
        Some(index) => format!("[{index}] {role}"),
        None => role.to_owned(),
    };
    if let Some(label) = safe_label.as_deref() {
        out.push_str(&format!(" \"{}\"", clip(label, 60)));
    }
    if let Some(value) = safe_value.as_deref() {
        if Some(value) != safe_label.as_deref() {
            out.push_str(&format!(" value=\"{}\"", clip(value, 60)));
        }
    }
    if element.get("enabled").and_then(Value::as_bool) == Some(false) {
        out.push_str(" disabled");
    }
    if element.get("selected").and_then(Value::as_bool) == Some(true) {
        out.push_str(" selected");
    }
    out
}

/// Lowercase words of two or more characters.
fn words(text: &str) -> Vec<String> {
    fold(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 2)
        .map(str::to_owned)
        .collect()
}

fn label_of(element: &Value) -> Option<&str> {
    element
        .get("label")
        .and_then(Value::as_str)
        .filter(|label| !label.trim().is_empty())
}

fn has_area(element: &Value) -> bool {
    let Some(frame) = element.get("frame") else {
        return true;
    };
    let dimension = |a: &str, b: &str| {
        frame
            .get(a)
            .or_else(|| frame.get(b))
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
    };
    dimension("w", "width") > 0.0 && dimension("h", "height") > 0.0
}

fn text_matches(value: Option<&Value>, needle: &str) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| fold(text).contains(&fold(needle)))
}

/// Whether `needle` occurs in `haystack` starting at a word boundary, so
/// "ok" matches "OK" and "Ok, continue" but not "Bookmark".
fn has_word_start(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.match_indices(needle).any(|(at, _)| {
        haystack[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_alphanumeric())
    })
}

fn fold(text: &str) -> String {
    text.trim().to_lowercase()
}

fn clip(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut out: String = flat.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn first_text(result: &ToolResult) -> String {
    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            Content::Text { text, .. } => Some(text.trim()),
            _ => None,
        })
        .unwrap_or("failed");
    clip(text.lines().next().unwrap_or(text), 300)
}

fn normalized_role(role: &str) -> String {
    let normalized: String = role
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(|c| c.to_lowercase())
        .collect();
    normalized
        .strip_prefix("ax")
        .unwrap_or(&normalized)
        .to_owned()
}

/// Role names models use, folded to one family per control kind, across the
/// macOS (AX), Windows (UIA) and Linux (AT-SPI) vocabularies.
pub(crate) fn role_family(role: &str) -> String {
    let normalized = normalized_role(role);
    let family = match normalized.as_str() {
        "button" | "pushbutton" | "togglebutton" => "button",
        "textfield" | "textarea" | "textbox" | "edit" | "entry" | "input" | "searchfield"
        | "securetextfield" | "passwordtext" | "text" => "textfield",
        "statictext" | "label" | "static" => "statictext",
        "link" | "hyperlink" => "link",
        "checkbox" | "checkbutton" | "check" => "checkbox",
        "radiobutton" | "radio" => "radiobutton",
        "popupbutton" | "combobox" | "menubutton" | "dropdown" | "select" => "popupbutton",
        "menuitem" | "menuitemcheckbox" | "menuitemradio" | "checkmenuitem" | "radiomenuitem" => {
            "menuitem"
        }
        "pagetab" | "tabitem" | "tab" => "tab",
        "image" | "img" | "icon" => "image",
        "row" | "listitem" | "dataitem" | "tablerow" | "outlinerow" => "row",
        "cell" | "tablecell" | "gridcell" => "cell",
        other => return other.to_owned(),
    };
    family.to_owned()
}

fn role_matches(wanted: &str, actual: &str) -> bool {
    let wanted = role_family(wanted);
    let actual = role_family(actual);
    wanted != "unknown" && wanted == actual
}

fn element_role_matches(wanted: &str, element: &Value) -> bool {
    use cua_driver_contract::Role;

    let wanted_canonical = Role::from_token(wanted);
    if wanted_canonical != Role::Unknown {
        if let Some(actual) = element.get("role_canonical") {
            return actual.as_str().is_some_and(|actual| {
                let actual = Role::from_token(actual);
                actual != Role::Unknown && actual == wanted_canonical
            });
        }
    }
    element
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|actual| role_matches(wanted, actual))
}

/// One driver call as the run_script journal records it.
pub(crate) fn journal_entry(
    tool: &str,
    pid: Option<i64>,
    window_id: Option<u64>,
    result: &crate::protocol::ToolResult,
) -> Value {
    let field = |key: &str| {
        result
            .structured_content
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    json!({
        "tool": tool,
        "pid": pid,
        "window_id": window_id,
        "ok": result.is_error != Some(true),
        "effect": field("effect"),
        "code": field("code").or_else(|| result.structured_content.as_ref().and_then(|value| value.pointer("/refusal/code")).and_then(Value::as_str).map(str::to_owned)),
    })
}

/// The window a call targets, from flat `pid`/`window_id` or a typed
/// `target: {kind: "window", pid, window_id}`.
pub(crate) fn call_target(args: &Value) -> (Option<i64>, Option<u64>) {
    let target = args.get("target");
    let pid = args.get("pid").and_then(Value::as_i64).or_else(|| {
        target
            .and_then(|target| target.get("pid"))
            .and_then(Value::as_i64)
    });
    let window_id = args.get("window_id").and_then(Value::as_u64).or_else(|| {
        target
            .and_then(|target| target.get("window_id"))
            .and_then(Value::as_u64)
    });
    (pid, window_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct TestClock(std::sync::atomic::AtomicU64);
    impl TestClock {
        fn advance(&self, duration: Duration) {
            self.0.fetch_add(
                duration.as_millis() as u64,
                std::sync::atomic::Ordering::SeqCst,
            );
        }
    }
    impl crate::actionability::Clock for TestClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.0.load(std::sync::atomic::Ordering::SeqCst))
        }
        fn sleep(
            &self,
            duration: Duration,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(async move {
                self.advance(duration);
            })
        }
    }
    struct TimedRead {
        def: crate::tool::ToolDef,
        clock: Arc<TestClock>,
        duration: Duration,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        incomplete: bool,
    }

    struct PendingRead {
        def: crate::tool::ToolDef,
        started: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl crate::tool::Tool for PendingRead {
        fn def(&self) -> &crate::tool::ToolDef {
            &self.def
        }
        async fn invoke(&self, _args: Value) -> ToolResult {
            self.started.notify_one();
            std::future::pending().await
        }
    }
    #[async_trait::async_trait]
    impl crate::tool::Tool for TimedRead {
        fn def(&self) -> &crate::tool::ToolDef {
            &self.def
        }
        async fn invoke(&self, _args: Value) -> ToolResult {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.clock.advance(self.duration);
            let mut state = json!({"pid":1,"window_id":2,"elements":[{"element_index":1,"role":"AXButton","label":"Save","depth":0}],"tree_markdown":""});
            if self.incomplete {
                state["observation_complete"] = json!(false);
                state["observation_incomplete_reasons"] =
                    json!([{"kind":"budget","detail":"node_budget"}]);
                state["elements"] = json!([]);
            }
            ToolResult::text("state").with_structured(state)
        }
    }
    fn timed_registry(
        clock: Arc<TestClock>,
        duration: Duration,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    ) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(TimedRead {
            def: crate::tool::ToolDef {
                name: "get_window_state".into(),
                description: "test".into(),
                input_schema: json!({"type":"object","additionalProperties":true}),
                read_only: true,
                destructive: false,
                idempotent: true,
                open_world: false,
            },
            clock,
            duration,
            calls,
            incomplete: false,
        }));
        registry
    }

    #[tokio::test]
    async fn locator_rejects_a_success_after_the_local_deadline() {
        let clock = Arc::new(TestClock::default());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registry = timed_registry(clock.clone(), Duration::from_millis(101), calls.clone());
        let locator = Locator::new(&registry, None, Instant::now() + Duration::from_secs(5))
            .with_clock(clock, Duration::from_secs(5));
        let check = Check::parse(
            &json!({"pid":1,"window_id":2,"name":"Save","timeout_ms":100}),
            0,
        )
        .unwrap();
        let outcome = locator.check(&check, &WindowSpec::default()).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let miss = outcome.unwrap_err();
        assert_eq!(miss.code, "timeout");
    }

    #[tokio::test]
    async fn late_incomplete_miss_preserves_code_and_typed_reasons() {
        for is_find in [false, true] {
            let clock = Arc::new(TestClock::default());
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut registry = ToolRegistry::new();
            registry.register(Box::new(TimedRead {
                def: crate::tool::ToolDef {
                    name: "get_window_state".into(),
                    description: "test".into(),
                    input_schema: json!({"type":"object","additionalProperties":true}),
                    read_only: true,
                    destructive: false,
                    idempotent: true,
                    open_world: false,
                },
                clock: clock.clone(),
                duration: Duration::from_millis(101),
                calls,
                incomplete: true,
            }));
            let locator = Locator::new(&registry, None, Instant::now() + Duration::from_secs(5))
                .with_clock(clock, Duration::from_secs(5));
            let check = Check::parse(
                &json!({"pid":1,"window_id":2,"name":"SECRET-QUERY","gone":true,"timeout_ms":100}),
                0,
            )
            .unwrap();
            let miss = if is_find {
                locator
                    .find(&check.window, &check.element, Duration::from_millis(100))
                    .await
                    .unwrap_err()
            } else {
                locator
                    .check(&check, &WindowSpec::default())
                    .await
                    .unwrap_err()
            };
            assert_eq!(miss.code, "timeout");
            assert!(
                miss.message.contains("incomplete_observation"),
                "{}",
                miss.message
            );
            assert!(
                miss.message.contains("budget/node_budget"),
                "{}",
                miss.message
            );
            assert!(!miss.message.contains("SECRET-QUERY"), "{}", miss.message);
        }
    }

    #[tokio::test]
    async fn locator_clock_accepts_equality_and_zero_but_not_expired_enclosing_budget() {
        for (local_ms, elapsed_ms, enclosing_ms, allowed, expected_calls) in [
            (100, 100, 500, true, 1),
            (0, 101, 500, true, 1),
            (0, 0, 0, false, 0),
            (0, 501, 500, false, 1),
        ] {
            let clock = Arc::new(TestClock::default());
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let registry = timed_registry(
                clock.clone(),
                Duration::from_millis(elapsed_ms),
                calls.clone(),
            );
            let locator = Locator::new(&registry, None, Instant::now() + Duration::from_secs(5))
                .with_clock(clock, Duration::from_millis(enclosing_ms));
            let check = Check::parse(
                &json!({"pid":1,"window_id":2,"name":"Save","timeout_ms":local_ms}),
                0,
            )
            .unwrap();
            assert_eq!(
                locator.check(&check, &WindowSpec::default()).await.is_ok(),
                allowed,
                "local={local_ms} elapsed={elapsed_ms} enclosing={enclosing_ms}"
            );
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                expected_calls
            );
        }
    }

    #[tokio::test]
    async fn incomplete_observation_waits_until_deadline_without_extra_reads() {
        let clock = Arc::new(TestClock::default());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(TimedRead {
            def: crate::tool::ToolDef {
                name: "get_window_state".into(),
                description: "test".into(),
                input_schema: json!({"type":"object","additionalProperties":true}),
                read_only: true,
                destructive: false,
                idempotent: true,
                open_world: false,
            },
            clock: clock.clone(),
            duration: Duration::ZERO,
            calls: calls.clone(),
            incomplete: true,
        }));
        let locator = Locator::new(&registry, None, Instant::now() + Duration::from_secs(5))
            .with_clock(clock.clone(), Duration::from_secs(5));
        let check = Check::parse(
            &json!({"pid":1,"window_id":2,"name":"Save","gone":true,"timeout_ms":600}),
            0,
        )
        .unwrap();
        let miss = locator
            .check(&check, &WindowSpec::default())
            .await
            .unwrap_err();
        assert_eq!(miss.code, "incomplete_observation");
        assert_eq!(clock.0.load(std::sync::atomic::Ordering::SeqCst), 600);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn zero_timeout_probe_can_be_cancelled() {
        let started = Arc::new(tokio::sync::Notify::new());
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(PendingRead {
            def: crate::tool::ToolDef {
                name: "get_window_state".into(),
                description: "test".into(),
                input_schema: json!({"type":"object","additionalProperties":true}),
                read_only: true,
                destructive: false,
                idempotent: true,
                open_world: false,
            },
            started: started.clone(),
        }));
        let locator = Locator::new_owned(
            Arc::new(registry),
            None,
            Instant::now() + Duration::from_secs(5),
            None,
            None,
            None,
        )
        .with_clock(Arc::new(TestClock::default()), Duration::from_secs(5));
        let check = Check::parse(
            &json!({"pid":1,"window_id":2,"name":"Save","timeout_ms":0}),
            0,
        )
        .unwrap();
        let mut task =
            tokio::spawn(async move { locator.check(&check, &WindowSpec::default()).await });
        tokio::select! {
            _ = started.notified() => {},
            result = &mut task => panic!("probe completed before cancellation: {result:?}"),
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    fn read(elements: Vec<Value>, markdown: &str) -> Read {
        Read {
            window: Window {
                pid: 1,
                window_id: 2,
                app: Some("Demo".into()),
                title: Some("Doc".into()),
                on_screen: true,
            },
            elements,
            markdown: markdown.to_owned(),
            completeness: Completeness::Unasserted,
            scope_exclusions: Vec::new(),
        }
    }

    #[test]
    fn exclusion_guards_text_queries_and_malformed_kinds() {
        let typed_text = ElementSpec {
            role: Some("button".into()),
            text: Some("Save".into()),
            ..Default::default()
        };
        for kind in [ScopeExclusion::ClosedMenus, ScopeExclusion::CollapsedLists] {
            assert!(exclusion_guards(kind, &typed_text));
        }
        for malformed in [
            json!([{}]),
            json!([{"kind":null}]),
            json!([{"kind":"future_kind"}]),
            json!({}),
        ] {
            let state = json!({"observation_scope_exclusions":malformed});
            let mut observation = read(Vec::new(), "");
            observation.scope_exclusions = scope_exclusions(&state);
            let button = ElementSpec {
                role: Some("button".into()),
                ..Default::default()
            };
            assert_eq!(
                observation.incomplete_for(&button),
                Some(vec!["unknown_scope_exclusion".into()])
            );
        }
    }

    #[test]
    fn u20b_u23b_exclusions_and_folded_rows_guard_together() {
        use cua_driver_contract::ScopeExclusionKind;
        let markdown = "- AXRow = \"Foo\" [+1 more: folded_children; descendants not omitted] [reason kinds: folded_children=policy_omission]";
        for (kind, expected) in [
            (
                ScopeExclusionKind::ClosedMenusNotExpanded,
                "closed_menus_not_expanded",
            ),
            (
                ScopeExclusionKind::CollapsedListsNotExpanded,
                "collapsed_lists_not_expanded",
            ),
        ] {
            let mut budget = crate::walk_budget::WalkBudget::nodes_only(100);
            budget.assert_exhaustive();
            budget.record_verified_scope_exclusion(kind, None);
            let mut state = json!({});
            budget.outcome().apply(&mut state);
            assert_eq!(state["observation_complete"], true);
            let mut observed = read(vec![], markdown);
            observed.completeness = completeness(&state);
            observed.scope_exclusions = scope_exclusions(&state);
            let check =
                Check::parse(&json!({"role":"statictext","name":"Foo","gone":true}), 0).unwrap();
            let miss = evaluate(&check, &observed).unwrap_err();
            assert_eq!(miss.code, "incomplete_observation");
            assert!(miss.message.contains(expected));
            assert!(miss.message.contains("folded_children"));
            let button =
                Check::parse(&json!({"role":"button","name":"Bar","gone":true}), 0).unwrap();
            assert!(evaluate(&button, &observed).is_ok());
            let mut budget = crate::walk_budget::WalkBudget::nodes_only(100);
            budget.assert_exhaustive();
            budget.record_scope_exclusion(kind, None);
            budget.outcome().apply(&mut state);
            observed.completeness = completeness(&state);
            observed.scope_exclusions = scope_exclusions(&state);
            let miss = evaluate(&check, &observed).unwrap_err();
            assert!(miss.message.contains("excluded_content_unclassified"));
            assert!(miss.message.contains("folded_children"));
            assert_eq!(
                evaluate(&button, &observed).unwrap_err().code,
                "incomplete_observation"
            );
        }
        let mut observed = read(vec![], markdown);
        observed.completeness = Completeness::Complete;
        observed.scope_exclusions =
            scope_exclusions(&json!({"observation_scope_exclusions":[{"kind":"future_kind"}]}));
        let check =
            Check::parse(&json!({"role":"statictext","name":"Foo","gone":true}), 0).unwrap();
        let miss = evaluate(&check, &observed).unwrap_err();
        assert!(miss.message.contains("unknown_scope_exclusion"));
        assert!(miss.message.contains("folded_children"));
    }

    #[test]
    fn u20b_a_folded_role_makes_absence_uniqueness_and_nth_incomplete() {
        let markdown = "- AXRow = \"Foo\" [+1 more: folded_children; descendants not omitted] [reason kinds: folded_children=policy_omission]";
        for completeness in [Completeness::Complete, Completeness::Unasserted] {
            let mut observed = read(vec![element(7, "AXStaticText", "Other")], markdown);
            observed.completeness = completeness;
            let check = |raw: Value| Check::parse(&raw, 0).unwrap();
            let miss = evaluate(
                &check(json!({"role":"statictext","name":"Foo","gone":true})),
                &observed,
            )
            .unwrap_err();
            assert_eq!(miss.code, "incomplete_observation");
            assert!(miss.message.contains("folded_children"));
            for nth in [None, Some(0)] {
                let mut spec = check(json!({"role":"statictext"})).element;
                spec.nth = nth;
                assert_eq!(
                    pick(&observed, &spec).unwrap_err().code,
                    "incomplete_observation"
                );
            }
            let nth = check(json!({"role":"statictext","nth":0}));
            assert_eq!(
                evaluate(&nth, &observed).unwrap_err().code,
                "incomplete_observation"
            );
            assert!(evaluate(&check(json!({"name":"Foo"})), &observed).is_ok());
            assert!(evaluate(
                &check(json!({"text":"folded_children","gone":true})),
                &observed
            )
            .is_ok());
            assert!(evaluate(
                &check(json!({"role":"button","name":"Bar","gone":true})),
                &observed
            )
            .is_ok());
            assert!(evaluate(&check(json!({"role":"row","name":"Foo"})), &observed).is_ok());
            assert_eq!(
                pick(
                    &observed,
                    &check(json!({"role":"row","name":"Foo"})).element
                )
                .unwrap_err()
                .code,
                "not_addressable"
            );
            assert!(evaluate(
                &check(json!({"role":"button","name":"folded_children","gone":true})),
                &observed
            )
            .is_ok());
            let miss = evaluate(
                &check(json!({"role":"statictext","text":"Foo","gone":true})),
                &observed,
            )
            .unwrap_err();
            assert_eq!(miss.code, "incomplete_observation");
            let indexed = read(vec![], "- [0] AXWindow \"Doc\" [+1 more: folded_children; descendants not omitted] [reason kinds: folded_children=policy_omission]");
            assert_eq!(
                pick(&indexed, &check(json!({"role":"group"})).element)
                    .unwrap_err()
                    .code,
                "incomplete_observation"
            );
        }
    }

    #[test]
    fn u20b_review_guard_only_covers_matchable_absorbed_roles() {
        let markdown = "- AXRow = \"Foo\" [+1 more: folded_children; descendants not omitted] [reason kinds: folded_children=policy_omission]";
        let mut observed = read(vec![], markdown);
        observed.completeness = Completeness::Complete;
        let check = |raw: Value| Check::parse(&raw, 0).unwrap();
        assert!(evaluate(&check(json!({"role":"button","gone":true})), &observed).is_ok());
        assert!(evaluate(
            &check(json!({"role":"button","name":"Foo","gone":true})),
            &observed
        )
        .is_ok());
        let roles: Vec<_> = crate::snapshot_compaction::absorbed_role_tokens().collect();
        for &role in &roles {
            assert!(
                crate::snapshot_compaction::is_plain_text_subtree_role(role)
                    || crate::snapshot_compaction::is_elidable_wrapper_role(role)
            );
            let guarded = roles.iter().any(|candidate| role_matches(role, candidate));
            let result = evaluate(&check(json!({"role":role,"gone":true})), &observed);
            assert_eq!(
                result
                    .as_ref()
                    .err()
                    .is_some_and(|miss| miss.code == "incomplete_observation"),
                guarded,
                "{role}"
            );
        }
        assert!(role_matches("textfield", "text"));
        assert!(role_matches("text", "textfield"));
        assert!(role_matches("textfield", "AXTextField"));
        assert!(!role_matches("button", "text"));
        assert_eq!(
            evaluate(&check(json!({"role":"textfield","gone":true})), &observed)
                .unwrap_err()
                .code,
            "incomplete_observation"
        );
        assert_eq!(
            evaluate(
                &check(json!({"role":"textfield","name":"Foo","gone":true})),
                &observed
            )
            .unwrap_err()
            .code,
            "incomplete_observation"
        );
        let mut row = element(4, "AXRow", "Foo");
        row["boundary"] = json!({"reasons":[{"kind":"policy_omission","detail":"folded_children"}],"children_skipped":{"known":1},"descendants_omitted":false});
        let observed = read(vec![row], "");
        assert!(evaluate(&check(json!({"role":"button","gone":true})), &observed).is_ok());
        assert_eq!(
            evaluate(&check(json!({"role":"textfield","gone":true})), &observed)
                .unwrap_err()
                .code,
            "incomplete_observation"
        );
    }

    #[test]
    fn u20b_review_marker_parser_does_not_truncate_page_text() {
        let markdown = "- AXRow = \"Foo [more: decoy [+ other] target\" [+1 more: folded_children; descendants not omitted] [reason kinds: folded_children=policy_omission]";
        let observed = read(vec![], markdown);
        let check =
            Check::parse(&json!({"role":"statictext","name":"target","gone":true}), 0).unwrap();
        assert_eq!(
            evaluate(&check, &observed).unwrap_err().code,
            "incomplete_observation"
        );
        assert!(strip_folded_children_marker(
            "- AXRow [+x more: folded_children] [reason kinds: folded_children=policy_omission]"
        )
        .is_none());
        assert!(strip_folded_children_marker(markdown)
            .unwrap()
            .contains("target"));
        let unknown = "- AXRow = \"Foo [+8 more: decoy] [more: decoy] target\" [more: unknown: folded_children; descendants not omitted] [reason kinds: folded_children=policy_omission]";
        assert!(strip_folded_children_marker(unknown)
            .unwrap()
            .contains("target"));
        let observed = read(vec![], unknown);
        assert_eq!(
            evaluate(&check, &observed).unwrap_err().code,
            "incomplete_observation"
        );
    }

    #[test]
    fn u20b_review_structured_fold_text_precedes_markdown_fallback() {
        let mut row = element(4, "AXRow", "Foo");
        row["value"] = json!("summary");
        row["boundary"] = json!({"reasons":[{"kind":"policy_omission","detail":"folded_children"}],"children_skipped":{"known":1},"descendants_omitted":false});
        let markdown = "- [4] AXRow \"Foo [more: decoy [+ fake] target\" [+1 more: folded_children; descendants not omitted] [reason kinds: folded_children=policy_omission]";
        let check =
            Check::parse(&json!({"role":"statictext","name":"target","gone":true}), 0).unwrap();
        assert!(evaluate(&check, &read(vec![row.clone()], markdown)).is_ok());
        row.as_object_mut().unwrap().remove("value");
        assert_eq!(
            evaluate(&check, &read(vec![row], markdown))
                .unwrap_err()
                .code,
            "incomplete_observation"
        );
    }

    #[test]
    fn u20b_review_structured_text_matches_rendered_rows_and_exposes_extra_fields() {
        let mut candidate = element(1, "AXButton", "Foo");
        candidate["value"] = json!("Bar");
        candidate["identifier"] = json!("beta");
        candidate["description"] = json!("not-rendered");
        let markdown = "- [1] AXButton \"Foo\" [value=\"Bar\" id=beta]";
        let observed = read(vec![candidate], markdown);
        for text in ["foo", "BAR", "beta", "no-match"] {
            let spec = Check::parse(&json!({"role":"button","text":text}), 0)
                .unwrap()
                .element;
            assert_eq!(
                !matches(&observed, &spec).is_empty(),
                row_matches(markdown, &spec),
                "{text}"
            );
        }
        let spec = Check::parse(&json!({"role":"button","text":"not-rendered"}), 0)
            .unwrap()
            .element;
        assert!(!matches(&observed, &spec).is_empty());
        assert!(!row_matches(markdown, &spec));
    }

    #[test]
    fn u20b_a_structured_only_folded_rows_guard_roles() {
        let mut folded = element(3, "AXRow", "Foo");
        folded["boundary"] = json!({"reasons":[{"kind":"policy_omission","detail":"folded_children"}],"children_skipped":{"known":1},"descendants_omitted":false});
        for completeness in [Completeness::Complete, Completeness::Unasserted] {
            let mut observed = read(vec![folded.clone()], "");
            observed.completeness = completeness;
            let check = |raw: Value| Check::parse(&raw, 0).unwrap();
            let miss = evaluate(
                &check(json!({"role":"statictext","name":"Foo","gone":true})),
                &observed,
            )
            .unwrap_err();
            assert_eq!(miss.code, "incomplete_observation");
            assert_eq!(
                pick(&observed, &check(json!({"role":"statictext"})).element)
                    .unwrap_err()
                    .code,
                "incomplete_observation"
            );
            assert!(evaluate(
                &check(json!({"role":"button","name":"Bar","gone":true})),
                &observed
            )
            .is_ok());
        }
        folded.as_object_mut().unwrap().remove("label");
        let check =
            Check::parse(&json!({"role":"statictext","name":"Bar","gone":true}), 0).unwrap();
        let observed = read(vec![folded.clone()], "");
        assert_eq!(
            evaluate(&check, &observed).unwrap_err().code,
            "incomplete_observation"
        );
        let observed = read(vec![folded], "- [3] AXRow \"Foo\" [+1 more: folded_children; descendants not omitted] [reason kinds: folded_children=policy_omission]");
        assert!(evaluate(&check, &observed).is_ok());
    }
    #[test]
    fn u20b_a_structured_only_observed_text_proves_presence() {
        let mut observed = read(vec![element(4, "AXStaticText", "Foo")], "");
        observed.completeness = Completeness::Complete;
        let check =
            Check::parse(&json!({"role":"statictext","text":"Foo","gone":true}), 0).unwrap();
        assert_eq!(
            evaluate(&check, &observed).unwrap_err().code,
            "still_present"
        );
        let check =
            Check::parse(&json!({"role":"statictext","text":"Bar","gone":true}), 0).unwrap();
        assert!(evaluate(&check, &observed).is_ok());
    }
    #[test]
    fn u20b_a_text_filter_precedes_exact_name_ranking() {
        let mut exact = element(1, "AXStaticText", "Save");
        exact["value"] = json!("other");
        let mut partial = element(2, "AXStaticText", "Save Draft");
        partial["value"] = json!("target");
        let mut observed = read(vec![exact, partial], "");
        observed.completeness = Completeness::Complete;
        let check = Check::parse(
            &json!({"role":"statictext","name":"Save","text":"target","gone":true}),
            0,
        )
        .unwrap();
        assert_eq!(
            evaluate(&check, &observed).unwrap_err().code,
            "still_present"
        );
        assert_eq!(
            pick(&observed, &check.element).unwrap().element["element_index"],
            2
        );
    }

    #[test]
    fn u20b_omitted_seventh_snippet_cannot_be_proven_gone() {
        use crate::snapshot_compaction::{
            fold_actionless_text_subtrees_with_reasons, CompactableNode,
        };
        struct Node {
            depth: usize,
            text: Option<String>,
            title: Option<String>,
            boundary: Option<crate::tree_boundary::NodeBoundary>,
        }
        impl CompactableNode for Node {
            fn element_index(&self) -> Option<usize> {
                None
            }
            fn role(&self) -> &str {
                if self.depth == 0 {
                    "AXRow"
                } else {
                    "AXStaticText"
                }
            }
            fn depth(&self) -> usize {
                self.depth
            }
            fn set_depth(&mut self, depth: usize) {
                self.depth = depth;
            }
            fn title(&self) -> Option<&str> {
                self.title.as_deref()
            }
            fn value(&self) -> Option<&str> {
                self.text.as_deref()
            }
            fn set_value(&mut self, value: Option<String>) {
                self.text = value;
            }
            fn description(&self) -> Option<&str> {
                None
            }
            fn format_line(&self) -> String {
                format!(
                    "- {} \"{}\" = \"{}\"",
                    self.role(),
                    self.title().unwrap_or_default(),
                    self.value().unwrap_or_default()
                )
            }
            fn boundary_mut(&mut self) -> Option<&mut Option<crate::tree_boundary::NodeBoundary>> {
                Some(&mut self.boundary)
            }
        }
        let mut nodes = vec![Node {
            depth: 0,
            text: None,
            title: None,
            boundary: None,
        }];
        nodes.extend((0..7).map(|n| Node {
            depth: 1,
            text: Some(format!("item {n}")),
            title: None,
            boundary: None,
        }));
        let mut lines = nodes
            .iter()
            .map(|node| (node.depth(), node.format_line()))
            .collect();
        let reasons = fold_actionless_text_subtrees_with_reasons(&mut lines, &mut nodes);
        let mut budget = crate::walk_budget::WalkBudget::nodes_only(100);
        budget.assert_exhaustive();
        for reason in reasons {
            budget.record_skip(reason);
        }
        let mut state = json!({});
        budget.outcome().apply(&mut state);
        assert_eq!(state["observation_complete"], false);
        let mut observed = read(vec![], &lines[0].1);
        observed.completeness = completeness(&state);
        let check = Check::parse(&json!({"text":"item 6","gone":true}), 0).unwrap();
        assert_eq!(
            evaluate(&check, &observed).unwrap_err().code,
            "incomplete_observation"
        );
        observed.elements = vec![element(1, "AXButton", "Save")];
        for nth in [None, Some(0)] {
            let mut selector = Check::parse(&json!({"name":"Save"}), 0).unwrap().element;
            selector.nth = nth;
            assert_eq!(
                pick(&observed, &selector).unwrap_err().code,
                "incomplete_observation"
            );
        }
        let mut nodes = vec![
            Node {
                depth: 0,
                text: None,
                title: None,
                boundary: None,
            },
            Node {
                depth: 1,
                text: Some("Foo".into()),
                title: None,
                boundary: None,
            },
        ];
        let mut lines = nodes
            .iter()
            .map(|node| (node.depth(), node.format_line()))
            .collect();
        let reasons = fold_actionless_text_subtrees_with_reasons(&mut lines, &mut nodes);
        assert!(reasons.is_empty());
        let mut budget = crate::walk_budget::WalkBudget::nodes_only(100);
        budget.assert_exhaustive();
        let mut state = json!({});
        budget.outcome().apply(&mut state);
        assert_eq!(state["observation_complete"], true);
        let mut observed = read(vec![], &lines[0].1);
        observed.completeness = completeness(&state);
        let check =
            Check::parse(&json!({"role":"statictext","name":"Foo","gone":true}), 0).unwrap();
        assert_eq!(
            evaluate(&check, &observed).unwrap_err().code,
            "incomplete_observation"
        );
        for (value, title) in [("bookmark", "ok"), ("width", "id")] {
            let mut nodes = vec![
                Node {
                    depth: 0,
                    text: None,
                    title: None,
                    boundary: None,
                },
                Node {
                    depth: 1,
                    text: Some(value.into()),
                    title: Some(title.into()),
                    boundary: None,
                },
            ];
            let mut lines = nodes
                .iter()
                .map(|node| (node.depth(), node.format_line()))
                .collect();
            let reasons = fold_actionless_text_subtrees_with_reasons(&mut lines, &mut nodes);
            let mut budget = crate::walk_budget::WalkBudget::nodes_only(100);
            budget.assert_exhaustive();
            for reason in reasons {
                budget.record_skip(reason);
            }
            let mut state = json!({});
            budget.outcome().apply(&mut state);
            let mut observed = read(vec![], &lines[0].1);
            observed.completeness = completeness(&state);
            let check =
                Check::parse(&json!({"role":"statictext","name":title,"gone":true}), 0).unwrap();
            assert_eq!(
                evaluate(&check, &observed).unwrap_err().code,
                "incomplete_observation",
                "{value}/{title}"
            );
            assert_eq!(state["observation_complete"], false);
        }
    }

    fn element(index: u64, role: &str, label: &str) -> Value {
        json!({
            "element_index": index,
            "element_token": format!("s00000001:{index}"),
            "role": role,
            "label": label,
            "frame": {"x": 0, "y": 0, "w": 10, "h": 10},
        })
    }

    #[test]
    fn secure_candidates_never_render_unsanitized_name_or_value() {
        for row in [
            json!({"element_index": 1, "role": "AXSecureTextField", "label": "SECRET-NAME", "value": "SECRET-VALUE"}),
            json!({"element_index": 1, "role": "edit", "secure": true, "label": "SECRET-NAME", "value": "SECRET-VALUE"}),
            json!({"element_index": 1, "role": "edit", "states": ["secure"], "label": "SECRET-NAME", "value": "SECRET-VALUE"}),
        ] {
            let rendered = describe_element(&row);
            assert!(!rendered.contains("SECRET-NAME"), "{rendered}");
            assert!(!rendered.contains("SECRET-VALUE"), "{rendered}");
            assert!(rendered.contains("[redacted]"), "{rendered}");
        }
    }

    #[test]
    fn only_a_complete_readable_id_set_proves_a_window_closed() {
        let closed = |listed: Value| window_closed(&listed, 7);
        assert!(closed(
            json!({"enumeration_complete": true, "authoritative_window_ids": [5]})
        ));
        assert!(!closed(
            json!({"enumeration_complete": true, "authoritative_window_ids": [5, 7]})
        ));
        assert!(!closed(
            json!({"enumeration_complete": true, "authoritative_window_ids": ["7"]})
        ));
        assert!(!closed(
            json!({"enumeration_complete": true, "authoritative_window_ids": [5, "x"]})
        ));
        assert!(!closed(
            json!({"enumeration_complete": true, "authoritative_window_ids": [-7]})
        ));
        assert!(!closed(
            json!({"enumeration_complete": false, "authoritative_window_ids": [5]})
        ));
        assert!(!closed(json!({"enumeration_complete": true})));
    }

    #[test]
    fn unknown_enabled_never_satisfies_enabled_true() {
        let read = read(vec![element(1, "AXButton", "Save")], "");
        let check = Check::parse(&json!({"name":"Save", "enabled":true}), 0).unwrap();
        let miss = evaluate(&check, &read).unwrap_err();
        assert_eq!(miss.code, "unexpected_state");
        assert!(miss.message.contains("state unknown"), "{}", miss.message);
    }

    #[test]
    fn unknown_selected_never_satisfies_selected_false() {
        let read = read(vec![element(1, "AXButton", "Save")], "");
        let check = Check::parse(&json!({"name":"Save", "selected":false}), 0).unwrap();
        let miss = evaluate(&check, &read).unwrap_err();
        assert_eq!(miss.code, "unexpected_state");
        assert!(miss.message.contains("state unknown"), "{}", miss.message);
    }

    #[test]
    fn missing_value_never_satisfies_empty_value() {
        let read = read(vec![element(1, "AXTextField", "Name")], "");
        for contains in [false, true] {
            let mut check = Check::parse(&json!({"name":"Name"}), 0).unwrap();
            if contains {
                check.value_contains = Some(String::new());
            } else {
                check.value = Some(String::new());
            }
            let miss = evaluate(&check, &read).unwrap_err();
            assert_eq!(miss.code, "unexpected_state");
            assert!(miss.message.contains("state unknown"), "{}", miss.message);
        }
    }

    #[test]
    fn exact_name_beats_substring_and_roles_fold_across_platforms() {
        let read = read(
            vec![
                element(1, "AXButton", "Save As…"),
                element(2, "AXButton", "Save"),
                element(3, "AXTextField", "Save"),
            ],
            "",
        );
        let spec = ElementSpec {
            role: Some("button".into()),
            name: Some("save".into()),
            ..Default::default()
        };
        let found = pick(&read, &spec).unwrap();
        assert_eq!(found.element["element_index"], 2);
        let field = ElementSpec {
            role: Some("textbox".into()),
            name: Some("Save".into()),
            ..Default::default()
        };
        assert_eq!(pick(&read, &field).unwrap().element["element_index"], 3);
        assert_eq!(role_family("Push Button"), "button");
        assert_eq!(role_family("Hyperlink"), "link");
    }

    #[test]
    fn atspi_text_matches_textfield_selector() {
        let read = read(vec![element(1, "text", "Reason")], "");
        let spec = ElementSpec {
            role: Some("textfield".into()),
            name: Some("Reason".into()),
            ..Default::default()
        };
        assert_eq!(pick(&read, &spec).unwrap().element["element_index"], 1);
    }

    #[test]
    fn atspi_text_is_preferred_for_roleless_text_actions() {
        let read = read(
            vec![element(1, "label", "Reason"), element(2, "text", "Reason")],
            "",
        );
        for action in ["set_value", "type_text"] {
            let spec = ElementSpec {
                name: Some("Reason".into()),
                action: Some(action),
                ..Default::default()
            };
            assert_eq!(
                pick(&read, &spec).unwrap().element["element_index"],
                2,
                "{action}"
            );
        }
    }

    #[test]
    fn text_role_is_guarded_without_guarding_other_textfield_family_roles() {
        for kind in [ScopeExclusion::ClosedMenus, ScopeExclusion::CollapsedLists] {
            let mut read = read(Vec::new(), "");
            read.scope_exclusions.push(kind);
            for role in ["text", " Text ", "AXText", "t-e-x-t"] {
                let spec = ElementSpec {
                    role: Some(role.into()),
                    ..Default::default()
                };
                assert!(read.incomplete_for(&spec).is_some(), "{kind:?} {role}");
            }
            for role in ["textfield", "edit", "entry"] {
                let spec = ElementSpec {
                    role: Some(role.into()),
                    ..Default::default()
                };
                assert!(read.incomplete_for(&spec).is_none(), "{kind:?} {role}");
            }
        }
    }

    #[test]
    fn canonical_role_precedes_native_and_unknown_never_matches_unknown() {
        let mut canonical = element(1, "AXButton", "one");
        canonical["role_canonical"] = json!("text_field");
        let mut unknown = element(2, "unknown", "two");
        unknown["role_canonical"] = json!("unknown");
        let read = read(
            vec![
                canonical,
                element(3, "AXButton", "three"),
                unknown,
                element(4, "unknown", "four"),
            ],
            "",
        );
        let spec = |role: &str| ElementSpec {
            role: Some(role.into()),
            ..Default::default()
        };
        assert_eq!(matches(&read, &spec(" Text_Field "))[0]["element_index"], 1);
        assert_eq!(matches(&read, &spec("button"))[0]["element_index"], 3);
        assert_eq!(matches(&read, &spec("Push Button"))[0]["element_index"], 1);
        assert!(matches(&read, &spec("unknown")).is_empty());
    }

    #[test]
    fn a_short_name_matches_whole_words_only() {
        let read = read(
            vec![
                element(1, "AXButton", "Bookmark this tab"),
                element(2, "AXButton", "Ok, continue"),
            ],
            "",
        );
        let spec = ElementSpec {
            role: Some("button".into()),
            name: Some("ok".into()),
            ..Default::default()
        };
        assert_eq!(pick(&read, &spec).unwrap().element["element_index"], 2);
        assert!(has_word_start("save as…", "as"));
        assert!(!has_word_start("bookmark", "ok"));
    }

    #[test]
    fn ambiguity_lists_candidates_and_nth_picks_one() {
        let read = read(
            vec![element(4, "AXButton", "OK"), element(9, "AXButton", "OK")],
            "",
        );
        let spec = ElementSpec {
            role: Some("button".into()),
            name: Some("OK".into()),
            ..Default::default()
        };
        let miss = pick(&read, &spec).unwrap_err();
        assert_eq!(miss.code, "ambiguous");
        assert!(
            miss.message.contains("nth 1: [9] AXButton \"OK\""),
            "{}",
            miss.message
        );
        let second = ElementSpec {
            nth: Some(1),
            ..spec
        };
        assert_eq!(pick(&read, &second).unwrap().element["element_index"], 9);
    }

    #[test]
    fn a_row_without_an_index_says_to_click_it_by_pixel() {
        // v037b: the LibreOffice sheet tab "Controls" is an AXRadioButton row
        // with no actions; the miss offered the menu item "Form Controls".
        let read = read(
            vec![element(359, "AXMenuItem", "Form Controls")],
            "- [0] AXWindow \"Doc\"\n  - [99] AXButton \"Add\"\n    - AXRadioButton \"Controls\"\n",
        );
        let spec = ElementSpec {
            role: Some("AXRadioButton".into()),
            name: Some("Controls".into()),
            ..Default::default()
        };
        let miss = pick(&read, &spec).unwrap_err();
        assert_eq!(miss.code, "not_addressable");
        assert!(
            miss.message.contains("no element index"),
            "{}",
            miss.message
        );
        assert!(miss.message.contains("by pixel"), "{}", miss.message);
        assert!(!miss.worth_waiting());
    }

    #[test]
    fn a_tie_goes_to_the_exact_role_or_what_the_tool_can_use() {
        // v037a: set_value name "Reason" matched the label and the field.
        let reason = read(
            vec![
                element(31, "AXStaticText", "Reason"),
                element(32, "AXTextArea", "Reason"),
            ],
            "",
        );
        let by_name = |action| ElementSpec {
            name: Some("Reason".into()),
            action,
            ..Default::default()
        };
        let set = pick(&reason, &by_name(Some("set_value"))).unwrap();
        assert_eq!(set.element["element_index"], 32);
        let click = pick(&reason, &by_name(Some("click"))).unwrap();
        assert_eq!(click.element["element_index"], 32);
        // Without a known tool the tie stays a tie.
        assert_eq!(pick(&reason, &by_name(None)).unwrap_err().code, "ambiguous");

        // v037a: role "textfield" matched AXTextField "A1" and AXTextArea "A1".
        let cells = read(
            vec![
                element(3, "AXTextField", "A1"),
                element(67, "AXTextArea", "A1"),
            ],
            "",
        );
        let field = ElementSpec {
            role: Some("textfield".into()),
            name: Some("A1".into()),
            ..Default::default()
        };
        assert_eq!(pick(&cells, &field).unwrap().element["element_index"], 3);
        // Two of the exact role stay ambiguous.
        let twins = read(
            vec![element(1, "AXButton", "OK"), element(2, "AXButton", "OK")],
            "",
        );
        let ok = ElementSpec {
            role: Some("button".into()),
            name: Some("OK".into()),
            action: Some("click"),
            ..Default::default()
        };
        assert_eq!(pick(&twins, &ok).unwrap_err().code, "ambiguous");
    }

    #[test]
    fn a_miss_lists_labels_sharing_a_word_first_across_roles() {
        // v036: "Create event" had become "Update event"; the miss listed
        // the first five buttons instead.
        let mut elements: Vec<Value> = [
            "Aster kickoff",
            "Boreal review",
            "Cinder decision",
            "Dune handoff",
            "Lighthouse lunch",
            "Update event",
        ]
        .iter()
        .enumerate()
        .map(|(at, label)| element(at as u64 + 1, "AXButton", label))
        .collect();
        elements.push(element(9, "AXLink", "Event history"));
        let read = read(elements, "");
        let spec = ElementSpec {
            role: Some("button".into()),
            name: Some("Create event".into()),
            ..Default::default()
        };
        let miss = pick(&read, &spec).unwrap_err();
        let nearest = miss.message.split("nearest: ").nth(1).unwrap();
        assert!(
            nearest.starts_with("[6] AXButton \"Update event\"; [9] AXLink \"Event history\""),
            "{nearest}"
        );

        // With no role, only labels sharing a word are offered.
        let any_role = ElementSpec {
            name: Some("event log".into()),
            ..Default::default()
        };
        let miss = pick(&read, &any_role).unwrap_err();
        assert!(!miss.message.contains("Aster"), "{}", miss.message);
        assert!(miss.message.contains("Update event"), "{}", miss.message);
    }

    #[test]
    fn a_miss_names_the_nearest_elements() {
        let read = read(vec![element(1, "AXButton", "Save Draft")], "");
        let spec = ElementSpec {
            role: Some("button".into()),
            name: Some("Submit".into()),
            ..Default::default()
        };
        let miss = pick(&read, &spec).unwrap_err();
        assert_eq!(miss.code, "not_found");
        assert!(
            miss.message
                .contains("nearest: [1] AXButton \"Save Draft\""),
            "{}",
            miss.message
        );
    }

    #[test]
    fn checks_read_values_static_text_and_absence() {
        let mut field = element(5, "AXTextField", "Email");
        field["value"] = json!("ada@example.com");
        let read = read(
            vec![field],
            "- [0] AXWindow \"Doc\"\n  - AXStaticText \"Saved to disk\"\n  - [5] AXTextField \"Email\"",
        );
        let check = |raw: Value| Check::parse(&raw, 0).unwrap();
        assert!(evaluate(
            &check(json!({"role": "textfield", "name": "email", "value": "ada@example.com"})),
            &read
        )
        .is_ok());
        let wrong = evaluate(&check(json!({"name": "Email", "value": "bob"})), &read).unwrap_err();
        assert_eq!(wrong.code, "unexpected_state");
        assert!(
            wrong.message.contains("value=\"ada@example.com\""),
            "{}",
            wrong.message
        );
        assert!(evaluate(&check(json!({"text": "saved to disk"})), &read).is_ok());
        assert!(evaluate(&check(json!({"text": "Error", "gone": true})), &read).is_ok());
        let still = evaluate(&check(json!({"text": "Saved", "gone": true})), &read).unwrap_err();
        assert_eq!(still.code, "still_present");
    }

    #[test]
    fn selected_falls_back_to_a_checkbox_value() {
        let mut checkbox = element(3, "AXCheckBox", "Accept terms");
        checkbox["value"] = json!("1");
        // As Chromium reports it: checked, but not list-selected.
        checkbox["selected"] = json!(false);
        let read = read(vec![checkbox], "");
        let check = Check::parse(
            &json!({"role": "checkbox", "name": "Accept terms", "selected": true}),
            0,
        )
        .unwrap();
        assert!(evaluate(&check, &read).is_ok());
        let check = Check::parse(
            &json!({"role": "checkbox", "name": "Accept terms", "selected": false}),
            0,
        )
        .unwrap();
        assert_eq!(
            evaluate(&check, &read).unwrap_err().code,
            "unexpected_state"
        );
    }

    #[test]
    fn field_checks_fall_back_to_display_rows() {
        let mut structured = element(5, "AXStaticText", "Result");
        structured["value"] = json!("wrong");
        let read = read(vec![structured], "- AXStaticText \"Result\" = \"702\"");
        let check = |raw: Value| Check::parse(&raw, 0).unwrap();
        for raw in [
            json!({"role":"AXStaticText","name":"Result","value":"702"}),
            json!({"role":"AXStaticText","name":"Result","value_contains":"70"}),
        ] {
            assert!(evaluate(&check(raw.clone()), &read).is_ok(), "{raw}");
        }
        for raw in [
            json!({"role":"AXStaticText","name":"Result","value":"703"}),
            json!({"role":"AXStaticText","name":"Result","enabled":true}),
            json!({"role":"AXStaticText","name":"Result","enabled":false}),
            json!({"role":"AXStaticText","name":"Result","selected":false}),
            json!({"role":"AXStaticText","name":"Result","selected":true}),
        ] {
            assert!(evaluate(&check(raw.clone()), &read).is_err(), "{raw}");
        }
        let out_of_range = check(json!({
            "role":"AXStaticText","name":"Result","value":"702","nth":1
        }));
        assert!(evaluate(&out_of_range, &read).is_err());
    }

    #[test]
    fn role_only_display_rows_participate_in_presence_and_selected_state() {
        let read = read(
            vec![],
            "- AXStaticText \"Result\" = \"702\"\n- AXCheckBox \"Enabled\" = \"1\"",
        );
        let check = |raw: Value| Check::parse(&raw, 0).unwrap();
        assert!(evaluate(&check(json!({"role":"AXStaticText","value":"702"})), &read).is_ok());
        assert!(evaluate(&check(json!({"role":"AXStaticText","gone":true})), &read).is_err());
        assert!(evaluate(&check(json!({"role":"AXCheckBox","selected":true})), &read).is_ok());
        assert!(evaluate(&check(json!({"role":"AXCheckBox","selected":false})), &read).is_err());
    }

    #[test]
    fn role_and_text_display_rows_participate_in_presence_and_fallback() {
        let read = read(vec![], "- AXStaticText \"Result\" = \"702\"");
        let check = |raw: Value| Check::parse(&raw, 0).unwrap();
        assert!(evaluate(
            &check(json!({"role":"AXStaticText","text":"Result","value":"702"})),
            &read
        )
        .is_ok());
        assert!(evaluate(
            &check(json!({"role":"AXStaticText","text":"Result","value":"703"})),
            &read
        )
        .is_err());
        assert!(evaluate(
            &check(json!({"role":"AXStaticText","text":"Result","gone":true})),
            &read
        )
        .is_err());
    }

    #[test]
    fn check_parsing_rejects_unclear_predicates() {
        for (raw, needle) in [
            (json!({}), "needs `role`, `name` or `text`"),
            (json!({"name": "x", "bogus": 1}), "unknown field `bogus`"),
            (
                json!({"name": "x", "gone": true, "value": "y"}),
                "cannot be combined",
            ),
            (json!({"text": "x", "value": "y"}), "need `role` or `name`"),
            (json!({"name": "x", "timeout_ms": 60000}), "timeout_ms"),
            (json!({"name": "x", "label": "y"}), "not both"),
        ] {
            let error = Check::parse(&raw, 0).unwrap_err();
            assert!(error.contains(needle), "{raw}: {error}");
        }
    }

    #[test]
    fn selector_keys_leave_the_action_arguments() {
        let mut args =
            json!({"role": "button", "name": "Go", "app": "Safari", "pid": 3, "count": 2})
                .as_object()
                .cloned()
                .unwrap();
        let (window, element) = take_selector(&mut args).unwrap();
        assert_eq!(window.app.as_deref(), Some("Safari"));
        assert_eq!(window.pid, Some(3));
        assert_eq!(element.name.as_deref(), Some("Go"));
        assert_eq!(args.keys().collect::<Vec<_>>(), ["count", "pid"]);
    }
}
