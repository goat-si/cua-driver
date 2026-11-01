//! AX tree walker: produces the treeMarkdown string and element cache.
//!
//! Format (matching libs/cua-driver exactly):
//!   `INDENT- [N] AXRole "Title" [value="..." actions=[...]]`
//!   `INDENT- AXStaticText = "value"`  (non-indexed)
//!
//! Rules (from cua-driver reference):
//! - An element is addressable (gets an index) when it has ≥1 action name or
//!   exposes a writable AXValue control surface.
//! - Non-actionable leaf nodes with a value are rendered as `AXRole = "value"`.
//! - AXStaticText with no title/value is omitted.
//! - Dense native rows may be read after the rest of the window; deferred chunks
//!   are spliced back and element indices renumbered to document order.

use super::bindings::*;
use super::content_attrs::ContentAttrs;
use super::dense_rows::{
    self, rows_windowed, with_dense_rows_windowed, ChildPlan, Chunk, DeferredRow,
};
use super::launch::{is_still_launching, LaunchStep, LaunchWait};
#[cfg(test)]
use super::snapshot::RetainedElement;
use super::tree_source::{AxTreeSource, Read};
use super::window_scope::{
    decide_window_scope, is_window_like_role, ScopeDecision, TopLevelCandidate, WindowScope,
};
use core_foundation::{
    base::{CFEqual, CFGetTypeID, CFRelease, CFRetain, CFTypeRef, TCFType},
    string::CFString,
};
use cua_driver_contract::{IncompleteReason, IncompleteReasonDetail, IncompleteReasonKind};
use cua_driver_core::snapshot_compaction::{
    compact_tab_strip, fold_actionless_text_subtrees, format_omitted_tabs_line,
    is_secure_text_field, redact_if_secure, CompactableNode, FieldSecurityMeta, TabChildMeta,
    TabStripCompaction, TAB_STRIP_COMPACTION_THRESHOLD,
};
use cua_driver_core::walk_budget::{WalkBudget, WalkOutcome};
use std::time::Instant;

/// Default maximum depth for AX tree walks. Deep menus and complex web views
/// can nest deeply; 25 covers realistic app chrome without exploding on
/// pathological trees (mirrors Swift reference implementation).
///
/// Callers can override per-call via `walk_tree`'s `max_depth` parameter to
/// trade fidelity for context-window budget on AX-heavy apps (Electron,
/// Obsidian, large web apps — issue #22865).
pub const DEFAULT_MAX_DEPTH: usize = 25;

/// Default maximum total nodes visited during a single AX walk. Chromium-family
/// apps (Arc, VS Code, Chrome) can expose thousands of nodes; capping at 2 000
/// keeps the walk bounded while still covering realistic app chrome.
/// When the cap is hit the walk stops early and the partial tree is returned
/// with a warning line appended (mirrors Swift reference implementation).
///
/// Callers can override per-call via `walk_tree`'s `max_elements` parameter
/// (issue #22865).
pub const DEFAULT_MAX_ELEMENTS: usize = 2_000;

/// Bound each native AX request. Tokio cannot cancel a blocked
/// `AXUIElementCopyAttributeValue` after `spawn_blocking` starts, so the native
/// messaging timeout is what keeps an unresponsive app from retaining a worker
/// indefinitely.
const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 2.0;

pub(crate) unsafe fn set_messaging_timeout(element: AXUIElementRef) {
    let _ = AXUIElementSetMessagingTimeout(element, AX_MESSAGING_TIMEOUT_SECONDS);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedTextRange {
    pub location: isize,
    pub length: isize,
}

#[repr(C)]
struct CfRange {
    location: isize,
    length: isize,
}

pub(crate) unsafe fn read_selected_text_range(
    element: AXUIElementRef,
) -> Option<SelectedTextRange> {
    let attr = CFString::new("AXSelectedTextRange");
    let mut value: CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if error != kAXErrorSuccess || value.is_null() {
        if !value.is_null() {
            CFRelease(value);
        }
        return None;
    }
    if CFGetTypeID(value) != AXValueGetTypeID() {
        CFRelease(value);
        return None;
    }
    let mut range = CfRange {
        location: 0,
        length: 0,
    };
    let decoded = AXValueGetValue(
        value as _,
        kAXValueCFRangeType,
        &mut range as *mut CfRange as *mut _,
    );
    CFRelease(value);
    if !decoded || range.location < 0 || range.length < 0 {
        return None;
    }
    Some(SelectedTextRange {
        location: range.location,
        length: range.length,
    })
}

fn optional_text_attribute(element: AXUIElementRef, name: &str) -> Option<Option<String>> {
    let attr = CFString::new(name);
    let mut value: CFTypeRef = std::ptr::null();
    let error =
        unsafe { AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value) };
    if error == kAXErrorNoValue || error == kAXErrorAttributeUnsupported {
        if !value.is_null() {
            unsafe { CFRelease(value) };
        }
        return Some(None);
    }
    if error != kAXErrorSuccess
        || value.is_null()
        || unsafe { CFGetTypeID(value) } != CFString::type_id()
    {
        if !value.is_null() {
            unsafe { CFRelease(value) };
        }
        return None;
    }
    let text = unsafe { CFString::wrap_under_create_rule(value as _) }.to_string();
    Some(Some(text))
}

pub(crate) unsafe fn selected_text_if_allowed(
    element: AXUIElementRef,
) -> (Option<String>, Option<SelectedTextRange>) {
    let attributes = [
        optional_text_attribute(element, "AXRole"),
        optional_text_attribute(element, "AXSubrole"),
        optional_text_attribute(element, "AXTitle"),
        optional_text_attribute(element, "AXDescription"),
        optional_text_attribute(element, "AXPlaceholderValue"),
        optional_text_attribute(element, "AXIdentifier"),
        optional_text_attribute(element, "AXHelp"),
    ];
    if attributes.iter().any(Option::is_none) {
        return (None, None);
    }
    let value = |index: usize| attributes[index].as_ref().unwrap().as_deref();
    let Some(role) = value(0).filter(|role| !role.trim().is_empty()) else {
        return (None, None);
    };
    if !is_selection_text_role(role)
        || is_secure_text_field(&FieldSecurityMeta {
            role,
            subrole: value(1),
            title: value(2),
            description: value(3),
            placeholder: value(4),
            identifier: value(5),
            help: value(6),
            is_password_flag: false,
        })
    {
        return (None, None);
    }
    let selected = copy_string_attr(element, "AXSelectedText").filter(|text| !text.is_empty());
    (selected, read_selected_text_range(element))
}

/// The roles [`selected_text_if_allowed`] reads a selection from.
pub(crate) fn is_selection_text_role(role: &str) -> bool {
    matches!(role, "AXTextField" | "AXTextArea" | "AXComboBox")
}

/// A single node in the AX tree.
#[derive(Clone)]
pub struct AXNode {
    /// Internal marker and structurally owned label for row-like items.
    pub(crate) owned_row_label: Option<Option<String>>,
    pub(crate) canonical: super::vocabulary::CanonicalEvidence,
    /// 0-based index (Some = actionable, None = non-actionable display-only node)
    pub element_index: Option<usize>,
    pub role: String,
    /// AXTitle — shown as `"title"` in the tree line.
    pub title: Option<String>,
    /// AXValue — shown as `= "value"` in the tree line.
    pub value: Option<String>,
    /// AXDescription — shown as `(description)` in the tree line.
    /// Kept separate from `title` so `_find_calc_button("2")` can find
    /// Calculator buttons where AXTitle="" but AXDescription="2".
    pub description: Option<String>,
    pub identifier: Option<String>,
    pub help: Option<String>,
    pub actions: Vec<String>,
    /// The raw AXUIElementRef pointer value, for caching.
    pub element_ptr: usize,
    /// Depth in the rendered markdown tree (matches the indent level used in
    /// `tree_markdown`). Layout containers AXScrollArea/AXGroup collapse so
    /// children share the parent's depth.
    pub depth: usize,
    /// `element_index` of the nearest actionable ancestor, if any. Walks the
    /// rendered tree (so it skips collapsed layout containers).
    pub parent_element_index: Option<usize>,
    /// Screen-coordinate bounding rect `[x, y, width, height]` captured at
    /// walk time. `None` when AX didn't report a usable position+size.
    pub frame: Option<[f64; 4]>,
    /// AXValue coerced to a string for ALL CF types (CFNumber → "8",
    /// CFBoolean → "1"/"0", CFString as-is). Kept separate from `value`
    /// (string-only) so tree_markdown and the has_content gate — both of
    /// which read `value` — stay byte-identical; only the structured
    /// `elements` array consumes this.
    pub value_state: Option<String>,
    /// AXValueDescription — human-readable value form (e.g. "8 dB").
    pub value_description: Option<String>,
    /// AXMinValue / AXMaxValue for range controls (sliders, steppers).
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    /// AXEnabled. `None` when the app doesn't report the attribute.
    pub enabled: Option<bool>,
    /// AXSelected. `None` when the app doesn't report the attribute.
    pub selected: Option<bool>,
    /// True when this node is an AX web-document root or descends from one.
    /// This trust marker is independent of actionable ancestry because
    /// AXWebArea is commonly non-actionable and therefore has no element index.
    pub in_web_content: bool,
}

impl std::fmt::Debug for AXNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AXNode")
            .field("owned_row_label", &self.owned_row_label)
            .field("element_index", &self.element_index)
            .field("role", &self.role)
            .field("title", &self.title)
            .field("value", &self.value)
            .field("description", &self.description)
            .field("identifier", &self.identifier)
            .field("help", &self.help)
            .field("actions", &self.actions)
            .field("element_ptr", &self.element_ptr)
            .field("depth", &self.depth)
            .field("parent_element_index", &self.parent_element_index)
            .field("frame", &self.frame)
            .field("value_state", &self.value_state)
            .field("value_description", &self.value_description)
            .field("min_value", &self.min_value)
            .field("max_value", &self.max_value)
            .field("enabled", &self.enabled)
            .field("selected", &self.selected)
            .field("in_web_content", &self.in_web_content)
            .finish()
    }
}

#[derive(Default)]
struct ControlState {
    value_state: Option<String>,
    value_description: Option<String>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    enabled: Option<bool>,
    selected: Option<bool>,
}

fn read_control_state_if_actionable<F>(is_actionable: bool, read: F) -> ControlState
where
    F: FnOnce() -> ControlState,
{
    if is_actionable {
        read()
    } else {
        ControlState::default()
    }
}

fn role_supports_value_addressing(role: &str) -> bool {
    matches!(
        role,
        "AXTextField"
            | "AXTextArea"
            | "AXSecureTextField"
            | "AXComboBox"
            | "AXSlider"
            | "AXStepper"
            | "AXCheckBox"
            | "AXRadioButton"
    )
}

impl CompactableNode for AXNode {
    fn element_index(&self) -> Option<usize> {
        self.element_index
    }
    fn role(&self) -> &str {
        &self.role
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
        self.value.as_deref()
    }
    fn set_value(&mut self, value: Option<String>) {
        // Compaction sees rendered depth after omitted wrappers. Only copy a
        // summary onto an item when the walk independently proved ownership.
        if let Some(owned) = &self.owned_row_label {
            if self.value.is_some() || value.as_deref() != owned.as_deref() {
                return;
            }
        }
        self.value = value;
    }
    fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
    fn format_line(&self) -> String {
        format_node_line(self)
    }
}

/// Control roles that are pressable even when they list no AX actions.
/// KakaoTalk's sidebar tabs are `AXButton`s with only `AXHelp` and
/// `AXIdentifier` and an empty action list, yet `AXPress` on them switches
/// the tab; without an index an agent could only reach them by pixel.
fn is_actionless_control_role(role: &str) -> bool {
    matches!(
        role,
        "AXButton"
            | "AXRadioButton"
            | "AXCheckBox"
            | "AXPopUpButton"
            | "AXMenuButton"
            | "AXDisclosureTriangle"
            | "AXLink"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LabelPath {
    Inactive,
    Open(usize),
    Cell(usize, usize),
    Blocked,
}

pub(super) fn next_label_path(
    parent: LabelPath,
    parent_role: Option<&str>,
    child_role: Option<&str>,
) -> LabelPath {
    match parent {
        LabelPath::Inactive => LabelPath::Inactive,
        LabelPath::Blocked => LabelPath::Blocked,
        LabelPath::Open(owner) => match child_role {
            None | Some("AXUnknown") => LabelPath::Blocked,
            Some("AXCell") if parent_role == Some("AXRow") => LabelPath::Open(owner),
            Some("AXRow" | "AXCell" | "AXOutlineRow" | "AXListItem" | "AXImage") => {
                LabelPath::Blocked
            }
            Some(_) => LabelPath::Open(owner),
        },
        LabelPath::Cell(cell, row) => match child_role {
            None
            | Some("AXUnknown" | "AXRow" | "AXCell" | "AXOutlineRow" | "AXListItem" | "AXImage") => {
                LabelPath::Blocked
            }
            Some(_) => LabelPath::Cell(cell, row),
        },
    }
}

fn row_label_candidate<'a>(
    path: LabelPath,
    role: &str,
    texts: [Option<&'a str>; 3],
) -> Option<&'a str> {
    if !matches!(path, LabelPath::Open(_) | LabelPath::Cell(_, _))
        || !matches!(role, "AXStaticText" | "AXTextField")
    {
        return None;
    }
    texts
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|text| !text.is_empty())
}

pub(super) fn initialize_owned_row_label(node: &mut AXNode) {
    node.owned_row_label =
        matches!(node.role.as_str(), "AXRow" | "AXCell" | "AXOutlineRow").then_some(None);
}

pub(super) fn child_label_path(
    path: LabelPath,
    role: &str,
    parent_role: Option<&str>,
    ptr: usize,
) -> LabelPath {
    match (role, parent_role, path) {
        ("AXCell", Some("AXRow"), LabelPath::Open(row)) => LabelPath::Cell(ptr, row),
        ("AXRow" | "AXCell" | "AXOutlineRow", _, _) => LabelPath::Open(ptr),
        _ => path,
    }
}

pub(super) fn collect_label_claim(
    claims: &mut Vec<(usize, String)>,
    path: LabelPath,
    role: &str,
    texts: [Option<&str>; 3],
) {
    if let Some(label) = row_label_candidate(path, role, texts) {
        match path {
            LabelPath::Open(owner) => claims.push((owner, label.to_owned())),
            LabelPath::Cell(cell, row) => {
                claims.push((cell, label.to_owned()));
                claims.push((row, label.to_owned()));
            }
            _ => {}
        }
    }
}

fn selectable_row_eligible(
    role: &str,
    parent_role: Option<&str>,
    actions_empty: bool,
    value_settable: bool,
    probe: impl FnOnce() -> super::write_outcome::Settable,
) -> bool {
    role == "AXRow"
        && parent_role.is_some_and(|role| dense_rows::DENSE_COLLECTION_ROLES.contains(&role))
        && actions_empty
        && !value_settable
        && probe() == super::write_outcome::Settable::Yes
}

fn selected_probe_before_deadline(
    expired: bool,
    probe: impl FnOnce() -> super::write_outcome::Settable,
    skipped: impl FnOnce(),
) -> super::write_outcome::Settable {
    if expired {
        skipped();
        super::write_outcome::Settable::Unknown(kAXErrorCannotComplete)
    } else {
        probe()
    }
}

fn selected_probe_observed(
    owner: Option<usize>,
    expired: bool,
    probe: impl FnOnce() -> super::write_outcome::Settable,
    skipped: impl FnOnce(),
) -> super::evidence::Observed<super::write_outcome::Settable> {
    use super::evidence::{EvidenceStatus, Observed, ReadEvidence, ReadSite};
    use super::tree_source::ReadFailure;
    use super::write_outcome::Settable;
    let legacy = selected_probe_before_deadline(expired, probe, skipped);
    let status = if expired {
        EvidenceStatus::DeadlineExpired
    } else {
        match legacy {
            Settable::Yes | Settable::No => EvidenceStatus::Known,
            Settable::Unknown(error) => EvidenceStatus::Unknown(ReadFailure::Ax(error)),
        }
    };
    Observed {
        legacy,
        evidence: vec![ReadEvidence {
            owner,
            site: ReadSite::SelectedSettable,
            status,
            attempt: 0,
            listing: None,
        }],
    }
}

pub(super) fn apply_owned_label_claims(nodes: &mut [AXNode], claims: Vec<(usize, String)>) {
    for (owner, label) in claims {
        if let Some(node) = nodes
            .iter_mut()
            .find(|node| node.element_ptr == owner && node.owned_row_label.is_some())
        {
            if let Some(slot @ None) = node.owned_row_label.as_mut() {
                *slot = Some(label);
            }
        }
    }
}

fn fold_with_owned_items(lines: &mut Vec<(usize, String)>, nodes: &mut Vec<AXNode>) {
    // The core folder only sees rendered depth; a missing item wrapper can
    // make unrelated text appear to belong to an unindexed row or cell.
    // Protect those owners during folding so their original children remain.
    let protected: std::collections::HashSet<usize> = nodes
        .iter_mut()
        .filter_map(|node| {
            if node.element_index.is_none() && node.owned_row_label.is_some() {
                node.element_index = Some(usize::MAX);
                Some(node.element_ptr)
            } else {
                None
            }
        })
        .collect();
    fold_actionless_text_subtrees(lines, nodes);
    for node in nodes {
        if node.element_index == Some(usize::MAX) && protected.contains(&node.element_ptr) {
            node.element_index = None;
        }
    }
}

fn is_addressable(
    role: &str,
    actions_present: bool,
    value_settable: bool,
    enabled: Option<bool>,
) -> bool {
    (actions_present || value_settable) && (role != "AXMenuItem" || enabled != Some(false))
}

fn window_chrome_button_description<F>(
    role: &str,
    visible_title: &str,
    visible_description: &str,
    read_subrole: F,
) -> Option<&'static str>
where
    F: FnOnce() -> Option<String>,
{
    if role != "AXButton" || !visible_title.is_empty() || !visible_description.is_empty() {
        return None;
    }
    match read_subrole()?.as_str() {
        "AXCloseButton" => Some("close button"),
        "AXMinimizeButton" => Some("minimize button"),
        "AXZoomButton" => Some("zoom button"),
        "AXFullScreenButton" => Some("full screen button"),
        _ => None,
    }
}

pub struct TreeWalkResult {
    pub tree_markdown: String,
    pub nodes: Vec<AXNode>,
    /// True when the walk was cut short by its node or time budget.
    pub truncated: bool,
    /// True when the walk passed over a node or subtree without visiting it
    /// for a reason other than its budget: the depth cap with children below,
    /// an `AXChildren` read that failed, a node whose attributes could not be
    /// read, omitted tabs, off-screen rows or menus left unwalked, or web
    /// content excluded from this walk.
    pub nodes_skipped: bool,
    /// Why and where the walk stopped (see [`cua_driver_core::walk_budget`]).
    pub walk: WalkOutcome,
    /// Whether the requested `window_id` actually resolved to an AX surface,
    /// and if not, why. `None` when no `window_id` was requested.
    ///
    /// Issue #2237: without this, an unresolvable id was indistinguishable
    /// from a clean snapshot — callers had no way to tell that the tree they
    /// were handed belonged to a different surface. Any variant other than
    /// [`WindowScope::Matched`] comes with an EMPTY walk, so `nodes` never
    /// describes a window other than the requested one.
    pub window_scope: Option<WindowScope>,
    /// Offscreen rows a windowed read left unwalked, one entry per summary
    /// line. Empty when every row was walked.
    pub offscreen_rows: Vec<super::dense_rows::OffscreenRows>,
}

#[cfg(test)]
impl TreeWalkResult {
    /// A finished walk of `nodes`, for tests of what callers do with it.
    pub(crate) fn for_test(tree_markdown: String, nodes: Vec<AXNode>) -> Self {
        Self {
            tree_markdown,
            nodes,
            truncated: false,
            nodes_skipped: false,
            walk: cua_driver_core::walk_budget::WalkBudget::nodes_only(usize::MAX).outcome(),
            window_scope: None,
            offscreen_rows: Vec::new(),
        }
    }
}

#[cfg(test)]
impl AXNode {
    /// A bare node holding `element_ptr`, for tests of retain ownership.
    pub(crate) fn for_test(element_index: Option<usize>, element_ptr: usize) -> Self {
        Self {
            owned_row_label: None,
            canonical: Default::default(),
            element_index,
            role: "AXLink".into(),
            title: None,
            value: None,
            description: None,
            identifier: None,
            help: None,
            actions: vec![],
            element_ptr,
            depth: 0,
            parent_element_index: None,
            frame: None,
            value_state: None,
            value_description: None,
            min_value: None,
            max_value: None,
            enabled: None,
            selected: None,
            in_web_content: false,
        }
    }
}

/// Walk the AX tree of `pid`, optionally filtered to a specific window.
///
/// `window_id` — when Some, only the AXWindow matching that CGWindowID is
/// walked (plus non-window children like the menu bar). When None, all
/// top-level children are walked.
///
/// Key background-app fix: at the application root we union `AXChildren`
/// and `AXWindows`. macOS only puts windows in `AXChildren` when the app
/// is frontmost; `AXWindows` returns the window list regardless of focus
/// state. Without this union, Safari / any backgrounded app returns an
/// empty tree.
///
/// # Safety
/// Calls macOS AX API. Must be called on a thread that has a CF run loop.
pub fn walk_tree(pid: i32, window_id: Option<u32>, query: Option<&str>) -> TreeWalkResult {
    walk_tree_bounded(
        pid,
        window_id,
        query,
        DEFAULT_MAX_ELEMENTS,
        DEFAULT_MAX_DEPTH,
    )
}

/// Walk the AX tree with caller-supplied caps. See [`walk_tree`] for the
/// common case (defaults apply). `max_elements`/`max_depth` clamp the
/// rendered tree breadth-wise (DFS truncated when the element counter hits
/// the cap) and depth-wise (nodes whose markdown indent would exceed the cap
/// are omitted). Markdown and the `nodes` vec are truncated identically.
///
/// Issue #22865: caps protect against Electron / Obsidian / large web apps
/// that produce 10k+ element trees and blow context windows.
pub fn walk_tree_bounded(
    pid: i32,
    window_id: Option<u32>,
    query: Option<&str>,
    max_elements: usize,
    max_depth: usize,
) -> TreeWalkResult {
    walk_tree_budgeted(
        pid,
        window_id,
        query,
        max_depth,
        WalkBudget::nodes_only(max_elements),
    )
}

pub(crate) fn walk_native_chrome_bounded(
    pid: i32,
    window_id: u32,
    max_elements: usize,
) -> TreeWalkResult {
    walk_tree_with_web_content(
        pid,
        Some(window_id),
        None,
        DEFAULT_MAX_DEPTH,
        WalkBudget::nodes_only(max_elements),
        false,
    )
}

/// [`walk_tree_bounded`] under a caller's [`WalkBudget`]: the walk also stops
/// when the budget's `timeout_ms` runs out, returning the partial tree. Each
/// AX call is bounded by the per-element messaging timeout, so the walk
/// overruns the deadline by at most one attribute read.
pub fn walk_tree_budgeted(
    pid: i32,
    window_id: Option<u32>,
    query: Option<&str>,
    max_depth: usize,
    budget: WalkBudget,
) -> TreeWalkResult {
    walk_tree_with_web_content(pid, window_id, query, max_depth, budget, true)
}

fn walk_tree_with_web_content(
    pid: i32,
    window_id: Option<u32>,
    query: Option<&str>,
    max_depth: usize,
    budget: WalkBudget,
    include_web_content: bool,
) -> TreeWalkResult {
    walk_tree_with_web_content_observed(
        pid,
        window_id,
        query,
        max_depth,
        budget,
        include_web_content,
    )
    .legacy
}

pub(crate) fn walk_tree_with_web_content_observed(
    pid: i32,
    window_id: Option<u32>,
    query: Option<&str>,
    max_depth: usize,
    budget: WalkBudget,
    include_web_content: bool,
) -> CompletedWalk {
    walk_with_environment(
        &super::tree_source::native::NativeSource,
        &NativeWalkEnvironment,
        WalkRequest {
            pid,
            window_id,
            query,
            max_depth,
            include_web_content,
        },
        budget,
    )
}

struct WalkRequest<'a> {
    pid: i32,
    window_id: Option<u32>,
    query: Option<&'a str>,
    max_depth: usize,
    include_web_content: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WalkRequestMode {
    Application,
    Window,
}

/// Internal completion metadata; it is never attached to an AX node or output.
#[allow(dead_code)]
struct CompletionEvidence {
    request_mode: WalkRequestMode,
    scope_observations: Vec<super::evidence::ReadEvidence>,
    provider_observations: Vec<super::evidence::ReadEvidence>,
    discovery_observations: Vec<super::evidence::ReadEvidence>,
    read_evidence: Vec<super::evidence::ReadEvidence>,
    final_deadline_sample: Option<bool>,
}

#[allow(dead_code)]
pub(crate) struct CompletedWalk {
    legacy: TreeWalkResult,
    completion: CompletionEvidence,
}

fn walk_with_environment<S, E>(
    source: &S,
    environment: &E,
    request: WalkRequest<'_>,
    budget: WalkBudget,
) -> CompletedWalk
where
    S: AxTreeSource,
    E: WalkOrchestration<S>,
{
    use super::evidence::ReadSite;
    let request_mode = if request.window_id.is_some() {
        WalkRequestMode::Window
    } else {
        WalkRequestMode::Application
    };
    let scope = super::evidence::EvidenceScope::start();
    let (result, final_deadline_sample) =
        walk_tree_with_web_content_inner(source, environment, request, budget);
    let observed = scope.finish(result);
    let scope_observations = observed
        .evidence
        .iter()
        .copied()
        .filter(|e| matches!(e.site, ReadSite::ScopeCandidate(_)))
        .collect();
    let provider_observations = observed
        .evidence
        .iter()
        .copied()
        .filter(|e| {
            matches!(
                e.site,
                ReadSite::ProviderEnablement
                    | ReadSite::ProviderCachedEnablement
                    | ReadSite::ProviderOptIn
            )
        })
        .collect();
    let discovery_observations = observed
        .evidence
        .iter()
        .copied()
        .filter(|e| {
            matches!(
                e.site,
                ReadSite::MenuWindowList
                    | ReadSite::MenuDiscoveryDeadline
                    | ReadSite::MenuWindowCap
                    | ReadSite::MenuResolution
                    | ReadSite::MenuFrame
                    | ReadSite::MenuOwnership
                    | ReadSite::RecoveryHit
                    | ReadSite::RecoveryDeadline
                    | ReadSite::RecoveryParent
                    | ReadSite::RecoveryWindowId
                    | ReadSite::RecoveryRoot
                    | ReadSite::RecoverySheetRole
                    | ReadSite::TopLevelWindows
                    | ReadSite::TopLevelWindowsMember
                    | ReadSite::TopLevelRemoteWindow
            )
        })
        .collect();
    CompletedWalk {
        legacy: observed.legacy,
        completion: CompletionEvidence {
            request_mode,
            scope_observations,
            provider_observations,
            discovery_observations,
            read_evidence: observed.evidence,
            final_deadline_sample,
        },
    }
}

/// Supplies native setup observations to shared walk code.
trait AxWalkEnvironment {
    type Application: Copy;
    type Element: Copy;

    fn create_application(&self, pid: i32) -> Option<Self::Application>;
    fn set_application_timeout(&self, application: &Self::Application);
    fn application_key(&self, application: &Self::Application) -> usize;
    fn children(
        &self,
        application: Self::Application,
    ) -> super::evidence::Observed<Read<Vec<Self::Element>>>;
    fn windows(
        &self,
        application: Self::Application,
        pid: i32,
        window_id: Option<u32>,
    ) -> super::evidence::Observed<Vec<Self::Element>>;
    fn same(&self, left: Self::Element, right: Self::Element) -> bool;
    fn release(&self, element: Self::Element);
    fn retain(&self, element: Self::Element);
    fn element_key(&self, element: Self::Element) -> usize;
    fn set_element_timeout(&self, element: Self::Element);
    fn window_id(&self, element: Self::Element) -> Option<u32>;
    fn window_bounds(&self, window_id: u32) -> Option<[f64; 4]>;
    fn hit(&self, pid: i32, x: f64, y: f64) -> super::evidence::Observed<Option<Self::Element>>;
    fn parent(&self, element: Self::Element) -> Option<Self::Element>;
    fn now(&self) -> Instant;
    /// Observe the same walk budget used by the native walker. Fake
    /// environments can couple these observations to their controlled clock.
    fn budget_remaining(&self, budget: &WalkBudget) -> Option<std::time::Duration> {
        budget.remaining()
    }
    fn budget_expired(&self, budget: &WalkBudget) -> bool {
        budget.expired()
    }
    fn enable(&self, pid: i32, application: Self::Application) -> super::evidence::Observed<()>;
    fn scope_candidate(
        &self,
        element: Self::Element,
    ) -> super::evidence::Observed<TopLevelCandidate>;
    fn window_owner(&self, pid: i32, window_id: u32) -> crate::windows::WindowOwner;
    fn sheet_role(&self, element: Self::Element) -> Read<String>;
    fn sheet_children(
        &self,
        element: Self::Element,
    ) -> super::evidence::Observed<Read<Vec<Self::Element>>>;
    fn is_still_launching(&self, pid: i32) -> bool;
    fn pause(&self, duration: std::time::Duration);
}

struct NativeWalkEnvironment;

/// Expose an existing orchestration retain credit to the attribute walker.
/// The borrow itself must neither acquire nor release a credit.
trait WalkElementBorrow<S: AxTreeSource> {
    type Raw: Copy;

    fn with_walk_element<R>(
        &self,
        source: &S,
        raw: Self::Raw,
        f: impl FnOnce(&S, &S::Element) -> R,
    ) -> R;
}

type MenuObservation<E> = (E, Option<[f64; 4]>, Option<super::context_menus::MenuOwner>);

/// Operations whose raw results are used by the shared walk orchestration.
trait WalkOrchestration<S: AxTreeSource>:
    AxWalkEnvironment + WalkElementBorrow<S, Raw = <Self as AxWalkEnvironment>::Element>
{
    fn release_application(&self, application: Self::Application);
    fn menu_frames(&self, pid: i32) -> super::evidence::Observed<Option<Vec<[f64; 4]>>>;
    fn menu_in_window(
        &self,
        pid: i32,
        frame: [f64; 4],
        deadline: Instant,
    ) -> super::evidence::Observed<Option<MenuObservation<Self::Element>>>;
}

impl WalkOrchestration<super::tree_source::native::NativeSource> for NativeWalkEnvironment {
    fn release_application(&self, application: Self::Application) {
        unsafe { CFRelease(application as CFTypeRef) }
    }
    fn menu_frames(&self, pid: i32) -> super::evidence::Observed<Option<Vec<[f64; 4]>>> {
        menu_list_observed(super::context_menus::open_menu_window_frames(pid))
    }
    fn menu_in_window(
        &self,
        pid: i32,
        frame: [f64; 4],
        deadline: Instant,
    ) -> super::evidence::Observed<Option<MenuObservation<Self::Element>>> {
        menu_resolution_observed(
            unsafe { super::context_menus::menu_in_window(pid, frame, deadline) },
            |element| *element as usize,
        )
    }
}

fn menu_list_observed<T>(legacy: Option<T>) -> super::evidence::Observed<Option<T>> {
    use super::evidence::{EvidenceStatus, Observed, ReadEvidence, ReadSite};
    let status = if legacy.is_some() {
        EvidenceStatus::Known
    } else {
        EvidenceStatus::Unavailable
    };
    Observed {
        legacy,
        evidence: vec![ReadEvidence {
            owner: None,
            site: ReadSite::MenuWindowList,
            status,
            attempt: 0,
            listing: None,
        }],
    }
}

fn menu_resolution_observed<T>(
    legacy: Option<MenuObservation<T>>,
    key: impl FnOnce(&T) -> usize,
) -> super::evidence::Observed<Option<MenuObservation<T>>> {
    use super::evidence::{EvidenceStatus, Observed, ReadEvidence, ReadSite};
    let mut evidence = vec![ReadEvidence {
        owner: legacy.as_ref().map(|(element, _, _)| key(element)),
        site: ReadSite::MenuResolution,
        status: if legacy.is_some() {
            EvidenceStatus::Known
        } else {
            EvidenceStatus::Unavailable
        },
        attempt: 0,
        listing: None,
    }];
    if let Some((_, frame, owner)) = &legacy {
        evidence.push(ReadEvidence {
            owner: evidence[0].owner,
            site: ReadSite::MenuFrame,
            status: if frame.is_some() {
                EvidenceStatus::Known
            } else {
                EvidenceStatus::Unavailable
            },
            attempt: 0,
            listing: None,
        });
        evidence.push(ReadEvidence {
            owner: evidence[0].owner,
            site: ReadSite::MenuOwnership,
            status: if owner.is_some() {
                EvidenceStatus::Known
            } else {
                EvidenceStatus::Unavailable
            },
            attempt: 0,
            listing: None,
        });
    }
    Observed { legacy, evidence }
}

fn menu_discovery_deadline(
    now: Instant,
    truncated: bool,
    remaining: Option<std::time::Duration>,
) -> Option<Instant> {
    let deadline = super::context_menus::discovery_deadline(now, truncated, remaining);
    super::evidence::record_status(
        None,
        super::evidence::ReadSite::MenuDiscoveryDeadline,
        if deadline.is_some() {
            super::evidence::EvidenceStatus::Known
        } else if truncated {
            super::evidence::EvidenceStatus::Unavailable
        } else {
            super::evidence::EvidenceStatus::DeadlineExpired
        },
    );
    deadline
}

impl WalkElementBorrow<super::tree_source::native::NativeSource> for NativeWalkEnvironment {
    type Raw = AXUIElementRef;

    fn with_walk_element<R>(
        &self,
        source: &super::tree_source::native::NativeSource,
        raw: Self::Raw,
        f: impl FnOnce(
            &super::tree_source::native::NativeSource,
            &super::tree_source::NativeElement,
        ) -> R,
    ) -> R {
        // The orchestration retains `raw` until the callback has returned.
        unsafe { super::tree_source::native::with_borrowed(raw, |_, element| f(source, element)) }
    }
}

impl AxWalkEnvironment for NativeWalkEnvironment {
    type Application = AXUIElementRef;
    type Element = AXUIElementRef;

    fn create_application(&self, pid: i32) -> Option<Self::Application> {
        let application = unsafe { AXUIElementCreateApplication(pid) };
        (!application.is_null()).then_some(application)
    }
    fn set_application_timeout(&self, application: &Self::Application) {
        unsafe { set_messaging_timeout(*application) }
    }
    fn application_key(&self, application: &Self::Application) -> usize {
        *application as usize
    }
    fn children(
        &self,
        application: Self::Application,
    ) -> super::evidence::Observed<Read<Vec<Self::Element>>> {
        unsafe { super::tree_source::native::children_observed_raw(application) }
    }
    fn windows(
        &self,
        application: Self::Application,
        pid: i32,
        window_id: Option<u32>,
    ) -> super::evidence::Observed<Vec<Self::Element>> {
        unsafe {
            match window_id {
                Some(wid) => copy_ax_windows_including_observed(application, pid, wid),
                None => copy_ax_windows_observed(application),
            }
        }
    }
    fn same(&self, left: Self::Element, right: Self::Element) -> bool {
        unsafe { CFEqual(left as CFTypeRef, right as CFTypeRef) != 0 }
    }
    fn release(&self, element: Self::Element) {
        unsafe { CFRelease(element as CFTypeRef) }
    }
    fn retain(&self, element: Self::Element) {
        unsafe {
            CFRetain(element as CFTypeRef);
        }
    }
    fn element_key(&self, element: Self::Element) -> usize {
        element as usize
    }
    fn set_element_timeout(&self, element: Self::Element) {
        unsafe { set_messaging_timeout(element) }
    }
    fn window_id(&self, element: Self::Element) -> Option<u32> {
        unsafe { ax_get_window_id(element) }
    }
    fn window_bounds(&self, window_id: u32) -> Option<[f64; 4]> {
        crate::windows::window_bounds_by_id(window_id)
            .map(|bounds| [bounds.x, bounds.y, bounds.width, bounds.height])
    }
    fn hit(&self, pid: i32, x: f64, y: f64) -> super::evidence::Observed<Option<Self::Element>> {
        unsafe { element_at_screen_position_observed(pid, x, y) }
    }
    fn parent(&self, element: Self::Element) -> Option<Self::Element> {
        unsafe { copy_element_attr(element, "AXParent") }
    }
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn enable(&self, pid: i32, application: Self::Application) -> super::evidence::Observed<()> {
        unsafe { super::enablement::ensure_chromium_ax_enabled_observed(pid, application) }
    }
    fn scope_candidate(
        &self,
        element: Self::Element,
    ) -> super::evidence::Observed<TopLevelCandidate> {
        unsafe {
            super::tree_source::native::with_borrowed(element, |source, child| {
                scope_candidate_with(source, child, || ax_get_window_id(element))
            })
        }
    }
    fn window_owner(&self, pid: i32, window_id: u32) -> crate::windows::WindowOwner {
        crate::windows::resolve_window_owner(pid, window_id)
    }
    fn sheet_role(&self, element: Self::Element) -> Read<String> {
        unsafe {
            super::tree_source::native::with_borrowed(element, |source, child| {
                source.text(child, "AXRole")
            })
        }
    }
    fn sheet_children(
        &self,
        element: Self::Element,
    ) -> super::evidence::Observed<Read<Vec<Self::Element>>> {
        unsafe { super::tree_source::native::children_observed_raw(element) }
    }
    fn is_still_launching(&self, pid: i32) -> bool {
        is_still_launching(pid)
    }
    fn pause(&self, duration: std::time::Duration) {
        std::thread::sleep(duration)
    }
}

fn create_application_with<E: AxWalkEnvironment>(
    environment: &E,
    pid: i32,
) -> super::evidence::Observed<Option<E::Application>> {
    use super::evidence::{EvidenceStatus, Observed, ReadEvidence, ReadSite};
    let legacy = environment.create_application(pid);
    if let Some(application) = &legacy {
        environment.set_application_timeout(application);
    }
    let owner = legacy
        .as_ref()
        .map(|application| environment.application_key(application));
    let status = if legacy.is_some() {
        EvidenceStatus::Known
    } else {
        EvidenceStatus::Unavailable
    };
    Observed {
        legacy,
        evidence: vec![ReadEvidence {
            owner,
            site: ReadSite::ApplicationRoot,
            status,
            attempt: 0,
            listing: None,
        }],
    }
}

fn walk_tree_with_web_content_inner<S, E>(
    source: &S,
    environment: &E,
    request: WalkRequest<'_>,
    mut budget: WalkBudget,
) -> (TreeWalkResult, Option<bool>)
where
    S: AxTreeSource,
    E: WalkOrchestration<S>,
{
    let WalkRequest {
        pid,
        window_id,
        query,
        max_depth,
        include_web_content,
    } = request;
    let ledger = SkipLedger::start();
    let mut nodes: Vec<AXNode> = Vec::new();
    let mut lines: Vec<(usize, String)> = Vec::new(); // (depth, line)
    let mut index_counter = 0usize;
    let mut deferred = Vec::new();
    let offscreen_rows;
    let mut batching_disabled = false;
    let window_scope;

    {
        let Some(app_elem) = create_application_with(environment, pid).into_walk_legacy() else {
            budget.record_skip(IncompleteReason {
                kind: IncompleteReasonKind::ProviderFailure,
                detail: IncompleteReasonDetail::EnumerationErrorUnclassified,
            });
            return (
                TreeWalkResult {
                    offscreen_rows: Vec::new(),
                    tree_markdown: String::new(),
                    nodes,
                    truncated: false,
                    nodes_skipped: true,
                    walk: budget.outcome(),
                    // No application AX element at all, so a requested window
                    // certainly did not resolve.
                    window_scope: window_id
                        .map(|_| WindowScope::AxUnresolved { ax_window_count: 0 }),
                },
                None,
            );
        };

        let (mut top_level, walk_these, resolved_scope) =
            resolve_top_level_with(environment, app_elem, pid, window_id, &mut budget);
        window_scope = resolved_scope;

        let mut label_claims = Vec::new();

        // Walk each top-level child at depth 0. Its own frame, read only if
        // a large container needs it, bounds what is on screen for children
        // whose app does not list the visible ones.
        for child in walk_these {
            environment.with_walk_element(source, child, |source, child| {
                walk_root(
                    source,
                    child,
                    &mut nodes,
                    &mut lines,
                    &mut index_counter,
                    &mut budget,
                    &mut batching_disabled,
                    max_depth,
                    include_web_content,
                    &mut deferred,
                    &mut label_claims,
                );
            });
        }

        // An open context menu of the walked window hangs off a view that
        // does not list it as a child: publish it after the window, each
        // under the same budget, so its items get element tokens. A menu
        // window that could not be examined in time (or past the cap) makes
        // the walk partial, never silently complete.
        // A WindowServer list that could not be read leaves open menus
        // unknown: partial.
        let menu_frames = match window_id {
            Some(_) if !nodes.is_empty() => environment
                .menu_frames(pid)
                .into_walk_legacy()
                .unwrap_or_else(|| {
                    budget.record_skip(IncompleteReason {
                        kind: IncompleteReasonKind::ProviderFailure,
                        detail: IncompleteReasonDetail::EnumerationErrorUnclassified,
                    });
                    budget.stop_for_timeout();
                    Vec::new()
                }),
            _ => Vec::new(),
        };
        let discovery = (!menu_frames.is_empty())
            .then(|| {
                menu_discovery_deadline(
                    environment.now(),
                    budget.outcome().truncated(),
                    environment.budget_remaining(&budget),
                )
            })
            .flatten();
        if !menu_frames.is_empty() && discovery.is_none() {
            if !budget.outcome().truncated() || environment.budget_expired(&budget) {
                budget.record_cut(IncompleteReason {
                    kind: IncompleteReasonKind::Budget,
                    detail: IncompleteReasonDetail::TimeBudget,
                });
            }
            budget.stop_for_timeout();
        }
        if let (Some(wid), Some(deadline)) = (window_id, discovery) {
            let mut incomplete = menu_frames.len() > super::context_menus::MAX_MENU_WINDOWS;
            if incomplete {
                budget.record_skip(IncompleteReason {
                    kind: IncompleteReasonKind::ProviderFailure,
                    detail: IncompleteReasonDetail::EnumerationErrorUnclassified,
                });
                super::evidence::record_status(
                    None,
                    super::evidence::ReadSite::MenuWindowCap,
                    super::evidence::EvidenceStatus::DiscoveryLimit,
                );
            }
            let mut walked_menus: Vec<[f64; 4]> = nodes
                .iter()
                .filter(|node| node.role == "AXMenu")
                .filter_map(|node| node.frame)
                .collect();
            for &frame in menu_frames
                .iter()
                .take(super::context_menus::MAX_MENU_WINDOWS)
            {
                let Some((menu, menu_frame, owner)) = environment
                    .menu_in_window(pid, frame, deadline)
                    .into_walk_legacy()
                else {
                    // Nothing readable reached a menu in this menu window.
                    incomplete = true;
                    budget.record_skip(IncompleteReason {
                        kind: IncompleteReasonKind::ProviderFailure,
                        detail: IncompleteReasonDetail::EnumerationErrorUnclassified,
                    });
                    if environment.budget_expired(&budget) {
                        budget.record_cut(IncompleteReason {
                            kind: IncompleteReasonKind::Budget,
                            detail: IncompleteReasonDetail::TimeBudget,
                        });
                    }
                    continue;
                };
                // Released with the other top-level elements after the walk.
                top_level.push(menu);
                match super::context_menus::decide_menu(
                    menu_frame,
                    frame,
                    owner,
                    wid,
                    &walked_menus,
                ) {
                    super::context_menus::MenuDecision::Publish => {}
                    super::context_menus::MenuDecision::Skip => continue,
                    super::context_menus::MenuDecision::Unknown => {
                        incomplete = true;
                        budget.record_untrusted(IncompleteReason {
                            kind: IncompleteReasonKind::Unknown,
                            detail: IncompleteReasonDetail::ScopeMismatch,
                        });
                        continue;
                    }
                }
                walked_menus.extend(menu_frame);
                environment.with_walk_element(source, menu, |source, menu| {
                    walk_element(
                        source,
                        menu,
                        menu,
                        0,
                        None,
                        None,
                        LabelPath::Inactive,
                        false,
                        &mut nodes,
                        &mut lines,
                        &mut index_counter,
                        &mut budget,
                        &mut batching_disabled,
                        max_depth,
                        include_web_content,
                        &mut deferred,
                        &mut label_claims,
                    )
                });
            }
            if incomplete {
                budget.stop_for_timeout();
            }
        }

        (lines, nodes, offscreen_rows) = finish_deferred(
            source,
            lines,
            nodes,
            deferred,
            query,
            &mut index_counter,
            &mut budget,
            &mut batching_disabled,
            max_depth,
            include_web_content,
            label_claims,
        );

        // Release all top-level elements (copy_children / copy_ax_windows both retain).
        release_environment(environment, top_level);

        environment.release_application(app_elem);
    }

    let (result, sample) = finalize_with_deadline_sample(
        lines,
        nodes,
        offscreen_rows,
        query,
        budget,
        ledger,
        window_scope,
        |budget| environment.budget_expired(budget),
    );
    (result, Some(sample))
}

/// `Instant` has no public fixed epoch. Seed a single origin for the scripted
/// clock; tests advance from it and never compare it with the running clock.
#[cfg(test)]
fn scripted_clock_origin() -> Instant {
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

#[allow(clippy::too_many_arguments)]
fn finalize_with_deadline_sample(
    lines: Vec<(usize, String)>,
    nodes: Vec<AXNode>,
    offscreen_rows: Vec<dense_rows::OffscreenRows>,
    query: Option<&str>,
    mut budget: WalkBudget,
    ledger: SkipLedger,
    window_scope: Option<WindowScope>,
    sample: impl FnOnce(&WalkBudget) -> bool,
) -> (TreeWalkResult, bool) {
    let final_deadline_sample = sample(&budget);
    record_attribute_failures(&mut budget);
    if final_deadline_sample {
        budget.record_cut(IncompleteReason {
            kind: IncompleteReasonKind::Budget,
            detail: IncompleteReasonDetail::TimeBudget,
        });
    }
    let walk = budget.outcome();
    let nodes_skipped = ledger.finish();
    (
        finalize_walk(
            lines,
            nodes,
            offscreen_rows,
            query,
            walk,
            nodes_skipped,
            window_scope,
        ),
        final_deadline_sample,
    )
}

/// Attribute failures are distinct from an unsupported or absent attribute.
/// Batch-call failures are excluded here: their individual replayed reads
/// carry the eventual result, and a fully recovered replay is not incomplete.
fn record_attribute_failures(budget: &mut WalkBudget) {
    use super::evidence::{current_evidence, EvidenceStatus, ReadSite};
    let evidence = current_evidence();
    let failed = evidence.iter().enumerate().any(|(index, entry)| {
        let attribute_site = matches!(
            entry.site,
            ReadSite::Role
                | ReadSite::ActionNames
                | ReadSite::Content(_)
                | ReadSite::Control(_)
                | ReadSite::Frame(_)
                | ReadSite::Scrollbar(_)
                | ReadSite::Tab(_)
                | ReadSite::ValueSettable
                | ReadSite::SelectedSettable
                | ReadSite::MenuSelected
        );
        let read_failed = attribute_site
            && (matches!(entry.status, EvidenceStatus::Unknown(_))
                || matches!(
                    (entry.site, entry.status),
                    (
                        ReadSite::Role,
                        EvidenceStatus::Absent | EvidenceStatus::Unavailable
                    )
                ));
        if !read_failed {
            return false;
        }
        let recovery_site = match entry.site {
            ReadSite::Tab("AXRole") => Some(ReadSite::Role),
            ReadSite::Tab("AXSelected") => Some(ReadSite::Control("AXSelected")),
            ReadSite::Tab(name @ ("AXTitle" | "AXDescription" | "AXRoleDescription")) => {
                Some(ReadSite::Content(name))
            }
            _ => None,
        };
        let Some((owner, site)) = entry.owner.zip(recovery_site) else {
            return true;
        };
        !evidence[index + 1..].iter().any(|later| {
            later.owner == Some(owner)
                && later.site == site
                && (later.status == EvidenceStatus::Known
                    || (site != ReadSite::Role && later.status == EvidenceStatus::Absent))
        })
    });
    if failed {
        budget.record_skip(IncompleteReason {
            kind: IncompleteReasonKind::ProviderFailure,
            detail: IncompleteReasonDetail::AttributeReadFailed,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_root<S: AxTreeSource>(
    source: &S,
    root: &S::Element,
    nodes: &mut Vec<AXNode>,
    lines: &mut Vec<(usize, String)>,
    index_counter: &mut usize,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
    max_depth: usize,
    include_web_content: bool,
    deferred: &mut Vec<DeferredRow<S::Element>>,
    label_claims: &mut Vec<(usize, String)>,
) {
    let surface = rows_windowed().then_some(source.key(root));
    dense_rows::with_visible_clip(surface, || {
        walk_element(
            source,
            root,
            root,
            0,
            None,
            None,
            LabelPath::Inactive,
            false,
            nodes,
            lines,
            index_counter,
            budget,
            batching_disabled,
            max_depth,
            include_web_content,
            deferred,
            label_claims,
        )
    });
}

#[allow(clippy::too_many_arguments)]
fn finish_deferred<S: AxTreeSource>(
    source: &S,
    mut lines: Vec<(usize, String)>,
    mut nodes: Vec<AXNode>,
    mut deferred: Vec<DeferredRow<S::Element>>,
    query: Option<&str>,
    counter: &mut usize,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
    max_depth: usize,
    include_web_content: bool,
    mut label_claims: Vec<(usize, String)>,
) -> (
    Vec<(usize, String)>,
    Vec<AXNode>,
    Vec<dense_rows::OffscreenRows>,
) {
    let mut offscreen_rows = Vec::new();
    if !deferred.is_empty() {
        let search_offscreen = query.is_some_and(|q| !q.trim().is_empty());
        let skipped = with_dense_rows_windowed(false, || {
            dense_rows::walk_deferred(&mut deferred, search_offscreen, |row| {
                let mut chunk = Chunk {
                    lines: Vec::new(),
                    nodes: Vec::new(),
                };
                walk_element(
                    source,
                    &row.element,
                    &row.element,
                    row.depth,
                    row.parent_index,
                    Some(&row.parent_role),
                    row.label_path,
                    row.in_web_content,
                    &mut chunk.nodes,
                    &mut chunk.lines,
                    counter,
                    budget,
                    batching_disabled,
                    max_depth,
                    include_web_content,
                    &mut Vec::new(),
                    &mut label_claims,
                );
                chunk
            })
        });
        if skipped {
            note_skipped();
        }
        (lines, nodes, offscreen_rows) = dense_rows::splice_and_renumber(lines, nodes, deferred);
    }
    apply_owned_label_claims(&mut nodes, label_claims);
    (lines, nodes, offscreen_rows)
}

#[allow(clippy::too_many_arguments)]
fn finalize_walk(
    mut lines: Vec<(usize, String)>,
    mut nodes: Vec<AXNode>,
    offscreen_rows: Vec<dense_rows::OffscreenRows>,
    query: Option<&str>,
    walk: WalkOutcome,
    nodes_skipped: bool,
    window_scope: Option<WindowScope>,
) -> TreeWalkResult {
    // Folding merges text rows into their parent; an exhaustive walk keeps
    // every row it visited.
    if !exhaustive_nodes() {
        fold_with_owned_items(&mut lines, &mut nodes);
    }
    nodes.retain(|n| n.role != "AXTabStripSummary");
    let mut line_nodes = align_line_nodes(&lines, &nodes);
    prune_echo_text_rows(&mut lines, &mut line_nodes, &nodes);
    let mut tree_markdown = if let Some(q) = query {
        filter_tree_with_original_text(&lines, &line_nodes, &nodes, q)
    } else {
        render_lines(&lines)
    };

    if let Some(note) = walk.note() {
        tree_markdown.push('\n');
        tree_markdown.push_str(&note);
    }

    TreeWalkResult {
        offscreen_rows,
        tree_markdown,
        nodes,
        truncated: walk.truncated(),
        nodes_skipped,
        walk,
        window_scope,
    }
}

/// Resolve requested scope from environment observations using the shared
/// production decisions. Values returned for walking are borrowed aliases;
/// only the first vector owns retain credits.
fn resolve_top_level_with<E: AxWalkEnvironment>(
    environment: &E,
    app_elem: E::Application,
    pid: i32,
    window_id: Option<u32>,
    budget: &mut WalkBudget,
) -> (Vec<E::Element>, Vec<E::Element>, Option<WindowScope>) {
    let mut launch_wait = LaunchWait::new(budget.time_limit());
    loop {
        let attempt_started = environment.now();
        environment.enable(pid, app_elem).into_walk_legacy();
        let recovery_deadline = budget
            .time_limit()
            .and_then(|limit| attempt_started.checked_add(limit));
        let mut top_level =
            top_level_with(environment, app_elem, pid, window_id, recovery_deadline);
        let Some(wid) = window_id else {
            let walk = top_level.clone();
            return (top_level, walk, None);
        };
        let mut decision = scope_top_level_with(environment, &top_level, pid, wid);
        if matches!(decision.scope, WindowScope::AxUnresolved { .. }) {
            if let Some((parent_window_id, sheet)) =
                window_holding_sheet_with(environment, &top_level, wid)
            {
                release_environment(environment, std::mem::take(&mut top_level));
                top_level.push(sheet);
                decision = ScopeDecision {
                    scope: WindowScope::SheetOfWindow { parent_window_id },
                    walk: vec![0],
                };
            }
        }
        if matches!(decision.scope, WindowScope::AxUnresolved { .. }) {
            match launch_wait.step(attempt_started, environment.now(), || {
                environment.is_still_launching(pid)
            }) {
                LaunchStep::Retry(pause) => {
                    release_environment(environment, top_level);
                    environment.pause(pause);
                    continue;
                }
                LaunchStep::TimedOut => budget.stop_for_app_lookup_timeout(),
                LaunchStep::Proceed => {}
            }
        }
        let walk = decision
            .walk
            .iter()
            .map(|&index| top_level[index])
            .collect();
        return (top_level, walk, Some(decision.scope));
    }
}

/// The application's top-level AX children: `AXChildren` ∪ `AXWindows`.
///
/// `AXChildren` omits windows when the app isn't frontmost (AppKit
/// limitation); `AXWindows` returns the window list regardless of activation
/// state, so the union is the only way to see background windows. Every
/// returned element is retained; release them with [`release_environment`].
fn top_level_with<E: AxWalkEnvironment>(
    environment: &E,
    app_elem: E::Application,
    pid: i32,
    window_id: Option<u32>,
    recovery_deadline: Option<Instant>,
) -> Vec<E::Element> {
    let children = environment.children(app_elem);
    // A requested window on another Space is absent from AXWindows; the
    // `_including` variant recovers it by exact CGWindowID.
    let mut top_level = top_level_union_with(
        children,
        || environment.windows(app_elem, pid, window_id),
        |existing, candidate| environment.same(*existing, *candidate),
        |duplicate| environment.release(duplicate),
    );

    if let Some(wid) = window_id {
        let already_present = top_level.iter().any(|&element| {
            environment.set_element_timeout(element);
            environment.window_id(element) == Some(wid)
        });
        if !already_present && !recovery_deadline_reached(recovery_deadline, environment.now()) {
            if let Some([x, y, width, height]) = environment.window_bounds(wid) {
                let hit = window_center_hit_if_live(recovery_deadline, environment.now(), || {
                    environment
                        .hit(pid, x + width * 0.5, y + height * 0.5)
                        .into_walk_legacy()
                });
                if let Some(root) = hit.flatten().and_then(|element| {
                    recover_window_root_with(environment, element, wid, recovery_deadline)
                }) {
                    if top_level
                        .iter()
                        .any(|&element| environment.same(element, root))
                    {
                        environment.release(root);
                    } else {
                        top_level.push(root);
                    }
                }
            }
        }
    }
    top_level
}

/// Union retained observations by identity; duplicates surrender their own
/// retain credit exactly once. Failed children retain the legacy skip mark.
fn top_level_union_with<T>(
    children: super::evidence::Observed<Read<Vec<T>>>,
    windows: impl FnOnce() -> super::evidence::Observed<Vec<T>>,
    same: impl Fn(&T, &T) -> bool,
    mut release: impl FnMut(T),
) -> Vec<T> {
    let children = match children.into_walk_legacy() {
        Read::Known(children) => Some(children),
        Read::Absent => Some(Vec::new()),
        Read::Unknown(_) => None,
    };
    let mut top_level = children_or_skipped(children);
    for window in windows().into_walk_legacy() {
        if top_level.iter().any(|item| same(item, &window)) {
            release(window);
        } else {
            top_level.push(window);
        }
    }
    top_level
}

const MAX_WINDOW_RECOVERY_ANCESTORS: usize = 32;

fn recovery_deadline_reached(deadline: Option<Instant>, now: Instant) -> bool {
    let expired = deadline.is_some_and(|deadline| now >= deadline);
    if expired {
        super::evidence::record_status(
            None,
            super::evidence::ReadSite::RecoveryDeadline,
            super::evidence::EvidenceStatus::DeadlineExpired,
        );
    }
    expired
}

fn window_center_hit_if_live<T>(
    deadline: Option<Instant>,
    now: Instant,
    hit: impl FnOnce() -> T,
) -> Option<T> {
    if recovery_deadline_reached(deadline, now) {
        None
    } else {
        Some(hit())
    }
}

fn topmost_window_match(window_ids: &[Option<u32>], requested: u32) -> Option<usize> {
    window_ids
        .iter()
        .rposition(|window_id| *window_id == Some(requested))
}

fn recovery_root_outcome(
    window_ids: &[Option<u32>],
    requested: u32,
) -> super::evidence::Observed<Option<usize>> {
    use super::evidence::{EvidenceStatus, Observed, ReadEvidence, ReadSite};
    let legacy = topmost_window_match(window_ids, requested);
    let status = if legacy.is_some() {
        EvidenceStatus::Known
    } else if window_ids.iter().any(Option::is_none) {
        EvidenceStatus::Unavailable
    } else {
        EvidenceStatus::Absent
    };
    Observed {
        legacy,
        evidence: vec![ReadEvidence {
            owner: None,
            site: ReadSite::RecoveryRoot,
            status,
            attempt: 0,
            listing: None,
        }],
    }
}

/// Recover the highest matching ancestor of a center-hit element. The hit and
/// every parent are retained; keep only one extra retain on the returned root.
fn recover_window_root_with<E: AxWalkEnvironment>(
    environment: &E,
    hit: E::Element,
    requested: u32,
    deadline: Option<Instant>,
) -> Option<E::Element> {
    let mut ancestors = vec![hit];
    for _ in 1..MAX_WINDOW_RECOVERY_ANCESTORS {
        if recovery_deadline_reached(deadline, environment.now()) {
            release_environment(environment, ancestors);
            return None;
        }
        let current = *ancestors.last().expect("the hit is always present");
        environment.set_element_timeout(current);
        let parent = environment.parent(current);
        super::evidence::record_status(
            Some(environment.element_key(current)),
            super::evidence::ReadSite::RecoveryParent,
            if parent.is_some() {
                super::evidence::EvidenceStatus::Known
            } else {
                super::evidence::EvidenceStatus::Unavailable
            },
        );
        let Some(parent) = parent else {
            break;
        };
        ancestors.push(parent);
        if recovery_deadline_reached(deadline, environment.now()) {
            release_environment(environment, ancestors);
            return None;
        }
    }

    let mut window_ids = Vec::with_capacity(ancestors.len());
    for &element in &ancestors {
        if recovery_deadline_reached(deadline, environment.now()) {
            release_environment(environment, ancestors);
            return None;
        }
        environment.set_element_timeout(element);
        let id = environment.window_id(element);
        super::evidence::record_status(
            Some(environment.element_key(element)),
            super::evidence::ReadSite::RecoveryWindowId,
            if id.is_some() {
                super::evidence::EvidenceStatus::Known
            } else {
                super::evidence::EvidenceStatus::Unavailable
            },
        );
        window_ids.push(id);
        if recovery_deadline_reached(deadline, environment.now()) {
            release_environment(environment, ancestors);
            return None;
        }
    }
    let mut observed = recovery_root_outcome(&window_ids, requested);
    if let Some(index) = observed.legacy {
        observed.evidence[0].owner = Some(environment.element_key(ancestors[index]));
    }
    let root = observed.into_walk_legacy().map(|index| {
        environment.retain(ancestors[index]);
        ancestors[index]
    });
    release_environment(environment, ancestors);
    root
}

fn release_environment<E: AxWalkEnvironment>(environment: &E, elements: Vec<E::Element>) {
    for element in elements {
        environment.release(element);
    }
}

/// The children a checked `AXChildren` read found; a failed read records
/// the walk as skipping what it could not see (at the application root: the
/// menu bar and any sheet), even when `AXWindows` still recovers the window.
fn children_or_skipped<T>(read: Option<Vec<T>>) -> Vec<T> {
    read.unwrap_or_else(|| {
        note_skipped();
        Vec::new()
    })
}

/// Decide which of `top_level` a walk scoped to `wid` covers.
fn scope_top_level_with<E: AxWalkEnvironment>(
    environment: &E,
    top_level: &[E::Element],
    pid: i32,
    wid: u32,
) -> ScopeDecision {
    let candidates: Vec<TopLevelCandidate> = top_level
        .iter()
        .map(|&child| environment.scope_candidate(child).into_walk_legacy())
        .collect();
    decide_window_scope(&candidates, wid, || environment.window_owner(pid, wid))
}

fn scope_candidate_with<S: AxTreeSource>(
    source: &S,
    element: &S::Element,
    window_id: impl FnOnce() -> Option<u32>,
) -> super::evidence::Observed<TopLevelCandidate> {
    use super::evidence::{read_status, EvidenceStatus, Observed, ReadEvidence, ReadSite};
    source.set_messaging_timeout(element);
    let role_read = source.text(element, "AXRole");
    let role_status = read_status(&role_read);
    let role = known(role_read).unwrap_or_default();
    let subrole_read = source.text(element, "AXSubrole");
    let subrole_status = read_status(&subrole_read);
    let subrole = known(subrole_read);
    let identifier_read = source.text(element, "AXIdentifier");
    let identifier_status = read_status(&identifier_read);
    let identifier = known(identifier_read);
    let window_read = if is_window_like_role(&role) {
        Some(window_id())
    } else {
        None
    };
    let owner = Some(source.key(element));
    let mut evidence = [
        ("AXRole", role_status),
        ("AXSubrole", subrole_status),
        ("AXIdentifier", identifier_status),
    ]
    .into_iter()
    .map(|(name, status)| ReadEvidence {
        owner,
        site: ReadSite::ScopeCandidate(name),
        status,
        attempt: 0,
        listing: None,
    })
    .collect::<Vec<_>>();
    if let Some(result) = window_read {
        evidence.push(ReadEvidence {
            owner,
            site: ReadSite::ScopeCandidate("AXWindowId"),
            status: if result.is_some() {
                EvidenceStatus::Known
            } else {
                EvidenceStatus::Unavailable
            },
            attempt: 0,
            listing: None,
        });
    }
    Observed {
        legacy: TopLevelCandidate {
            role,
            subrole,
            identifier,
            ax_window_id: window_read.flatten(),
        },
        evidence,
    }
}

/// The CGWindowID of the app window holding an `AXSheet` whose own
/// CGWindowID is `wid`.
fn window_holding_sheet_with<E: AxWalkEnvironment>(
    environment: &E,
    top_level: &[E::Element],
    wid: u32,
) -> Option<(u32, E::Element)> {
    for &window in top_level {
        if sheet_role_with(environment, window).as_deref() != Some("AXWindow") {
            continue;
        }
        let children = match environment.sheet_children(window).into_walk_legacy() {
            Read::Known(children) => children,
            Read::Absent | Read::Unknown(_) => Vec::new(),
        };
        let mut sheet = None;
        for child in children {
            environment.set_element_timeout(child);
            if sheet.is_none()
                && sheet_role_with(environment, child).as_deref() == Some("AXSheet")
                && recovery_window_id_with(environment, child) == Some(wid)
            {
                sheet = Some(child);
            } else {
                environment.release(child);
            }
        }
        if let Some(sheet) = sheet {
            if let Some(parent) =
                recovery_window_id_with(environment, window).filter(|&parent| parent != wid)
            {
                return Some((parent, sheet));
            }
            environment.release(sheet);
        }
    }
    None
}

fn sheet_role_with<E: AxWalkEnvironment>(environment: &E, element: E::Element) -> Option<String> {
    known(
        super::evidence::observe_read(
            Some(environment.element_key(element)),
            super::evidence::ReadSite::RecoverySheetRole,
            environment.sheet_role(element),
        )
        .into_walk_legacy(),
    )
}

fn recovery_window_id_with<E: AxWalkEnvironment>(
    environment: &E,
    element: E::Element,
) -> Option<u32> {
    let id = environment.window_id(element);
    super::evidence::record_status(
        Some(environment.element_key(element)),
        super::evidence::ReadSite::RecoveryWindowId,
        if id.is_some() {
            super::evidence::EvidenceStatus::Known
        } else {
            super::evidence::EvidenceStatus::Unavailable
        },
    );
    id
}

fn known<T>(read: Read<T>) -> Option<T> {
    match read {
        Read::Known(value) => Some(value),
        Read::Absent | Read::Unknown(_) => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_element<S: AxTreeSource>(
    source: &S,
    element: &S::Element,
    surface: &S::Element,
    depth: usize,
    parent_index: Option<usize>,
    parent_role: Option<&str>,
    parent_path: LabelPath,
    in_web_content: bool,
    nodes: &mut Vec<AXNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
    max_depth: usize,
    include_web_content: bool,
    deferred: &mut Vec<DeferredRow<S::Element>>,
    label_claims: &mut Vec<(usize, String)>,
) {
    if depth > max_depth {
        note_skipped();
        budget.record_cut(IncompleteReason {
            kind: IncompleteReasonKind::PolicyOmission,
            detail: IncompleteReasonDetail::DepthLimit,
        });
        return;
    }
    // Node and time budget: a refused node is counted as discovered but not
    // visited, and the walk unwinds with the partial tree it has.
    if !budget.admit() {
        return;
    }

    // Messaging timeouts are per AX object, not inherited from the application
    // element, so every descendant must be bounded before any attribute read.
    source.set_messaging_timeout(element);

    let role_observation = source.text(element, "AXRole");
    let role_status = super::evidence::read_status(&role_observation);
    let role_observed = known(role_observation);
    let role_read = role_observed.is_some();
    let role = role_observed.unwrap_or_else(|| {
        // A selector on the role cannot be answered for this node.
        note_skipped();
        "AXUnknown".into()
    });

    let label_path = next_label_path(parent_path, parent_role, Some(&role));
    let element_key = source.key(element);
    let _evidence_owner = super::evidence::EvidenceOwner::start(element_key);
    super::evidence::record_status(
        Some(element_key),
        super::evidence::ReadSite::Role,
        role_status,
    );
    if role_read && (role.trim().is_empty() || role == "AXUnknown") {
        super::evidence::record_status(
            Some(element_key),
            super::evidence::ReadSite::Role,
            super::evidence::EvidenceStatus::Unavailable,
        );
    }
    let child_label_path = child_label_path(label_path, &role, parent_role, element_key);
    let in_web_content = in_web_content || is_web_content_role(&role);
    if in_web_content && !include_web_content {
        note_skipped();
        return;
    }

    // A scroll area that can scroll is a row of its own: it is what scroll
    // targets, and its bar position tells an agent, and a diff read, whether
    // a scroll moved anything. One that cannot scroll is layout only.
    let mut scroll_attrs = None;
    if role == "AXScrollArea" && !in_web_content {
        let Some(attrs) = super::content_attrs::read_content_attrs_with(
            source,
            element,
            &role,
            budget,
            batching_disabled,
        ) else {
            push_timed_out_row(
                role,
                source.key(element),
                depth,
                parent_index,
                in_web_content,
                role_read,
                nodes,
                lines,
            );
            return;
        };
        if attrs.read_failed {
            note_skipped();
        }
        let vertical = super::scroll_area::bar_position_with(source, element, "down");
        let horizontal = super::scroll_area::bar_position_with(source, element, "right");
        if let Some(label) = super::scroll_area::position_label(vertical, horizontal) {
            let idx = *counter;
            *counter += 1;
            let snapshot_key = source.retain_for_snapshot(element);
            // An exhaustive walk keeps the area's own labels, read with their
            // failures, so a selector naming it is answered; the bar
            // position stays the row's value.
            let secure = attrs.secure;
            let canonical = super::vocabulary::CanonicalEvidence {
                role_read,
                subrole: (&attrs.subrole_status).into(),
                secure: (secure || !attrs.value_unknown).then_some(secure),
                copied_value: None,
                classification_failures: attrs.classification_failures.clone(),
                value_settable_observed_at: None,
                states: Default::default(),
            };
            let value_unknown = attrs.value_unknown;
            let ((title, description, own_value, identifier), exhaustive) = if exhaustive_nodes() {
                (scroll_area_labels(attrs).0, true)
            } else {
                (
                    (
                        None,
                        trimmed_label(known(
                            super::evidence::observe_read(
                                Some(element_key),
                                super::evidence::ReadSite::Scrollbar("AXDescription"),
                                source.text(element, "AXDescription"),
                            )
                            .into_walk_legacy(),
                        )),
                        None,
                        known(
                            super::evidence::observe_read(
                                Some(element_key),
                                super::evidence::ReadSite::Scrollbar("AXIdentifier"),
                                source.text(element, "AXIdentifier"),
                            )
                            .into_walk_legacy(),
                        ),
                    ),
                    false,
                )
            };
            let node = AXNode {
                owned_row_label: None,
                canonical,
                element_index: Some(idx),
                role: role.clone(),
                title,
                // A verify read keeps the area's own label order (its value,
                // then its identifier); the bar position is in value_state,
                // which is what the row's structured `value` reports.
                value: if secure {
                    Some("[redacted]".into())
                } else if value_unknown {
                    None
                } else if exhaustive {
                    own_value
                } else {
                    Some(label)
                },
                description,
                identifier,
                help: None,
                actions: known(
                    super::evidence::observe_read(
                        Some(element_key),
                        super::evidence::ReadSite::ActionNames,
                        source.action_names(element),
                    )
                    .into_walk_legacy(),
                )
                .unwrap_or_default(),
                element_ptr: snapshot_key,
                depth,
                parent_element_index: parent_index,
                frame: known(
                    super::evidence::observe_read(
                        Some(element_key),
                        super::evidence::ReadSite::Frame("scroll"),
                        source.frame(element),
                    )
                    .into_walk_legacy(),
                ),
                value_state: if secure {
                    Some("[redacted]".into())
                } else if value_unknown {
                    None
                } else {
                    vertical.or(horizontal).map(|v| format!("{v:.3}"))
                },
                value_description: None,
                min_value: Some(0.0),
                max_value: Some(1.0),
                enabled: None,
                selected: None,
                in_web_content,
            };
            lines.push((depth, format_node_line(&node)));
            nodes.push(node);
            walk_children(
                source,
                element,
                surface,
                &role,
                depth + 1,
                Some(idx),
                label_path,
                in_web_content,
                nodes,
                lines,
                counter,
                budget,
                batching_disabled,
                max_depth,
                include_web_content,
                deferred,
                label_claims,
            );
            return;
        }
        scroll_attrs = Some(attrs);
    }

    // Skip pure layout containers that have no interesting content.
    if role == "AXScrollArea" || role == "AXGroup" {
        // An exhaustive walk keeps the container as a display-only row, so a
        // selector naming it is answered rather than assumed absent.
        if exhaustive_nodes() {
            let attrs = match scroll_attrs.take() {
                Some(attrs) => Some(attrs),
                None => super::content_attrs::read_content_attrs_with(
                    source,
                    element,
                    &role,
                    budget,
                    batching_disabled,
                ),
            };
            let mut node = match attrs {
                Some(attrs) => {
                    if attrs.read_failed {
                        note_skipped();
                    }
                    display_node(
                        &role,
                        attrs,
                        source.key(element),
                        depth,
                        parent_index,
                        in_web_content,
                    )
                }
                None => {
                    note_skipped();
                    role_only_node(
                        role.clone(),
                        source.key(element),
                        depth,
                        parent_index,
                        in_web_content,
                    )
                }
            };
            node.canonical.role_read = role_read;
            lines.push((depth, format_node_line(&node)));
            nodes.push(node);
        }
        // Still recurse — children may be interesting. Layout containers
        // collapse, so children inherit the parent's depth AND the same
        // parent_index (no actionable node was emitted here).
        walk_children(
            source,
            element,
            surface,
            &role,
            depth,
            parent_index,
            child_label_path,
            in_web_content,
            nodes,
            lines,
            counter,
            budget,
            batching_disabled,
            max_depth,
            include_web_content,
            deferred,
            label_claims,
        );
        return;
    }

    // Keep AXTitle and AXDescription SEPARATE so that the tree format matches
    // the Swift reference: title → "title", description → (description).
    // This is critical for Calculator where AXTitle="" but AXDescription="2"
    // (digit buttons). Merging them would produce "2" (quoted) instead of (2)
    // (parens), breaking _find_calc_button which searches for "(2)".
    let Some(ContentAttrs {
        title,
        copied_value,
        value,
        placeholder: _,
        description,
        identifier,
        help,
        subrole,
        subrole_status,
        classification_failures,
        read_failed,
        secure,
        value_unknown,
    }) = super::content_attrs::read_content_attrs_with(
        source,
        element,
        &role,
        budget,
        batching_disabled,
    )
    else {
        // Only the role is known: a selector on its label cannot be answered.
        note_skipped();
        let mut node = role_only_node(
            role,
            source.key(element),
            depth,
            parent_index,
            in_web_content,
        );
        node.canonical.role_read = role_read;
        lines.push((depth, format_node_line(&node)));
        nodes.push(node);
        return;
    };
    if read_failed {
        // A label read failed: a selector on it cannot be answered.
        note_skipped();
    }
    let actions = known(
        super::evidence::observe_read(
            Some(element_key),
            super::evidence::ReadSite::ActionNames,
            source.action_names(element),
        )
        .into_walk_legacy(),
    )
    .unwrap_or_default();
    let visible_title = title.as_deref().unwrap_or("").trim().to_owned();
    let visible_description = description.as_deref().unwrap_or("").trim().to_owned();
    let visible_value = value.as_deref().unwrap_or("").trim().to_owned();
    let visible_description =
        window_chrome_button_description(&role, &visible_title, &visible_description, || {
            subrole.clone()
        })
        .map(str::to_owned)
        .unwrap_or(visible_description);

    collect_label_claim(
        label_claims,
        label_path,
        &role,
        [title.as_deref(), value.as_deref(), description.as_deref()],
    );

    let has_content =
        !visible_title.is_empty() || !visible_description.is_empty() || !visible_value.is_empty();
    // Some native controls expose no AX action names but do expose a writable
    // AXValue. Finder's transient inline-rename field is the important case:
    // rendering it without an element_index leaves an agent able to see the
    // field but unable to call set_value on it. Probe writability only for the
    // small family of value controls so arbitrary display nodes do not pay an
    // extra AX round trip.
    let value_settable_read = if actions.is_empty() && role_supports_value_addressing(&role) {
        Some(source.value_settable(element))
    } else {
        None
    };
    let value_settable_observed_at = value_settable_read.map(|_| std::time::Instant::now());
    let value_settable_in_time = !budget.expired();
    if let Some(value) = value_settable_read {
        let read = match value {
            super::write_outcome::Settable::Yes => Read::Known(true),
            super::write_outcome::Settable::No => Read::Known(false),
            super::write_outcome::Settable::Unknown(error) => {
                Read::Unknown(super::tree_source::ReadFailure::Ax(error))
            }
        };
        super::evidence::observe_read(
            Some(element_key),
            super::evidence::ReadSite::ValueSettable,
            read,
        )
        .into_walk_legacy();
    }
    let value_settable = matches!(
        value_settable_read,
        Some(super::write_outcome::Settable::Yes)
    );
    // A closed submenu can keep its descendants in AXChildren while reporting
    // those controls disabled. Never assign such a row a live element index:
    // the same native state also causes dispatch to refuse it, and exposing an
    // index for it invites agents to retain an unusable menu target.
    let selectable_row = selectable_row_eligible(
        &role,
        parent_role,
        actions.is_empty(),
        value_settable,
        || {
            selected_probe_observed(
                Some(element_key),
                budget.expired(),
                || source.selected_settable(element),
                note_skipped,
            )
            .into_walk_legacy()
        },
    );
    let actionless_control = selectable_row
        || actions.is_empty() && !value_settable && is_actionless_control_role(&role);
    // A row that may be indexed reads its control state and frame in one
    // batched call; any other row reads only its frame, later.
    // A batch that failed past the deadline stops the walk: like a content
    // read that timed out, the row keeps only its role, without an index or
    // its children.
    let control = if !actions.is_empty() || value_settable || actionless_control {
        match super::control_attrs::read_control_attrs_with(
            source,
            element,
            budget,
            batching_disabled,
        ) {
            Some(control) => Some(control),
            None => {
                push_timed_out_row(
                    role,
                    source.key(element),
                    depth,
                    parent_index,
                    in_web_content,
                    role_read,
                    nodes,
                    lines,
                );
                return;
            }
        }
    } else {
        None
    };
    let enabled = control.as_ref().and_then(|control| control.enabled);
    let is_actionable = is_addressable(
        &role,
        !actions.is_empty() || actionless_control,
        value_settable,
        enabled,
    );

    if !is_actionable && !has_content && role != "AXWindow" && role != "AXSheet" {
        if exhaustive_nodes() {
            let mut node = AXNode {
                canonical: super::vocabulary::CanonicalEvidence {
                    role_read,
                    subrole: (&subrole_status).into(),
                    secure: (secure || !value_unknown).then_some(secure),
                    copied_value: None,
                    classification_failures: classification_failures.clone(),
                    value_settable_observed_at: None,
                    states: Default::default(),
                },
                identifier: identifier.clone(),
                help: help.clone(),
                ..role_only_node(
                    role.clone(),
                    source.key(element),
                    depth,
                    parent_index,
                    in_web_content,
                )
            };
            initialize_owned_row_label(&mut node);
            lines.push((depth, format_node_line(&node)));
            nodes.push(node);
        }
        walk_children(
            source,
            element,
            surface,
            &role,
            depth + 1,
            parent_index,
            child_label_path,
            in_web_content,
            nodes,
            lines,
            counter,
            budget,
            batching_disabled,
            max_depth,
            include_web_content,
            deferred,
            label_claims,
        );
        return;
    }

    let mut canonical = super::vocabulary::CanonicalEvidence {
        role_read,
        subrole: (&subrole_status).into(),
        secure: (secure || !value_unknown).then_some(secure),
        copied_value: if matches!(role.as_str(), "AXCheckBox" | "AXSwitch" | "AXRadioButton")
            || (role == "AXButton" && subrole.as_deref() == Some("AXToggle"))
        {
            copied_value.as_ref().map(|value| value.state_value.clone())
        } else {
            None
        },
        classification_failures,
        value_settable_observed_at,
        states: Default::default(),
    };
    if let Some(settable) = value_settable_read {
        canonical.states.value_settable = if value_settable_in_time {
            super::vocabulary::Observation::Known(settable)
        } else {
            super::vocabulary::Observation::Expired
        };
    }
    let element_ptr = source.key(element);
    let frame = match &control {
        Some(control) => control.frame,
        None => {
            match super::control_attrs::read_frame_with(source, element, budget, batching_disabled)
            {
                Some(frame) => frame,
                None => {
                    push_timed_out_row(
                        role,
                        source.key(element),
                        depth,
                        parent_index,
                        in_web_content,
                        role_read,
                        nodes,
                        lines,
                    );
                    return;
                }
            }
        }
    };
    // Structured `elements` only contains actionable nodes. Keep all new AX
    // round-trips behind that same gate so display-only rows pay no cost.
    let control_state = read_control_state_if_actionable(is_actionable, || {
        // An indexed row always has its batched control state.
        let control = control.clone().unwrap_or_default();
        ControlState {
            value_state: copied_value
                .map(|copied| copied.state_value)
                .filter(|v| !v.trim().is_empty())
                .or_else(|| value.clone())
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty()),
            value_description: if secure || value_unknown {
                None
            } else {
                control
                    .value_description
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty())
            },
            min_value: control.min_value,
            max_value: control.max_value,
            enabled,
            selected: control.selected,
        }
    });
    let (redacted_value, redacted_value_state) = if secure {
        (Some("[redacted]".to_owned()), Some("[redacted]".to_owned()))
    } else {
        (
            (!visible_value.is_empty()).then(|| visible_value.clone()),
            control_state.value_state.clone(),
        )
    };
    let mut node = if is_actionable {
        let idx = *counter;
        *counter += 1;
        // Retain so the element stays alive in the cache after `copy_children`
        // releases the per-child ref at the end of the caller's loop.
        let element_ptr = source.retain_for_snapshot(element);
        AXNode {
            owned_row_label: None,
            canonical: canonical.clone(),
            element_index: Some(idx),
            role: role.clone(),
            title: if visible_title.is_empty() {
                None
            } else {
                Some(visible_title.clone())
            },
            value: redacted_value,
            description: if visible_description.is_empty() {
                None
            } else {
                Some(visible_description.clone())
            },
            identifier: identifier.clone(),
            help: help.clone(),
            actions: actions.clone(),
            element_ptr,
            depth,
            parent_element_index: parent_index,
            frame,
            value_state: redacted_value_state,
            value_description: control_state.value_description.clone(),
            min_value: control_state.min_value,
            max_value: control_state.max_value,
            enabled: control_state.enabled,
            selected: control_state.selected,
            in_web_content,
        }
    } else {
        AXNode {
            owned_row_label: None,
            canonical,
            element_index: None,
            role: role.clone(),
            title: if visible_title.is_empty() {
                None
            } else {
                Some(visible_title.clone())
            },
            value: redacted_value,
            description: if visible_description.is_empty() {
                None
            } else {
                Some(visible_description.clone())
            },
            identifier: identifier.clone(),
            help: help.clone(),
            actions: vec![],
            element_ptr,
            depth,
            parent_element_index: parent_index,
            frame,
            value_state: redacted_value_state,
            value_description: control_state.value_description.clone(),
            min_value: control_state.min_value,
            max_value: control_state.max_value,
            enabled: control_state.enabled,
            selected: control_state.selected,
            in_web_content,
        }
    };
    initialize_owned_row_label(&mut node);

    // Track this node as the parent for its descendants only when it was
    // assigned an element_index (mirrors what the markdown shows: only
    // indexed rows are addressable in click(element_index=N)).
    let next_parent = node.element_index.or(parent_index);

    // A closed menu of the menu bar stays collapsed when the caller asked for
    // it: its items are reachable through invoke_menu or a query, and walking
    // them cost most of a read (94% of a Chrome window read in a VM check)
    // and ate the node budget before the window content.
    let collapse_here = role == "AXMenuBarItem"
        && MENUS_COLLAPSED.with(std::cell::Cell::get)
        && known(
            super::evidence::observe_read(
                Some(element_key),
                super::evidence::ReadSite::MenuSelected,
                source.boolean(element, "AXSelected"),
            )
            .into_walk_legacy(),
        ) != Some(true);
    let line = if role == "AXMenuBar" && MENUS_COLLAPSED.with(std::cell::Cell::get) {
        format!("{} {COLLAPSED_MENU_NOTE}", format_node_line(&node))
    } else {
        format_node_line(&node)
    };
    lines.push((depth, line));
    nodes.push(node);
    if collapse_here {
        note_skipped();
        return;
    }

    walk_children(
        source,
        element,
        surface,
        &role,
        depth + 1,
        next_parent,
        child_label_path,
        in_web_content,
        nodes,
        lines,
        counter,
        budget,
        batching_disabled,
        max_depth,
        include_web_content,
        deferred,
        label_claims,
    );
}

#[allow(clippy::too_many_arguments)]
fn walk_children<S: AxTreeSource>(
    source: &S,
    element: &S::Element,
    surface: &S::Element,
    role: &str,
    depth: usize,
    parent_index: Option<usize>,
    label_path: LabelPath,
    in_web_content: bool,
    nodes: &mut Vec<AXNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
    max_depth: usize,
    include_web_content: bool,
    deferred: &mut Vec<DeferredRow<S::Element>>,
    label_claims: &mut Vec<(usize, String)>,
) {
    if depth > max_depth {
        // A tree that ends exactly at the cap is still complete.
        match source.observed_children(element).into_walk_legacy() {
            Read::Absent => {}
            Read::Known(below) if below.is_empty() => {}
            Read::Known(_below) => {
                note_skipped();
                budget.record_cut(IncompleteReason {
                    kind: IncompleteReasonKind::PolicyOmission,
                    detail: IncompleteReasonDetail::DepthLimit,
                });
            }
            Read::Unknown(_) => note_skipped(),
        }
        return;
    }
    let children = match source.observed_children(element).into_walk_legacy() {
        Read::Known(children) => children,
        Read::Absent => Vec::new(),
        Read::Unknown(_) => {
            note_skipped();
            return;
        }
    };
    let tab_plan = if !in_web_content && children.len() >= TAB_STRIP_COMPACTION_THRESHOLD {
        plan_macos_tab_strip_observed(source, element, role, &children, budget, batching_disabled)
            .into_walk_legacy()
    } else {
        None
    };
    let plan = if tab_plan.is_none()
        && dense_rows::windows_children(rows_windowed(), in_web_content, role, children.len())
    {
        Some(dense_rows::plan_collection_children(
            source, element, surface, role, &children, budget,
        ))
    } else {
        None
    };
    for (index, child) in children.into_iter().enumerate() {
        if let Some(ref tp) = tab_plan {
            if index == tp.first_omitted_index {
                lines.push((depth, format_omitted_tabs_line(tp.omitted_count)));
                nodes.push(role_only_node(
                    "AXTabStripSummary".into(),
                    0,
                    depth,
                    parent_index,
                    in_web_content,
                ));
            }
            if !tp.keep[index] {
                note_skipped();
                continue;
            }
        }
        let action = plan.as_ref().map_or(ChildPlan::WalkNow, |plan| plan[index]);
        match action {
            ChildPlan::Defer { visible } => deferred.push(DeferredRow {
                anchor: nodes.len(),
                seq: deferred.len(),
                position: index,
                collection: source.key(element),
                element: child.clone(),
                depth,
                parent_index,
                parent_role: role.to_owned(),
                label_path,
                in_web_content,
                visible,
                walked: None,
            }),
            ChildPlan::WalkNow => walk_element(
                source,
                &child,
                surface,
                depth,
                parent_index,
                Some(role),
                label_path,
                in_web_content,
                nodes,
                lines,
                counter,
                budget,
                batching_disabled,
                max_depth,
                include_web_content,
                deferred,
                label_claims,
            ),
        }
    }
}

fn plan_macos_tab_strip<S: AxTreeSource>(
    source: &S,
    parent: &S::Element,
    parent_role: &str,
    children: &[S::Element],
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> Option<TabStripCompaction> {
    if !matches!(
        parent_role,
        "AXTabGroup" | "AXGroup" | "AXScrollArea" | "AXToolbar"
    ) {
        return None;
    }
    let parent_title = known(source.text(parent, "AXTitle"));
    let parent_desc = known(source.text(parent, "AXDescription"));
    let parent_role_desc = known(source.text(parent, "AXRoleDescription"));

    let mut child_roles = Vec::with_capacity(children.len());
    let mut child_role_descs = Vec::with_capacity(children.len());
    let mut child_selected = Vec::with_capacity(children.len());
    let mut child_values = Vec::with_capacity(children.len());
    let mut tab_like_count = 0usize;

    for child in children {
        let _evidence_owner = super::evidence::EvidenceOwner::start(source.key(child));
        source.set_messaging_timeout(child);
        let r = known(source.text(child, "AXRole")).unwrap_or_default();
        let rd = if r == "AXTab" || r == "AXRadioButton" {
            None
        } else {
            known(source.text(child, "AXRoleDescription"))
        };
        if cua_driver_core::snapshot_compaction::is_tab_like_child(parent_role, &r, rd.as_deref()) {
            tab_like_count += 1;
            child_selected.push(known(source.boolean(child, "AXSelected")));
            let role = if r.is_empty() { "AXUnknown" } else { &r };
            let attrs = super::content_attrs::read_content_attrs_with(
                source,
                child,
                role,
                budget,
                batching_disabled,
            )?;
            let (value, incomplete) = tab_child_value(attrs);
            if incomplete {
                note_skipped();
            }
            child_values.push(value);
        } else {
            child_selected.push(None);
            child_values.push(None);
        }
        child_roles.push(r);
        child_role_descs.push(rd);
    }
    if tab_like_count < TAB_STRIP_COMPACTION_THRESHOLD {
        return None;
    }
    let metas: Vec<TabChildMeta<'_>> = (0..children.len())
        .map(|i| TabChildMeta {
            role: &child_roles[i],
            role_description: child_role_descs[i].as_deref(),
            title: None,
            selected: child_selected[i],
            value: child_values[i].as_deref(),
            value_state: child_values[i].as_deref(),
        })
        .collect();
    compact_tab_strip(
        parent_role,
        parent_title.as_deref(),
        parent_desc.as_deref(),
        parent_role_desc.as_deref(),
        &metas,
    )
}

fn plan_macos_tab_strip_observed<S: AxTreeSource>(
    source: &S,
    parent: &S::Element,
    parent_role: &str,
    children: &[S::Element],
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> super::evidence::Observed<Option<TabStripCompaction>> {
    let source = super::evidence_source::EvidenceSource::new(
        source,
        super::evidence_source::Domain::Tab,
        None,
    );
    let result = plan_macos_tab_strip(
        &source,
        parent,
        parent_role,
        children,
        budget,
        batching_disabled,
    );
    source.finish(result)
}

fn tab_child_value(attrs: ContentAttrs) -> (Option<String>, bool) {
    (
        attrs.copied_value.map(|value| value.state_value),
        attrs.read_failed,
    )
}

#[cfg(test)]
mod secure_value_tests {
    use super::*;
    use crate::ax::batch_attrs::BatchFailure;
    use crate::ax::content_attrs::{read_with, AttrStatus, ContentReader};
    use cua_driver_core::protocol::Content;
    use cua_driver_core::window_state_view::{apply, ViewContext, ViewKey, ViewOptions};
    use serde_json::json;
    use std::cell::RefCell;

    struct Fake {
        attrs: [AttrStatus; 7],
        trace: Vec<&'static str>,
    }
    impl Fake {
        fn new(subrole: Option<&str>) -> Self {
            let mut attrs = std::array::from_fn(|_| AttrStatus::Absent);
            if let Some(subrole) = subrole {
                attrs[5] = AttrStatus::Known(subrole.into());
            }
            Self {
                attrs,
                trace: Vec::new(),
            }
        }
        fn read(&mut self, role: AttrStatus) -> ContentAttrs {
            read_with(role, self, &mut false).unwrap()
        }
    }
    impl ContentReader for Fake {
        fn batch(&mut self) -> Result<[AttrStatus; 7], BatchFailure> {
            self.trace.push("batch");
            Ok(self.attrs.clone())
        }
        fn string(&mut self, _: &str) -> AttrStatus {
            panic!("batch should succeed")
        }
        fn value(&mut self) -> Result<Option<StringishAttrValue>, AXError> {
            self.trace.push("AXValue");
            Ok(Some(StringishAttrValue {
                string_value: Some("ordinary".into()),
                state_value: "ordinary".into(),
            }))
        }
        fn expired(&self) -> bool {
            false
        }
    }

    #[test]
    fn tab_compaction_never_reads_secure_or_unknown_child_values() {
        assert!(cua_driver_core::snapshot_compaction::is_tab_like_child(
            "AXTabGroup",
            "",
            Some("tab")
        ));
        let mut secure = Fake::new(Some("AXSecureTextField"));
        let attrs = secure.read(AttrStatus::Known("AXTab".into()));
        assert_eq!(tab_child_value(attrs).0, None);
        assert!(!secure.trace.contains(&"AXValue"));
        let mut unknown = Fake::new(None);
        let attrs = unknown.read(crate::ax::content_attrs::FAILED_TEST);
        let (value, incomplete) = tab_child_value(attrs);
        assert_eq!(value, None);
        assert!(incomplete);
        assert!(!unknown.trace.contains(&"AXValue"));
        let mut ordinary = Fake::new(None);
        assert_eq!(
            tab_child_value(ordinary.read(AttrStatus::Known("AXTab".into())))
                .0
                .as_deref(),
            Some("ordinary")
        );
    }

    #[test]
    fn chrome_button_reuses_the_classification_subrole() {
        let mut reader = Fake::new(Some("AXCloseButton"));
        let attrs = reader.read(AttrStatus::Known("AXButton".into()));
        assert_eq!(
            window_chrome_button_description("AXButton", "", "", || attrs.subrole.clone()),
            Some("close button")
        );
        assert_eq!(reader.trace, ["batch", "AXValue"]);
    }

    #[test]
    fn scrollbar_position_reads_require_a_known_scrollbar_role() {
        for role in [None, Some("AXButton"), Some("AXSecureTextField")] {
            let trace = RefCell::new(Vec::new());
            let value = super::super::scroll_area::scrollbar_value(
                || {
                    trace.borrow_mut().push("AXRole");
                    role.map(str::to_owned)
                },
                || {
                    trace.borrow_mut().push("AXValue");
                    Some(0.5)
                },
            );
            assert_eq!(value, None);
            assert_eq!(*trace.borrow(), ["AXRole"]);
        }
        let trace = RefCell::new(Vec::new());
        let value = super::super::scroll_area::scrollbar_value(
            || {
                trace.borrow_mut().push("AXRole");
                Some("AXScrollBar".into())
            },
            || {
                trace.borrow_mut().push("AXValue");
                Some(0.5)
            },
        );
        assert_eq!(value, Some(0.5));
        assert_eq!(*trace.borrow(), ["AXRole", "AXValue"]);
    }

    fn fake_node(role: AttrStatus, subrole: Option<&str>) -> (AXNode, Vec<&'static str>) {
        let mut reader = Fake::new(subrole);
        let attrs = reader.read(role);
        let mut node = display_node("AXTextField", attrs, 0, 0, None, false);
        node.element_index = Some(0);
        node.value_state = node.value.clone();
        (node, reader.trace)
    }

    #[test]
    fn structured_elements_and_since_follow_the_same_rule() {
        let (secure, trace) = fake_node(
            AttrStatus::Known("AXTextField".into()),
            Some("AXSecureTextField"),
        );
        assert!(!trace.contains(&"AXValue"));
        let line = format_node_line(&secure);
        assert!(line.contains("[redacted]"));
        assert_eq!(
            crate::tools::test_element_entry(&secure, None, None)["value"],
            "[redacted]"
        );
        let (unknown, trace) = fake_node(crate::ax::content_attrs::FAILED_TEST, None);
        assert!(!trace.contains(&"AXValue"));
        assert!(!format_node_line(&unknown).contains("ordinary"));
        assert!(crate::tools::test_element_entry(&unknown, None, None)
            .get("value")
            .is_none());
        let ctx = ViewContext {
            pid: 260105,
            window_id: 260105,
            key: ViewKey {
                query: None,
                max_elements: 250,
                max_depth: None,
            },
            focus_probe: None,
        };
        let mut before = json!({"snapshot_id":"s00de1001", "tree_markdown":line});
        let mut content = vec![Content::text(before["tree_markdown"].as_str().unwrap())];
        apply(
            &ViewOptions::from_args(&json!({})).unwrap(),
            &ctx,
            &mut content,
            &mut before,
        );
        let mut after =
            json!({"snapshot_id":"s00de1002", "tree_markdown":format_node_line(&secure)});
        apply(
            &ViewOptions::from_args(&json!({"since":"s00de1001"})).unwrap(),
            &ctx,
            &mut content,
            &mut after,
        );
        assert_eq!(after["since_status"], "no_change");
        assert!(!after.to_string().contains("ordinary"));
    }

    #[test]
    fn deferred_dense_rows_use_the_same_policy() {
        let mut rows = vec![DeferredRow {
            anchor: 0,
            seq: 0,
            position: 0,
            collection: 0,
            element: unsafe { RetainedElement::retain(0) },
            depth: 0,
            parent_index: None,
            parent_role: "AXTable".into(),
            label_path: LabelPath::Inactive,
            in_web_content: false,
            visible: true,
            walked: None,
        }];
        let skipped = dense_rows::walk_deferred(&mut rows, false, |_| {
            let (node, trace) = fake_node(
                AttrStatus::Known("AXTextField".into()),
                Some("AXSecureTextField"),
            );
            assert!(!trace.contains(&"AXValue"));
            Chunk {
                lines: vec![(0, format_node_line(&node))],
                nodes: vec![node],
            }
        });
        assert!(!skipped);
        let chunk = rows[0].walked.as_ref().unwrap();
        assert!(chunk.lines[0].1.contains("[redacted]"));
        assert_eq!(chunk.nodes[0].value.as_deref(), Some("[redacted]"));
    }
}

/// Trimmed text, or `None` when empty.
/// A row whose attribute read ran past the walk's deadline: its role only,
/// with no index and no children, as for a content read that timed out.
#[allow(clippy::too_many_arguments)]
fn push_timed_out_row(
    role: String,
    element: usize,
    depth: usize,
    parent_index: Option<usize>,
    in_web_content: bool,
    role_read: bool,
    nodes: &mut Vec<AXNode>,
    lines: &mut Vec<(usize, String)>,
) {
    note_skipped();
    let mut node = role_only_node(role, element, depth, parent_index, in_web_content);
    node.canonical.role_read = role_read;
    lines.push((depth, format_node_line(&node)));
    nodes.push(node);
}

fn trimmed_label(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// A scrollable area's own (title, description, value, identifier) for an
/// exhaustive walk, and whether they show everything a selector could match
/// (false when a label read failed).
type ScrollAreaLabels = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn scroll_area_labels(attrs: super::content_attrs::ContentAttrs) -> (ScrollAreaLabels, bool) {
    (
        (
            trimmed_label(attrs.title),
            trimmed_label(attrs.description),
            trimmed_label(attrs.value),
            attrs.identifier,
        ),
        !attrs.read_failed,
    )
}

fn role_only_node(
    role: String,
    element_ptr: usize,
    depth: usize,
    parent_element_index: Option<usize>,
    in_web_content: bool,
) -> AXNode {
    AXNode {
        owned_row_label: None,
        canonical: Default::default(),
        element_index: None,
        role,
        title: None,
        value: None,
        description: None,
        identifier: None,
        help: None,
        actions: Vec::new(),
        element_ptr,
        depth,
        parent_element_index,
        frame: None,
        value_state: None,
        value_description: None,
        min_value: None,
        max_value: None,
        enabled: None,
        selected: None,
        in_web_content,
    }
}

/// A display-only row for a container an ordinary walk collapses: its
/// labels and value (redacted like any other row's), no index.
fn display_node(
    role: &str,
    attrs: ContentAttrs,
    element: usize,
    depth: usize,
    parent_element_index: Option<usize>,
    in_web_content: bool,
) -> AXNode {
    let label = |text: Option<String>| {
        text.map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
    };
    let canonical = super::vocabulary::CanonicalEvidence {
        role_read: true,
        subrole: (&attrs.subrole_status).into(),
        secure: (attrs.secure || !attrs.value_unknown).then_some(attrs.secure),
        copied_value: None,
        classification_failures: attrs.classification_failures.clone(),
        value_settable_observed_at: None,
        states: Default::default(),
    };
    let title = label(attrs.title);
    let description = label(attrs.description);
    let meta = FieldSecurityMeta {
        role,
        subrole: None,
        title: title.as_deref(),
        description: description.as_deref(),
        placeholder: attrs.placeholder.as_deref(),
        identifier: attrs.identifier.as_deref(),
        help: attrs.help.as_deref(),
        is_password_flag: false,
    };
    let (value, _) = redact_if_secure(&meta, label(attrs.value), None);
    AXNode {
        canonical,
        title,
        description,
        value,
        identifier: attrs.identifier,
        help: attrs.help,
        ..role_only_node(
            role.to_owned(),
            element,
            depth,
            parent_element_index,
            in_web_content,
        )
    }
}

fn is_web_content_role(role: &str) -> bool {
    let normalized = role
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized.contains("webarea") || normalized.contains("documentweb") || normalized == "document"
}

#[cfg(test)]
mod web_content_role_tests {
    use super::{format_node_line, is_web_content_role, role_only_node};

    #[test]
    fn a_timed_out_content_read_emits_an_unaddressable_role_only_node() {
        let node = role_only_node("AXButton".into(), 0, 2, Some(3), false);
        assert_eq!(format_node_line(&node), "- AXButton");
        assert_eq!(node.element_index, None);
        assert!(node.actions.is_empty());
        assert_eq!(node.frame, None);
        assert_eq!(node.depth, 2);
        assert_eq!(node.parent_element_index, Some(3));
    }

    #[test]
    fn custom_action_names_cannot_split_or_escape_a_tree_row() {
        let mut node = role_only_node("AXButton".into(), 0, 0, None, false);
        node.element_index = Some(4);
        node.actions = vec!["Name:Reveal \"details\"\nTarget:inspector\nSelector:perform:".into()];
        assert_eq!(
            format_node_line(&node),
            "- [4] AXButton [actions=[reveal \\\"details\\\"]]"
        );
    }

    #[test]
    fn recognizes_native_web_document_roles_without_marking_app_chrome() {
        for role in ["AXWebArea", "AXDocumentWeb", "document"] {
            assert!(is_web_content_role(role), "{role} must start web trust");
        }
        for role in ["AXWindow", "AXButton", "AXToolbar"] {
            assert!(!is_web_content_role(role), "{role} stays native");
        }
    }
}

fn format_node_line(node: &AXNode) -> String {
    format_node_line_inner(node, true)
}

fn format_node_line_unbounded(node: &AXNode) -> String {
    format_node_line_inner(node, false)
}

const MAX_NODE_TEXT_CHARS: usize = 1_000;
const MAX_NODE_LINE_BYTES: usize = 64 * 1_024;
/// A capped value keeps this many leading characters; the rest of the cap
/// goes to its end. A field's newest text is at its end (a terminal's last
/// output, text typed at the end of a document), so a head-only cut hid
/// exactly what an agent reads back after typing.
const VALUE_HEAD_CHARS: usize = 250;

fn escape_node_char(sanitized: &mut String, character: char) {
    match character {
        '\n' => sanitized.push_str("\\n"),
        '\r' => sanitized.push_str("\\r"),
        _ => sanitized.push(character),
    }
}

fn sanitize_node_text(text: &str, char_limit: Option<usize>) -> String {
    let mut sanitized = String::new();
    let mut truncated = false;
    for (chars_seen, character) in text.chars().enumerate() {
        if char_limit == Some(chars_seen) {
            truncated = true;
            break;
        }
        escape_node_char(&mut sanitized, character);
    }
    if truncated {
        sanitized.push('…');
    }
    sanitized
}

/// A value for its tree line: within the cap as is, otherwise its first
/// [`VALUE_HEAD_CHARS`] and last characters around one `…`, so both the
/// start and the newest text stay readable.
fn sanitize_node_value(text: &str, char_limit: Option<usize>) -> String {
    let Some(limit) = char_limit else {
        return sanitize_node_text(text, None);
    };
    let total = text.chars().count();
    if total <= limit {
        return sanitize_node_text(text, None);
    }
    let head = VALUE_HEAD_CHARS.min(limit);
    let tail = limit - head;
    let mut sanitized = String::new();
    for character in text.chars().take(head) {
        escape_node_char(&mut sanitized, character);
    }
    sanitized.push('…');
    for character in text.chars().skip(total - tail) {
        escape_node_char(&mut sanitized, character);
    }
    sanitized
}

fn format_node_line_inner(node: &AXNode, bounded: bool) -> String {
    let char_limit = bounded.then_some(MAX_NODE_TEXT_CHARS);
    let mut parts = String::new();

    // Common prefix (with or without index).
    let role = sanitize_node_text(&node.role, char_limit);
    if let Some(idx) = node.element_index {
        parts.push_str(&format!("- [{}] {}", idx, role));
    } else {
        parts.push_str(&format!("- {}", role));
    }

    // AXTitle → "title"
    if let Some(t) = &node.title {
        parts.push_str(&format!(" \"{}\"", sanitize_node_text(t, char_limit)));
    }
    // AXValue → = "value"
    if let Some(v) = &node.value {
        parts.push_str(&format!(" = \"{}\"", sanitize_node_value(v, char_limit)));
    }
    // AXDescription → (description) — critical for Calculator digit buttons
    // where AXTitle="" but AXDescription="2". A description that only repeats
    // the title (Chrome's `"Back" (Back)`) is not shown twice.
    if let Some(d) = node
        .description
        .as_ref()
        .filter(|d| node.title.as_ref() != Some(*d))
    {
        parts.push_str(&format!(" ({})", sanitize_node_text(d, char_limit)));
    }

    // Bracketed metadata block (identifier, help, actions).
    if node.element_index.is_some() {
        let mut attrs: Vec<String> = Vec::new();
        if node.enabled == Some(false) {
            attrs.push("disabled".into());
        }
        if let Some(id) = node
            .identifier
            .as_deref()
            .filter(|id| !id.trim().is_empty())
        {
            attrs.push(format!("id={}", sanitize_node_text(id, char_limit)));
        }
        if let Some(h) = &node.help {
            attrs.push(format!("help=\"{}\"", sanitize_node_text(h, char_limit)));
        }
        let shown: Vec<String> = node
            .actions
            .iter()
            .filter(|a| !UNLISTED_ACTIONS.contains(&a.as_str()))
            .map(|a| crate::input::ax_actions::displayed_action(a))
            .collect();
        if !shown.is_empty() {
            attrs.push(format!("actions=[{}]", shown.join(",")));
        }
        if !attrs.is_empty() {
            parts.push_str(" [");
            parts.push_str(&attrs.join(" "));
            parts.push(']');
        }
    }

    if bounded && parts.len() > MAX_NODE_LINE_BYTES {
        let mut end = MAX_NODE_LINE_BYTES;
        while !parts.is_char_boundary(end) {
            end -= 1;
        }
        parts.truncate(end);
        parts.push('…');
    }
    parts
}

thread_local! {
    static MENUS_COLLAPSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static WALK_SKIPPED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static EXHAUSTIVE_NODES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `walk` keeping every visited node in `nodes` (or not) on this thread:
/// layout containers and unlabelled rows stay as display-only rows and text
/// rows are not folded into their parent. Only `verify_state`'s internal
/// read asks for this, so that `elements` lists the whole walked window and
/// a selector matching none of it names nothing there.
pub fn with_exhaustive_nodes<T>(exhaustive: bool, walk: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            EXHAUSTIVE_NODES.with(|flag| flag.set(self.0));
        }
    }
    let _restore = Restore(EXHAUSTIVE_NODES.with(|flag| flag.replace(exhaustive)));
    walk()
}

fn exhaustive_nodes() -> bool {
    EXHAUSTIVE_NODES.with(std::cell::Cell::get)
}

/// Record that the walk on this thread passed over a node or subtree without
/// visiting it (see [`TreeWalkResult::nodes_skipped`]).
fn note_skipped() {
    WALK_SKIPPED.with(|flag| flag.set(true));
}

/// The skip record of one walk. A nested walk on the same thread keeps its
/// own record and hands the outer one back when it ends.
struct SkipLedger {
    previous: bool,
}

impl SkipLedger {
    fn start() -> Self {
        Self {
            previous: WALK_SKIPPED.with(|flag| flag.replace(false)),
        }
    }

    /// Whether this walk skipped anything.
    fn finish(self) -> bool {
        WALK_SKIPPED.with(std::cell::Cell::get)
    }
}

impl Drop for SkipLedger {
    fn drop(&mut self) {
        WALK_SKIPPED.with(|flag| flag.set(self.previous));
    }
}

/// Shown on the menu bar row of a read whose menus were not walked.
pub(crate) const COLLAPSED_MENU_NOTE: &str =
    "(menus collapsed: run an item with invoke_menu path, or pass query to search menu items)";

/// Run `walk` with the menu bar's closed menus collapsed (or not) on this
/// thread. Only `get_window_state` collapses; every other walker keeps the
/// full menu tree.
pub fn with_menus_collapsed<T>(collapse: bool, walk: impl FnOnce() -> T) -> T {
    let previous = MENUS_COLLAPSED.with(|flag| flag.replace(collapse));
    let result = walk();
    MENUS_COLLAPSED.with(|flag| flag.set(previous));
    result
}

/// Actions every web-content element advertises. Listing them on each row
/// cost about a third of a web tree's text and told the caller nothing; the
/// structured `elements` still carry the full list.
const UNLISTED_ACTIONS: &[&str] = &["AXShowMenu", "AXScrollToVisible"];

/// Associate rendered lines with nodes while every line is still present.
/// Only synthetic summary lines lack a node. Every other line follows the same
/// DFS order, so renumbering or byte capping cannot break the association.
fn align_line_nodes(lines: &[(usize, String)], nodes: &[AXNode]) -> Vec<Option<usize>> {
    let mut next_node = 0;
    lines
        .iter()
        .map(|(_, line)| {
            if dense_rows::is_summary_line(line) || next_node >= nodes.len() {
                return None;
            }
            let index = next_node;
            next_node += 1;
            Some(index)
        })
        .collect()
}

/// Drop static-text leaf rows that exactly repeat text already shown on their
/// parent row (a web button "Save case" with a child text "Save case"). The
/// rows keep their element indices; only the rendering omits them. Compare
/// original node text so truncation can never turn distinct text into an echo.
fn prune_echo_text_rows(
    lines: &mut Vec<(usize, String)>,
    line_nodes: &mut Vec<Option<usize>>,
    nodes: &[AXNode],
) {
    let mut keep = vec![true; lines.len()];
    for i in 0..lines.len() {
        let (depth, line) = &lines[i];
        if echo_text_of(line).is_none() {
            continue;
        }
        let is_leaf = lines.get(i + 1).is_none_or(|(next, _)| next <= depth);
        if !is_leaf || *depth == 0 {
            continue;
        }
        let parent = lines[..i].iter().rposition(|(d, _)| *d + 1 == *depth);
        let (Some(child_index), Some(parent)) = (line_nodes[i], parent) else {
            continue;
        };
        let Some(parent_index) = line_nodes[parent] else {
            continue;
        };
        let Some(text) = nodes[child_index].value.as_deref() else {
            continue;
        };
        let parent = &nodes[parent_index];
        let displayed_help = parent
            .element_index
            .is_some()
            .then_some(parent.help.as_deref())
            .flatten();
        let repeats = [
            parent.title.as_deref(),
            parent.value.as_deref(),
            displayed_help,
        ]
        .into_iter()
        .flatten()
        .any(|candidate| candidate == text);
        if repeats {
            keep[i] = false;
        }
    }
    let mut index = 0;
    lines.retain(|_| {
        let kept = keep[index];
        index += 1;
        kept
    });
    let mut index = 0;
    line_nodes.retain(|_| {
        let kept = keep[index];
        index += 1;
        kept
    });
}

/// The text of a bare static-text row: `- [N] AXStaticText = "text"` with
/// nothing else on the row.
fn echo_text_of(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("- ")?;
    let rest = match rest.strip_prefix('[') {
        Some(after) => after.split_once("] ")?.1,
        None => rest,
    };
    let text = rest.strip_prefix("AXStaticText = \"")?.strip_suffix('"')?;
    (!text.is_empty() && !text.contains('"')).then_some(text)
}

fn render_lines(lines: &[(usize, String)]) -> String {
    let mut out = String::new();
    for (depth, line) in lines {
        for _ in 0..*depth {
            out.push_str("  ");
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn filter_tree_with_original_text(
    lines: &[(usize, String)],
    line_nodes: &[Option<usize>],
    nodes: &[AXNode],
    query: &str,
) -> String {
    let output = render_lines(lines);
    let mut search = String::new();
    for (index, (_, output_line)) in lines.iter().enumerate() {
        if let Some(node_index) = line_nodes[index] {
            search.push_str(&format_node_line_unbounded(&nodes[node_index]));
        } else {
            search.push_str(output_line);
        }
        search.push('\n');
    }
    filter_tree_pair(&output, &search, query)
}

/// Filter the tree markdown to lines matching `query` plus their ancestor chain.
#[cfg(test)]
fn filter_tree(markdown: &str, query: &str) -> String {
    filter_tree_pair(markdown, markdown, query)
}

fn filter_tree_pair(markdown: &str, search_markdown: &str, query: &str) -> String {
    let needle = query.to_lowercase();
    let lines: Vec<&str> = markdown.lines().collect();
    let search_lines: Vec<&str> = search_markdown.lines().collect();

    let mut current_ancestor: Vec<&str> = Vec::new();
    let mut last_emitted_at: Vec<Option<&str>> = Vec::new();
    let mut output: Vec<&str> = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        if dense_rows::is_summary_line(line) {
            continue;
        }
        let depth = leading_indent_depth(line);

        while current_ancestor.len() <= depth {
            current_ancestor.push("");
            last_emitted_at.push(None);
        }
        for emitted_at in last_emitted_at.iter_mut().skip(depth + 1) {
            *emitted_at = None;
        }
        current_ancestor[depth] = line;

        let search_line = search_lines.get(index).copied().unwrap_or(line);
        if search_line.to_lowercase().contains(&needle) {
            for ancestor_depth in 0..depth {
                let ancestor = current_ancestor[ancestor_depth];
                if ancestor.is_empty() {
                    continue;
                }
                if last_emitted_at[ancestor_depth] == Some(ancestor) {
                    continue;
                }
                last_emitted_at[ancestor_depth] = Some(ancestor);
                output.push(ancestor);
            }
            last_emitted_at[depth] = Some(line);
            output.push(line);
        }
    }

    if output.is_empty() {
        return String::new();
    }
    let mut result = output.join("\n");
    result.push('\n');
    result
}

fn leading_indent_depth(line: &str) -> usize {
    let mut count = 0;
    for ch in line.chars() {
        if ch == ' ' {
            count += 1;
        } else {
            break;
        }
    }
    count / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_that_timed_out_keeps_only_its_role() {
        let (mut nodes, mut lines) = (Vec::new(), Vec::new());
        push_timed_out_row(
            "AXMenuItem".into(),
            0,
            2,
            Some(4),
            false,
            true,
            &mut nodes,
            &mut lines,
        );
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].element_index, None, "no index with unknown state");
        assert_eq!(nodes[0].role, "AXMenuItem");
        assert_eq!(nodes[0].parent_element_index, Some(4));
        assert_eq!(lines.len(), 1);
    }
    use std::cell::Cell;

    #[test]
    fn querying_spliced_rows_keeps_ancestors_and_never_matches_offscreen_hints() {
        let md = "- [0] AXWindow \"Example\"\n  - [1] AXTable \"History\"\n    - (offscreen rows: 2; scroll the list, or pass query to search them)\n    - (11 inactive tabs omitted)\n    - [2] AXRow \"Message\"\n      - AXStaticText = \"needle\"\n  - [3] AXTextArea \"Compose\"\n";
        assert_eq!(filter_tree(md, "needle"), "- [0] AXWindow \"Example\"\n  - [1] AXTable \"History\"\n    - [2] AXRow \"Message\"\n      - AXStaticText = \"needle\"\n");
        assert_eq!(filter_tree(md, "scroll"), "");
        assert_eq!(filter_tree(md, "inactive tabs"), "");
    }

    #[test]
    fn writable_value_controls_are_addressable_without_actions() {
        assert!(is_addressable("AXTextField", false, true, Some(true)));
        assert!(is_addressable("AXButton", true, false, None));
        assert!(!is_addressable("AXStaticText", false, false, Some(true)));
        assert!(!is_addressable("AXMenuItem", true, false, Some(false)));
    }

    #[test]
    fn wave_400b_disabled_window_controls_remain_addressable() {
        assert!(is_addressable("AXButton", true, false, Some(false)));
        assert!(is_addressable("AXTextField", false, true, Some(false)));
    }

    #[test]
    fn wave_400b_node_lines_show_disabled_and_cannot_be_forced_to_split() {
        let mut node = role_only_node("AXButton".into(), 0, 0, None, false);
        node.element_index = Some(4);
        node.enabled = Some(false);
        node.title = Some("first\nsecond\rthird".into());
        node.value = Some(format!("{}tail", "가".repeat(1_001)));
        let line = format_node_line(&node);
        assert!(line.contains("[disabled]"));
        assert!(line.contains("first\\nsecond\\rthird"));
        assert_eq!(line.matches('\n').count(), 0);
        assert_eq!(line.matches('\r').count(), 0);
        assert!(
            line.contains("tail\"") && line.matches('…').count() == 1,
            "a long value keeps its newest text: {line}"
        );
    }

    fn value_node(
        role: &str,
        element_index: Option<usize>,
        title: Option<&str>,
        value: Option<&str>,
    ) -> AXNode {
        let mut node = role_only_node(role.into(), 0, 0, None, false);
        node.element_index = element_index;
        node.title = title.map(str::to_owned);
        node.value = value.map(str::to_owned);
        node
    }

    /// Observe after typing: a long field (a terminal buffer, a document
    /// typed at its end) shows its start and its newest text, Unicode and
    /// symbols included, within the same cap.
    #[test]
    fn a_long_value_keeps_its_start_and_its_newest_text() {
        let start = "start ";
        let newest = "tail-probe 한글✓\nprompt%";
        let value = format!("{start}{}{newest}", "filler line\n".repeat(400));
        let node = value_node("AXTextArea", Some(2), None, Some(&value));
        let line = format_node_line(&node);
        assert!(line.contains("= \"start filler line\\n"), "{line}");
        assert!(line.ends_with("tail-probe 한글✓\\nprompt%\""), "{line}");
        assert_eq!(line.matches('…').count(), 1, "{line}");
        assert!(!line.contains('\n'), "{line}");
        let quoted = line.split(" = \"").nth(1).unwrap().trim_end_matches('"');
        let unescaped = quoted.replace("\\n", "\n");
        assert_eq!(unescaped.chars().count(), MAX_NODE_TEXT_CHARS + 1);
        // The search form keeps the whole value.
        let full = format_node_line_unbounded(&node);
        assert!(full.contains(&value.replace('\n', "\\n")), "{full}");
        assert!(!full.contains('…'));
    }

    #[test]
    fn values_at_the_cap_are_whole_and_one_over_is_elided_once() {
        let at_cap = "가".repeat(MAX_NODE_TEXT_CHARS);
        let line = format_node_line(&value_node("AXTextArea", Some(0), None, Some(&at_cap)));
        assert!(line.contains(&at_cap) && !line.contains('…'), "{line}");

        let over = format!("{}✓", "가".repeat(MAX_NODE_TEXT_CHARS));
        let line = format_node_line(&value_node("AXTextArea", Some(0), None, Some(&over)));
        assert_eq!(line.matches('…').count(), 1);
        assert!(line.ends_with("✓\""), "{line}");
        let quoted = line.split(" = \"").nth(1).unwrap().trim_end_matches('"');
        assert_eq!(quoted.chars().count(), MAX_NODE_TEXT_CHARS + 1);
    }

    #[test]
    fn titles_keep_the_head_cap() {
        let title = format!("{}tail", "a".repeat(MAX_NODE_TEXT_CHARS));
        let line = format_node_line(&value_node("AXButton", Some(0), Some(&title), None));
        assert!(!line.contains("tail") && line.contains("…\""), "{line}");
    }

    /// Redaction runs before a line is formatted, so neither end of a long
    /// secure value reaches the capped line.
    #[test]
    fn a_long_secure_value_leaks_neither_its_start_nor_its_end() {
        let secret = format!("HEADSECRET{}TAILSECRET", "x".repeat(5_000));
        let meta = FieldSecurityMeta {
            role: "AXSecureTextField",
            subrole: None,
            title: None,
            description: None,
            placeholder: None,
            identifier: None,
            help: None,
            is_password_flag: false,
        };
        let (value, state) = redact_if_secure(&meta, Some(secret.clone()), Some(secret));
        let mut field = value_node("AXSecureTextField", Some(0), None, value.as_deref());
        field.value_state = state;
        let line = format_node_line(&field);
        assert!(
            !line.contains("HEADSECRET") && !line.contains("TAILSECRET"),
            "{line}"
        );
        assert!(!format_node_line_unbounded(&field).contains("SECRET"));
    }

    #[test]
    fn actionless_control_roles_are_pressable_but_display_roles_are_not() {
        for role in ["AXButton", "AXRadioButton", "AXCheckBox", "AXLink"] {
            assert!(is_actionless_control_role(role), "{role}");
        }
        for role in ["AXStaticText", "AXImage", "AXGroup", "AXCell", "AXRow"] {
            assert!(!is_actionless_control_role(role), "{role}");
        }

        for role in [
            "AXTextField",
            "AXTextArea",
            "AXSecureTextField",
            "AXComboBox",
            "AXSlider",
            "AXStepper",
            "AXCheckBox",
            "AXRadioButton",
        ] {
            assert!(role_supports_value_addressing(role), "{role}");
        }
        for role in ["AXStaticText", "AXImage", "AXWindow", "AXGroup"] {
            assert!(!role_supports_value_addressing(role), "{role}");
        }
    }

    #[test]
    fn control_state_reads_are_gated_by_actionability() {
        let reads = Cell::new(0);
        let display_only = read_control_state_if_actionable(false, || {
            reads.set(reads.get() + 1);
            ControlState {
                enabled: Some(true),
                ..ControlState::default()
            }
        });
        assert_eq!(reads.get(), 0, "display-only nodes must not read state");
        assert_eq!(display_only.enabled, None);

        let actionable = read_control_state_if_actionable(true, || {
            reads.set(reads.get() + 1);
            ControlState {
                enabled: Some(true),
                ..ControlState::default()
            }
        });
        assert_eq!(reads.get(), 1, "actionable nodes must read state once");
        assert_eq!(actionable.enabled, Some(true));

        let disabled = read_control_state_if_actionable(true, || ControlState {
            enabled: Some(false),
            ..ControlState::default()
        });
        assert_eq!(disabled.enabled, Some(false));
    }

    #[test]
    fn wave_400b_window_chrome_names_are_mapped_and_read_lazily() {
        for (subrole, expected) in [
            ("AXCloseButton", "close button"),
            ("AXMinimizeButton", "minimize button"),
            ("AXZoomButton", "zoom button"),
            ("AXFullScreenButton", "full screen button"),
        ] {
            assert_eq!(
                window_chrome_button_description("AXButton", "", "", || {
                    Some(subrole.to_owned())
                }),
                Some(expected)
            );
        }
        assert_eq!(
            window_chrome_button_description("AXButton", "", "", || {
                Some("AXToolbarButton".into())
            }),
            None
        );

        let reads = Cell::new(0);
        for (role, title, description) in [
            ("AXButton", "Named", ""),
            ("AXButton", "", "Described"),
            ("AXWindow", "", ""),
        ] {
            assert_eq!(
                window_chrome_button_description(role, title, description, || {
                    reads.set(reads.get() + 1);
                    Some("AXCloseButton".into())
                }),
                None
            );
        }
        assert_eq!(reads.get(), 0, "named or non-button nodes skip AXSubrole");
    }

    #[test]
    fn wave_400b_window_recovery_selects_the_topmost_match_and_checks_deadlines() {
        let ids = [None, Some(22), Some(11), Some(22), None];
        assert_eq!(topmost_window_match(&ids, 22), Some(3));
        assert_eq!(topmost_window_match(&ids, 33), None);
        let at_cap = vec![Some(22); MAX_WINDOW_RECOVERY_ANCESTORS];
        assert_eq!(
            topmost_window_match(&at_cap, 22),
            Some(MAX_WINDOW_RECOVERY_ANCESTORS - 1)
        );

        let now = Instant::now();
        assert!(!recovery_deadline_reached(None, now));
        assert!(recovery_deadline_reached(Some(now), now));
        assert!(!recovery_deadline_reached(
            Some(now + std::time::Duration::from_millis(1)),
            now
        ));

        let calls = Cell::new(0);
        let expired = window_center_hit_if_live(Some(now), now, || {
            calls.set(calls.get() + 1);
            1
        });
        assert_eq!(expired, None);
        assert_eq!(calls.get(), 0, "an expired recovery never starts a hit");
        let live = window_center_hit_if_live(None, now, || {
            calls.set(calls.get() + 1);
            1
        });
        assert_eq!(live, Some(1));
        assert_eq!(calls.get(), 1);
    }
}

#[cfg(test)]
mod lean_row_tests {
    use super::{
        align_line_nodes, echo_text_of, filter_tree_with_original_text, format_node_line,
        prune_echo_text_rows, role_only_node, AXNode, COLLAPSED_MENU_NOTE, MAX_NODE_LINE_BYTES,
    };

    fn rows(lines: &[(usize, &str)]) -> Vec<(usize, String)> {
        lines.iter().map(|(d, l)| (*d, l.to_string())).collect()
    }

    fn node(
        role: &str,
        element_index: Option<usize>,
        title: Option<&str>,
        value: Option<&str>,
        actions: &[&str],
    ) -> AXNode {
        let mut node = role_only_node(role.into(), 0, 0, None, false);
        node.element_index = element_index;
        node.title = title.map(str::to_owned);
        node.value = value.map(str::to_owned);
        node.actions = actions.iter().map(|action| (*action).to_owned()).collect();
        node
    }

    fn prune(lines: &mut Vec<(usize, String)>, nodes: &[AXNode]) -> Vec<Option<usize>> {
        let mut line_nodes = align_line_nodes(lines, nodes);
        prune_echo_text_rows(lines, &mut line_nodes, nodes);
        line_nodes
    }

    fn rendered(nodes: &[AXNode]) -> Vec<(usize, String)> {
        nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (usize::from(index != 0), format_node_line(node)))
            .collect()
    }

    #[test]
    fn wave_400b_echo_text_rows_with_only_partial_parent_matches_are_kept() {
        let mut lines = rows(&[
            (0, "- [0] AXWindow \"Mail\" [actions=[raise]]"),
            (
                1,
                "- [8] AXButton \"Atrium plant service CM-199\" [actions=[press]]",
            ),
            (2, "- [9] AXStaticText = \"Atrium plant service\""),
            (2, "- [10] AXStaticText = \"CM-199\""),
            (1, "- [11] AXHeading \"Messages\""),
            (2, "- [12] AXStaticText = \"Inbox\""),
            (1, "- [13] AXStaticText = \"Mail\""),
        ]);
        let nodes = vec![
            node("AXWindow", Some(0), Some("Mail"), None, &["AXRaise"]),
            node(
                "AXButton",
                Some(8),
                Some("Atrium plant service CM-199"),
                None,
                &["AXPress"],
            ),
            node(
                "AXStaticText",
                Some(9),
                None,
                Some("Atrium plant service"),
                &[],
            ),
            node("AXStaticText", Some(10), None, Some("CM-199"), &[]),
            node("AXHeading", Some(11), Some("Messages"), None, &[]),
            node("AXStaticText", Some(12), None, Some("Inbox"), &[]),
            node("AXStaticText", Some(13), None, Some("Mail"), &[]),
        ];
        prune(&mut lines, &nodes);
        let kept: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(
            kept,
            [
                "- [0] AXWindow \"Mail\" [actions=[raise]]",
                "- [8] AXButton \"Atrium plant service CM-199\" [actions=[press]]",
                "- [9] AXStaticText = \"Atrium plant service\"",
                "- [10] AXStaticText = \"CM-199\"",
                "- [11] AXHeading \"Messages\"",
                "- [12] AXStaticText = \"Inbox\"",
            ]
        );
    }

    #[test]
    fn wave_400b_echo_uses_original_exact_text_and_only_visible_parent_fields() {
        let prefix = "가".repeat(1_000);
        let parent_text = format!("{prefix}parent");
        let child_text = format!("{prefix}child");
        let nodes = vec![
            node("AXButton", Some(0), Some(&parent_text), None, &[]),
            node("AXStaticText", None, None, Some(&child_text), &[]),
        ];
        let mut lines = rendered(&nodes);
        prune(&mut lines, &nodes);
        assert_eq!(lines.len(), 2, "different text after the cap is not echo");

        let nodes = vec![
            node("AXButton", Some(0), Some(&parent_text), None, &[]),
            node("AXStaticText", None, None, Some(&parent_text), &[]),
        ];
        let mut lines = rendered(&nodes);
        prune(&mut lines, &nodes);
        assert_eq!(lines.len(), 1, "exact original text remains an echo");

        let mut display_parent = node("AXHeading", None, Some("Heading"), None, &[]);
        display_parent.help = Some("needle".into());
        let nodes = vec![
            display_parent,
            node("AXStaticText", None, None, Some("needle"), &[]),
        ];
        let mut lines = rendered(&nodes);
        prune(&mut lines, &nodes);
        assert_eq!(lines.len(), 2, "hidden display-only help is not visible");

        let mut indexed_parent = node("AXHeading", Some(0), Some("Heading"), None, &[]);
        indexed_parent.help = Some("needle".into());
        let nodes = vec![
            indexed_parent,
            node("AXStaticText", None, None, Some("needle"), &[]),
        ];
        let mut lines = rendered(&nodes);
        prune(&mut lines, &nodes);
        assert_eq!(lines.len(), 1, "indexed help is rendered and can be echo");
    }

    #[test]
    fn wave_400b_query_uses_the_aligned_original_after_duplicate_pruning() {
        let prefix = "x".repeat(1_000);
        let first = format!("{prefix}first");
        let second = format!("{prefix}second");
        let nodes = vec![
            node("AXButton", Some(0), Some(&first), None, &[]),
            node("AXStaticText", None, None, Some(&first), &[]),
            node("AXStaticText", None, None, Some(&second), &[]),
        ];
        let mut lines = rendered(&nodes);
        let line_nodes = prune(&mut lines, &nodes);
        assert_eq!(lines.len(), 2);
        assert_eq!(line_nodes, vec![Some(0), Some(2)]);
        let filtered = filter_tree_with_original_text(&lines, &line_nodes, &nodes, "second");
        assert!(filtered.contains("AXStaticText"), "{filtered}");
    }

    #[test]
    fn wave_400b_query_past_the_cap_finds_indexed_and_display_only_nodes() {
        let long_text = format!("{}needle", "x".repeat(1_001));
        let long_title = format!("{}action", "y".repeat(1_001));
        let nodes = vec![
            node("AXWindow", Some(0), Some("Root"), None, &[]),
            node("AXStaticText", None, None, Some(&long_text), &[]),
            node("AXButton", Some(2), Some(&long_title), None, &[]),
        ];
        let lines = rendered(&nodes);
        let line_nodes = align_line_nodes(&lines, &nodes);
        let text = filter_tree_with_original_text(&lines, &line_nodes, &nodes, "needle");
        assert!(text.contains("AXWindow"), "{text}");
        assert!(text.contains("AXStaticText"), "{text}");
        let action = filter_tree_with_original_text(&lines, &line_nodes, &nodes, "action");
        assert!(action.contains("[2] AXButton"), "{action}");
    }

    #[test]
    fn wave_400b_alignment_handles_menu_notes_and_summary_lines() {
        let nodes = vec![
            node("AXMenuBar", Some(0), Some("Menu"), None, &[]),
            node("AXStaticText", None, None, Some("Child"), &[]),
        ];
        let mut lines = rendered(&nodes);
        lines[0].1 = format!("{} {COLLAPSED_MENU_NOTE}", lines[0].1);
        lines.insert(
            1,
            (
                1,
                "- (offscreen rows: 2; scroll the list, or pass query to search them)".into(),
            ),
        );
        let line_nodes = align_line_nodes(&lines, &nodes);
        assert_eq!(line_nodes, vec![Some(0), None, Some(1)]);
        let filtered = filter_tree_with_original_text(&lines, &line_nodes, &nodes, "child");
        assert!(filtered.contains("AXMenuBar"), "{filtered}");
        assert!(filtered.contains("AXStaticText"), "{filtered}");
    }

    #[test]
    fn wave_400b_alignment_survives_renumbering_a_byte_capped_row() {
        let custom = format!("Name:{}\nTarget:0x0\nSelector:(null)", "x".repeat(200_000));
        let mut first = node("AXButton", Some(100), Some("Root"), None, &[]);
        first.actions = vec![custom];
        let mut nodes = vec![
            first,
            node("AXStaticText", None, None, Some("needle"), &[]),
            node("AXStaticText", None, None, Some("Root"), &[]),
        ];
        let mut lines = rendered(&nodes);
        lines[0].1 = lines[0].1.replacen("- [100]", "- [2]", 1);
        nodes[0].element_index = Some(2);
        assert_ne!(
            lines[0].1,
            format_node_line(&nodes[0]),
            "the fixture must reproduce renumbering across the byte cap"
        );

        let line_nodes = prune(&mut lines, &nodes);
        assert_eq!(lines.len(), 2, "the exact echo is still pruned");
        assert_eq!(line_nodes, vec![Some(0), Some(1)]);
        let filtered = filter_tree_with_original_text(&lines, &line_nodes, &nodes, "needle");
        assert!(filtered.contains("AXStaticText"), "{filtered}");
    }

    #[test]
    fn wave_400b_node_line_byte_cap_covers_long_and_numerous_actions() {
        let custom = format!("Name:{}\nTarget:0x0\nSelector:(null)", "가".repeat(200_000));
        let mut long_action = node("AXButton", Some(0), None, None, &[]);
        long_action.actions = vec![custom];
        let line = format_node_line(&long_action);
        assert!(line.len() <= MAX_NODE_LINE_BYTES + '…'.len_utf8());
        assert!(line.ends_with('…'));
        assert!(!line.contains('\n') && !line.contains('\r'));

        let mut many_actions = node("AXButton", Some(0), None, None, &[]);
        many_actions.actions = (0..20_000).map(|_| "AXPress".to_owned()).collect();
        let line = format_node_line(&many_actions);
        assert!(line.len() <= MAX_NODE_LINE_BYTES + '…'.len_utf8());
        assert!(line.ends_with('…'));
    }

    /// DSH-08: each walk keeps its own skip record; a walk nested on the same
    /// thread neither clears nor leaks into the outer one.
    #[test]
    fn each_walk_keeps_its_own_skip_record() {
        use super::{note_skipped, SkipLedger};
        let outer = SkipLedger::start();
        note_skipped();
        let inner = SkipLedger::start();
        assert!(!inner.finish(), "a nested walk starts clean");
        assert!(outer.finish(), "the outer skip survives the nested walk");
        let next = SkipLedger::start();
        assert!(!next.finish());
    }

    /// Review finding: a walk that panics must not leave exhaustive mode on
    /// for later walks on the thread.
    #[test]
    fn exhaustive_mode_ends_with_its_walk_even_on_panic() {
        use super::{exhaustive_nodes, with_exhaustive_nodes};
        let unwound = std::panic::catch_unwind(|| {
            with_exhaustive_nodes(true, || -> () { panic!("walk failed") })
        });
        assert!(unwound.is_err());
        assert!(!exhaustive_nodes());
        assert!(with_exhaustive_nodes(true, exhaustive_nodes));
        assert!(!exhaustive_nodes());
    }

    /// Review round 2: a container kept by an exhaustive walk keeps the
    /// Review round 3: a scrollable area keeps its own title in an
    /// exhaustive walk; a failed read or a value hidden by the bar position
    /// leaves the walk incomplete.
    #[test]
    fn a_scroll_area_keeps_its_labels_in_an_exhaustive_walk() {
        use super::super::content_attrs::ContentAttrs;
        let attrs = |title: Option<&str>, value: Option<&str>, read_failed| ContentAttrs {
            title: title.map(str::to_owned),
            copied_value: None,
            value: value.map(str::to_owned),
            placeholder: None,
            description: None,
            identifier: Some("editor-scroll".into()),
            help: None,
            subrole: None,
            subrole_status: crate::ax::content_attrs::AttrStatus::Absent,
            classification_failures: [None, None],
            read_failed,
            secure: false,
            value_unknown: false,
        };
        let ((title, _, _, identifier), complete) =
            super::scroll_area_labels(attrs(Some(" Editor "), None, false));
        assert_eq!(title.as_deref(), Some("Editor"));
        assert_eq!(identifier.as_deref(), Some("editor-scroll"));
        assert!(complete);
        assert!(!super::scroll_area_labels(attrs(Some("Editor"), None, true)).1);
        // Its own value is kept for the label, not hidden by the position.
        let ((_, _, value, _), complete) =
            super::scroll_area_labels(attrs(None, Some(" Notes "), false));
        assert_eq!(value.as_deref(), Some("Notes"));
        assert!(complete);
    }

    /// Review round 2: a container kept by an exhaustive walk keeps the
    /// value that labels it (AXGroup = "Saved").
    #[test]
    fn an_exhaustive_container_row_keeps_its_value() {
        use super::super::content_attrs::ContentAttrs;
        let attrs = ContentAttrs {
            title: None,
            copied_value: None,
            value: Some(" Saved ".into()),
            placeholder: None,
            description: None,
            identifier: None,
            help: None,
            subrole: None,
            subrole_status: crate::ax::content_attrs::AttrStatus::Absent,
            classification_failures: [None, None],
            read_failed: false,
            secure: false,
            value_unknown: false,
        };
        let node = super::display_node("AXGroup", attrs, 0, 1, None, false);
        assert_eq!(node.value.as_deref(), Some("Saved"));
        assert_eq!(node.element_index, None);
    }

    /// Review round 6: a failed AXChildren read at the application root
    /// leaves the walk incomplete even when AXWindows recovers the window.
    #[test]
    fn a_failed_root_children_read_is_recorded() {
        use super::{children_or_skipped, SkipLedger};
        let ledger = SkipLedger::start();
        assert_eq!(children_or_skipped::<u8>(Some(vec![1, 2])), vec![1, 2]);
        assert!(!ledger.finish());
        let ledger = SkipLedger::start();
        assert!(children_or_skipped::<u8>(None).is_empty());
        assert!(ledger.finish());
    }

    #[test]
    fn only_a_failed_children_read_hides_what_lies_below() {
        use super::super::bindings::{
            children_read_succeeded, kAXErrorAttributeUnsupported, kAXErrorCannotComplete,
            kAXErrorFailure, kAXErrorInvalidUIElement, kAXErrorNoValue, kAXErrorSuccess,
        };
        assert!(children_read_succeeded(kAXErrorSuccess));
        assert!(children_read_succeeded(kAXErrorNoValue));
        assert!(children_read_succeeded(kAXErrorAttributeUnsupported));
        assert!(!children_read_succeeded(kAXErrorCannotComplete));
        assert!(!children_read_succeeded(kAXErrorFailure));
        assert!(!children_read_succeeded(kAXErrorInvalidUIElement));
    }

    #[test]
    fn menu_collapsing_is_scoped_to_the_wrapped_walk() {
        use super::{with_menus_collapsed, MENUS_COLLAPSED};
        assert!(!MENUS_COLLAPSED.with(std::cell::Cell::get));
        let inside = with_menus_collapsed(true, || {
            let nested = with_menus_collapsed(false, || MENUS_COLLAPSED.with(std::cell::Cell::get));
            (MENUS_COLLAPSED.with(std::cell::Cell::get), nested)
        });
        assert_eq!(inside, (true, false));
        assert!(!MENUS_COLLAPSED.with(std::cell::Cell::get));
    }

    #[test]
    fn rows_with_children_or_other_fields_are_kept() {
        let mut lines = rows(&[
            (0, "- [1] AXLink \"Docs\""),
            (1, "- [2] AXStaticText = \"Docs\" [actions=[press]]"),
            (1, "- [3] AXStaticText = \"Docs\""),
            (2, "- [4] AXStaticText = \"child\""),
        ]);
        let nodes = vec![
            node("AXLink", Some(1), Some("Docs"), None, &[]),
            node("AXStaticText", Some(2), None, Some("Docs"), &["AXPress"]),
            node("AXStaticText", Some(3), None, Some("Docs"), &[]),
            node("AXStaticText", Some(4), None, Some("child"), &[]),
        ];
        prune(&mut lines, &nodes);
        assert_eq!(lines.len(), 4);
        assert_eq!(echo_text_of("- AXStaticText = \"x\""), Some("x"));
        assert_eq!(echo_text_of("- [2] AXStaticText = \"\""), None);
    }

    #[test]
    fn ax_node_compaction_folds_actionless_rows_and_preserves_indexed_elements() {
        use super::{fold_actionless_text_subtrees, format_node_line, AXNode};
        let mut nodes = vec![
            AXNode {
                owned_row_label: None,
                canonical: Default::default(),
                element_index: Some(0),
                role: "AXWindow".into(),
                title: Some("Messages".into()),
                value: None,
                description: None,
                identifier: None,
                help: None,
                actions: vec!["AXRaise".into()],
                element_ptr: 0,
                depth: 0,
                parent_element_index: None,
                frame: None,
                value_state: None,
                value_description: None,
                min_value: None,
                max_value: None,
                enabled: Some(true),
                selected: None,
                in_web_content: false,
            },
            AXNode {
                owned_row_label: None,
                canonical: Default::default(),
                element_index: None,
                role: "AXRow".into(),
                title: None,
                value: None,
                description: None,
                identifier: None,
                help: None,
                actions: vec![],
                element_ptr: 0,
                depth: 1,
                parent_element_index: Some(0),
                frame: None,
                value_state: None,
                value_description: None,
                min_value: None,
                max_value: None,
                enabled: None,
                selected: None,
                in_web_content: false,
            },
            AXNode {
                owned_row_label: None,
                canonical: Default::default(),
                element_index: None,
                role: "AXStaticText".into(),
                title: None,
                value: Some("Sender".into()),
                description: None,
                identifier: None,
                help: None,
                actions: vec![],
                element_ptr: 0,
                depth: 2,
                parent_element_index: Some(0),
                frame: None,
                value_state: None,
                value_description: None,
                min_value: None,
                max_value: None,
                enabled: None,
                selected: None,
                in_web_content: false,
            },
            AXNode {
                owned_row_label: None,
                canonical: Default::default(),
                element_index: None,
                role: "AXStaticText".into(),
                title: None,
                value: Some("Message preview".into()),
                description: None,
                identifier: None,
                help: None,
                actions: vec![],
                element_ptr: 0,
                depth: 2,
                parent_element_index: Some(0),
                frame: None,
                value_state: None,
                value_description: None,
                min_value: None,
                max_value: None,
                enabled: None,
                selected: None,
                in_web_content: false,
            },
            AXNode {
                owned_row_label: None,
                canonical: Default::default(),
                element_index: Some(1),
                role: "AXTextField".into(),
                title: Some("Compose".into()),
                value: Some("[redacted]".into()),
                description: None,
                identifier: None,
                help: None,
                actions: vec!["AXConfirm".into()],
                element_ptr: 0,
                depth: 1,
                parent_element_index: Some(0),
                frame: None,
                value_state: Some("[redacted]".into()),
                value_description: None,
                min_value: None,
                max_value: None,
                enabled: Some(true),
                selected: None,
                in_web_content: false,
            },
        ];
        let mut lines: Vec<(usize, String)> = nodes
            .iter()
            .map(|n| (n.depth, format_node_line(n)))
            .collect();
        fold_actionless_text_subtrees(&mut lines, &mut nodes);
        assert_eq!(nodes.len(), 3);
        assert_eq!(lines[1], (1, "- AXRow = \"Sender Message preview\"".into()));
        assert_eq!(nodes[2].element_index, Some(1));
        assert_eq!(nodes[2].value.as_deref(), Some("[redacted]"));
    }
}

#[cfg(test)]
pub(crate) fn test_fold_and_filter(mut nodes: Vec<AXNode>, query: &str) -> (Vec<AXNode>, String) {
    let mut lines: Vec<_> = nodes
        .iter()
        .map(|node| (node.depth, format_node_line(node)))
        .collect();
    fold_with_owned_items(&mut lines, &mut nodes);
    let mapping = align_line_nodes(&lines, &nodes);
    let rendered = filter_tree_with_original_text(&lines, &mapping, &nodes, query);
    (nodes, rendered)
}

#[cfg(test)]
mod selectable_row_tests {
    use super::*;
    #[test]
    fn a_flat_selectable_row_without_actions_is_eligible() {
        use crate::ax::write_outcome::Settable;
        assert!(selectable_row_eligible(
            "AXRow",
            Some("AXOutline"),
            true,
            false,
            || Settable::Yes
        ));
    }

    #[test]
    fn a_finder_sidebar_shape_gets_its_owned_label() {
        let row = LabelPath::Open(42);
        let cell = next_label_path(row, Some("AXRow"), Some("AXCell"));
        let text = next_label_path(cell, Some("AXCell"), Some("AXStaticText"));
        assert_eq!(text, LabelPath::Open(42));
        let label =
            row_label_candidate(text, "AXStaticText", [Some("Applications"), None, None]).unwrap();
        let mut indexed = role_only_node("AXRow".into(), 42, 1, None, false);
        indexed.element_index = Some(0);
        indexed.owned_row_label = Some(None);
        let mut child = role_only_node("AXStaticText".into(), 0, 2, Some(0), false);
        child.value = Some(label.into());
        // The AXCell was structurally visited but was not emitted.
        let mut nodes = vec![indexed, child];
        let mut lines = nodes
            .iter()
            .map(|node| (node.depth, format_node_line(node)))
            .collect();
        apply_owned_label_claims(&mut nodes, vec![(42, label.into())]);
        fold_actionless_text_subtrees(&mut lines, &mut nodes);
        assert_eq!(
            nodes[0].owned_row_label.as_ref().unwrap().as_deref(),
            Some("Applications")
        );
    }

    #[test]
    fn foreign_items_and_unreadable_roles_block_the_label_path() {
        for role in [
            Some("AXRow"),
            Some("AXListItem"),
            Some("AXImage"),
            Some("AXCell"),
            None,
            Some("AXUnknown"),
        ] {
            assert_eq!(
                next_label_path(LabelPath::Open(42), Some("AXGroup"), role),
                LabelPath::Blocked
            );
        }
    }

    #[test]
    fn unsettable_failed_or_expired_selected_is_not_eligible() {
        use crate::ax::write_outcome::Settable;
        for outcome in [Settable::No, Settable::Unknown(-1)] {
            assert!(!selectable_row_eligible(
                "AXRow",
                Some("AXTable"),
                true,
                false,
                || outcome
            ));
        }
        let skipped = std::cell::Cell::new(0);
        let outcome = selected_probe_before_deadline(
            true,
            || panic!("expired probe"),
            || skipped.set(skipped.get() + 1),
        );
        assert_eq!(skipped.get(), 1);
        assert!(!selectable_row_eligible(
            "AXRow",
            Some("AXTable"),
            true,
            false,
            || outcome
        ));
    }

    #[test]
    fn outline_row_text_and_other_roles_are_not_eligible() {
        for role in [
            "AXOutlineRow",
            "AXCell",
            "AXStaticText",
            "AXImage",
            "AXButton",
        ] {
            assert!(!selectable_row_eligible(
                role,
                Some("AXOutline"),
                true,
                false,
                || panic!("unqualified role")
            ));
        }
    }

    #[test]
    fn rows_with_actions_or_settable_value_keep_the_existing_rule() {
        for (actions_empty, value_settable) in [(false, false), (true, true)] {
            assert!(!selectable_row_eligible(
                "AXRow",
                Some("AXList"),
                actions_empty,
                value_settable,
                || panic!("existing rule")
            ));
        }
    }

    #[test]
    fn the_probe_runs_once_and_only_for_a_flat_actionless_row() {
        use crate::ax::write_outcome::Settable;
        let calls = std::cell::Cell::new(0);
        let probe = || {
            calls.set(calls.get() + 1);
            Settable::Yes
        };
        assert!(selectable_row_eligible(
            "AXRow",
            Some("AXGrid"),
            true,
            false,
            probe
        ));
        assert_eq!(calls.get(), 1);
        assert!(!selectable_row_eligible(
            "AXCell",
            Some("AXGrid"),
            true,
            false,
            || panic!("cell probe")
        ));
    }

    #[test]
    fn nested_or_cell_rows_are_not_eligible() {
        for parent in [None, Some("AXRow"), Some("AXCell"), Some("AXGroup")] {
            assert!(!selectable_row_eligible(
                "AXRow",
                parent,
                true,
                false,
                || panic!("nested probe")
            ));
        }
    }

    #[test]
    fn deferred_open_and_blocked_paths_keep_their_context() {
        let open = next_label_path(LabelPath::Open(42), Some("AXGroup"), Some("AXStaticText"));
        let blocked = next_label_path(LabelPath::Blocked, Some("AXGroup"), Some("AXStaticText"));
        assert_eq!(
            row_label_candidate(open, "AXStaticText", [Some("owned"), None, None]),
            Some("owned")
        );
        assert_eq!(
            row_label_candidate(blocked, "AXStaticText", [Some("foreign"), None, None]),
            None
        );
        let mut row = role_only_node("AXRow".into(), 42, 0, None, false);
        row.owned_row_label = Some(None);
        apply_owned_label_claims(
            std::slice::from_mut(&mut row),
            vec![
                (999, "wrong".into()),
                (42, "owned".into()),
                (42, "later".into()),
            ],
        );
        assert_eq!(row.owned_row_label.unwrap().as_deref(), Some("owned"));
    }

    #[test]
    fn a_deferred_row_under_a_large_generic_container_is_not_eligible() {
        let deferred = DeferredRow {
            anchor: 0,
            seq: 0,
            position: 0,
            collection: 0,
            element: unsafe { RetainedElement::retain(0) },
            depth: 1,
            parent_index: None,
            parent_role: "AXGroup".into(),
            label_path: LabelPath::Inactive,
            in_web_content: false,
            visible: true,
            walked: None,
        };
        assert!(!selectable_row_eligible(
            "AXRow",
            Some(&deferred.parent_role),
            true,
            false,
            || panic!("generic parent")
        ));
    }

    #[test]
    fn own_text_after_a_foreign_item_still_labels_the_row() {
        let row = LabelPath::Open(42);
        let foreign = next_label_path(row, Some("AXRow"), Some("AXListItem"));
        assert_eq!(
            row_label_candidate(foreign, "AXStaticText", [Some("foreign"), None, None]),
            None
        );
        let own = next_label_path(row, Some("AXRow"), Some("AXStaticText"));
        assert_eq!(
            row_label_candidate(own, "AXStaticText", [Some("own"), None, None]),
            Some("own")
        );
    }

    #[test]
    fn baseline_rows_are_not_newly_eligible_or_labelled() {
        assert!(!selectable_row_eligible(
            "AXButton",
            Some("AXOutline"),
            true,
            false,
            || panic!("baseline never probes")
        ));
        assert_eq!(
            next_label_path(LabelPath::Inactive, Some("AXRow"), Some("AXCell")),
            LabelPath::Inactive
        );
        assert_eq!(
            next_label_path(LabelPath::Inactive, None, None),
            LabelPath::Inactive
        );
        assert_eq!(
            row_label_candidate(
                LabelPath::Inactive,
                "AXStaticText",
                [Some("label"), None, None]
            ),
            None
        );
    }
}

#[cfg(test)]
mod completeness_tests;
#[cfg(test)]
mod evidence_preservation_tests;
#[cfg(test)]
mod walker_seam_tests;

#[cfg(test)]
pub(crate) use walker_seam_tests::{
    canonical_deferred_fixture, canonical_folded_row_fixture, canonical_metadata_fixture,
    canonical_test_node, canonical_walk_and_collect, canonical_walk_fixture,
};

#[cfg(test)]
mod canonical_evidence_tests {
    use super::*;
    use crate::ax::vocabulary::{canonical_role, canonical_states, Subrole};
    use cua_driver_contract::{Role, State};

    #[test]
    fn production_walker_assembles_role_evidence() {
        let (tree, trace) = canonical_walk_fixture("button");
        let node = &tree.nodes[0];
        assert_eq!(
            canonical_role(
                node.canonical.role_read.then_some(node.role.as_str()),
                &node.canonical.subrole,
                node.in_web_content
            ),
            Some(Role::Button)
        );
        assert_eq!(trace.iter().filter(|s| *s == "value 1").count(), 1);
    }
    #[test]
    fn role_failure_is_not_successful_unknown() {
        let (failed, _) = canonical_walk_fixture("role_failure");
        let (unknown, _) = canonical_walk_fixture("role_unknown");
        assert!(!failed.nodes[0].canonical.role_read);
        assert!(unknown.nodes[0].canonical.role_read);
        for (node, want) in [
            (&failed.nodes[0], None),
            (&unknown.nodes[0], Some(Role::Unknown)),
        ] {
            assert_eq!(
                canonical_role(
                    node.canonical.role_read.then_some(node.role.as_str()),
                    &node.canonical.subrole,
                    false
                ),
                want
            );
        }
    }
    #[test]
    fn secure_classification_forbids_value_read() {
        let (tree, trace) = canonical_walk_fixture("secure");
        assert!(!trace.iter().any(|s| s == "value 1"));
        assert_eq!(tree.nodes[0].canonical.secure, Some(true));
        assert_eq!(
            canonical_states(
                &tree.nodes[0].role,
                &tree.nodes[0].canonical,
                None,
                None,
                false
            ),
            Some(vec![State::Secure])
        );
    }
    #[test]
    fn failed_classification_forbids_value_read() {
        let (tree, trace) = canonical_walk_fixture("classification_failure");
        assert!(!trace.iter().any(|s| s == "value 1"));
        assert_eq!(tree.nodes[0].canonical.subrole, Subrole::Unknown);
        assert_eq!(tree.nodes[0].canonical.secure, None);
    }
    #[test]
    fn u25_exception_retains_description_and_identifier_failures() {
        let (tree, trace) = canonical_walk_fixture("u25");
        assert!(trace.iter().any(|s| s == "value 1"));
        for error in &tree.nodes[0].canonical.classification_failures {
            assert!(
                matches!(error,Some(crate::ax::content_attrs::AttrStatus::Failed { error, listed: crate::ax::content_attrs::AttributeListing::Listed }) if *error == kAXErrorFailure)
            );
        }
    }
    #[test]
    fn placeholder_is_not_copied_value_evidence() {
        let (tree, _) = canonical_walk_fixture("placeholder");
        let node = &tree.nodes[0];
        assert_eq!(node.value_state.as_deref(), Some("1"));
        assert_eq!(node.canonical.copied_value, None);
        let entry = crate::tools::test_element_entry(node, None, None);
        assert_eq!(entry["selected"], true);
        assert!(!entry["states"]
            .as_array()
            .unwrap()
            .iter()
            .any(|state| state == "checked"));
        let (tree, _) = canonical_walk_fixture("copied");
        let entry = crate::tools::test_element_entry(&tree.nodes[0], None, None);
        assert!(entry["states"]
            .as_array()
            .unwrap()
            .iter()
            .any(|state| state == "checked"));
    }
    #[test]
    fn folded_row_text_is_not_toggle_evidence() {
        let tree = canonical_folded_row_fixture();
        let row = tree.nodes.iter().find(|node| node.role == "AXRow").unwrap();
        assert!(row
            .owned_row_label
            .as_ref()
            .is_some_and(|label| label.as_deref() == Some("Alpha")));
        assert!(row.canonical.copied_value.is_none());
        assert!(row.canonical.role_read);
    }
    #[test]
    fn deferred_scroll_and_display_paths_keep_role_provenance() {
        let tree = canonical_deferred_fixture();
        for node in &tree.nodes {
            assert!(node.canonical.role_read);
        }
        assert!(tree.nodes.iter().any(|node| node.element_index.is_none()));
        walker_seam_tests::scroll_area_preserves_position_labels_and_read_order();
    }
}
#[cfg(test)]
mod environment_evidence_tests;
#[cfg(test)]
mod environment_setup_tests;
#[cfg(test)]
mod orchestration_preservation_tests;

#[cfg(test)]
pub(crate) fn evidence_output_fixture(search: bool, shared: bool) -> (TreeWalkResult, Vec<String>) {
    walker_seam_tests::dense_case_with_entry(search, false, shared)
}

#[cfg(test)]
pub(crate) fn evidence_deadline_fixture() -> TreeWalkResult {
    environment_setup_tests::run_menu_deadline_case().0.legacy
}
#[cfg(test)]
mod walker_test_support;
