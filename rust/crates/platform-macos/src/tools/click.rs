//! click tool — matches the Swift reference ClickTool.swift.
//!
//! Two addressing modes:
//!
//! * **AX path** (`element_token`): performs AXAction on the cached
//!   element. Fires via AX RPC — the target app never needs to be frontmost.
//!   Extra behaviors vs. the naive dispatch:
//!   - AXTextField / AXTextArea inside web content (AXWebArea): 800 ms post-click delay
//!     for DOM focus settle. Native text inputs return without it.
//!   - AXPopUpButton: appends the list of available options and redirects to set_value.
//!   - Advertised-action warning if the element didn't list the requested action.
//!
//! * **Pixel path** (`x`, `y`): synthesises CGEvent mouse clicks and posts them to
//!   the target pid.  `from_zoom=true` translates zoom-crop pixel coordinates back
//!   to full-window space using the most recent `zoom` context stored per-pid.

use crate::goat::action_latency as latency;

use async_trait::async_trait;
use cua_driver_contract::ClickButton;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_legacy_click_input,
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_action_names, copy_bool_attr, copy_children, copy_element_attr, copy_string_attr,
    element_at_screen_position, element_screen_rect, kAXErrorSuccess, AXUIElementPerformAction,
    AXUIElementRef,
};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};

use super::pixel_route::PixelClickRoute;
use super::ToolState;

pub struct ClickTool {
    state: Arc<ToolState>,
}

impl ClickTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Focus posture for the raw pixel transport after AX hit-testing has failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PixelActivationPolicy {
    /// Standard background delivery: suppress activation of the target.
    SuppressTarget,
    /// Left-click with a concrete window: intentionally make the target
    /// AppKit-active without raising it, while suppressing every other app.
    AllowTargetWithoutRaise,
    /// Explicit foreground rung owns its brief activation and restoration.
    ForegroundAssist,
}

#[derive(Clone, Copy, Debug)]
struct SelectionPixelTarget {
    screen_x: f64,
    screen_y: f64,
    window_x: f64,
    window_y: f64,
}

fn selection_pixel_target(
    screen_center: (f64, f64),
    window_origin: (f64, f64),
) -> SelectionPixelTarget {
    SelectionPixelTarget {
        screen_x: screen_center.0,
        screen_y: screen_center.1,
        window_x: screen_center.0 - window_origin.0,
        window_y: screen_center.1 - window_origin.1,
    }
}

/// Resolve the selectable row/item rather than an actionable child such as a
/// selectable NSTextField. A modified click on the child can be consumed as a
/// text interaction without ever reaching the collection's selection model.
fn nearest_selectable_container_center(element_ptr: usize) -> Option<(f64, f64)> {
    let mut current = element_ptr as AXUIElementRef;
    let mut owns_current = false;

    for _ in 0..8 {
        let role = unsafe { copy_string_attr(current, "AXRole") }.unwrap_or_default();
        if matches!(role.as_str(), "AXRow" | "AXCell" | "AXListItem" | "AXImage")
            && unsafe { copy_bool_attr(current, "AXSelected") }.is_some()
        {
            let center = unsafe { element_screen_rect(current) }
                .map(|rect| (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0));
            if owns_current {
                unsafe { CFRelease(current as CFTypeRef) };
            }
            return center;
        }

        let parent = unsafe { copy_element_attr(current, "AXParent") };
        if owns_current {
            unsafe { CFRelease(current as CFTypeRef) };
        }
        current = parent?;
        owns_current = true;
    }

    if owns_current {
        unsafe { CFRelease(current as CFTypeRef) };
    }
    None
}

/// A plain click on a row already selected can show no change: nothing is
/// watched. A modified click can toggle the row either way.
fn selection_fallback_scope(before: bool, has_modifiers: bool) -> ObservationScope {
    if has_modifiers || !before {
        ObservationScope::Watch
    } else {
        ObservationScope::Nothing
    }
}

fn selection_readback_confirms(
    before: bool,
    after: bool,
    has_modifiers: bool,
    prior_selected_peers_preserved: bool,
) -> bool {
    if has_modifiers {
        after != before && prior_selected_peers_preserved
    } else {
        // A plain click on a row that was already selected changes nothing
        // the driver can read: it may not have landed at all.
        !before && after
    }
}

/// AXPress on a radio button or checkbox that advertises it: a press whose
/// effect the control's own AXValue shows.
fn is_toggle_press(ax_action: &str, role: &str, advertised: &[String]) -> bool {
    ax_action == "AXPress"
        && matches!(role, "AXRadioButton" | "AXCheckBox")
        && advertised.iter().any(|action| action == "AXPress")
}

/// The window an element is in, its title before the action, and the
/// element's own name. Only a title that changed TO that name is evidence:
/// any other change could be the app's own doing.
struct WindowTitleProbe {
    window: AXUIElementRef,
    before: String,
    target: String,
}

impl WindowTitleProbe {
    /// `None` when the element has no name to match.
    ///
    /// # Safety
    ///
    /// `element` must be a valid, live `AXUIElementRef`.
    unsafe fn of(element: AXUIElementRef) -> Option<Self> {
        let target = element_name(element)?;
        let window = copy_element_attr(element, "AXWindow")?;
        let Some(before) = copy_string_attr(window, "AXTitle") else {
            CFRelease(window as CFTypeRef);
            return None;
        };
        // A title that already names the item cannot change to name it.
        if !title_can_come_to_name(&before, &target) {
            CFRelease(window as CFTypeRef);
            return None;
        }
        Some(Self {
            window,
            before,
            target,
        })
    }

    fn read(&self) -> Option<String> {
        unsafe { copy_string_attr(self.window, "AXTitle") }
    }

    /// Whether `now` is a new title naming the pressed element.
    fn names_target(&self, now: &str) -> bool {
        title_names_target(&self.before, now, &self.target)
    }
}

/// Whether a window titled `before` can still come to name `target`
/// ([`title_names_target`] needs a change to it).
fn title_can_come_to_name(before: &str, target: &str) -> bool {
    window_title_name(before) != window_title_name(target)
}

fn title_names_target(before: &str, now: &str, target: &str) -> bool {
    let same = |a: &str, b: &str| window_title_name(a) == window_title_name(b);
    !same(now, before) && same(now, target)
}

/// A window title as the name it shows: without the bidi isolation marks
/// Finder wraps a location in, and without the " — <volume>" suffix it adds
/// for some locations (Finder titles the Desktop "Desktop — Local", the
/// name wrapped in U+2068/U+2069).
fn window_title_name(title: &str) -> String {
    let plain: String = title
        .chars()
        .filter(|c| !matches!(c, '\u{2066}'..='\u{2069}'))
        .collect();
    plain
        .split(" — ")
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// An element's visible name: its title or description, else the text of
/// the first static text inside it (a Finder sidebar cell holds its label as
/// a child), at most three levels down. A text field that is not being
/// edited counts as such a label too (Finder shows a file's name in one).
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef`.
unsafe fn element_name(element: AXUIElementRef) -> Option<String> {
    unsafe fn text_inside(element: AXUIElementRef, depth: usize) -> Option<String> {
        let role = copy_string_attr(element, "AXRole");
        let label_like = role.as_deref() == Some("AXStaticText")
            || (role.as_deref() == Some("AXTextField")
                && copy_bool_attr(element, "AXFocused") != Some(true));
        if label_like {
            if let Some(value) =
                copy_string_attr(element, "AXValue").filter(|v| !v.trim().is_empty())
            {
                return Some(value);
            }
        }
        if depth == 0 {
            return None;
        }
        let children = copy_children(element);
        let found = children
            .iter()
            .find_map(|&child| text_inside(child, depth - 1));
        for &child in &children {
            CFRelease(child as CFTypeRef);
        }
        found
    }
    ["AXTitle", "AXDescription"]
        .iter()
        .find_map(|attr| copy_string_attr(element, attr).filter(|text| !text.trim().is_empty()))
        .or_else(|| text_inside(element, 3))
}

impl Drop for WindowTitleProbe {
    fn drop(&mut self) {
        unsafe { CFRelease(self.window as CFTypeRef) };
    }
}

/// AX errors that AppKit apps return from a press they performed (#3835), so
/// a toggle's value read-back may confirm the press. Illegal argument,
/// invalid element and API disabled mean the request never reached the app;
/// cannot complete means the app did not answer in time, so a short read-back
/// proves nothing. Neither qualifies; an unresolved error after dispatch is
/// reported as unverifiable by `unresolved_perform_error`.
fn press_error_may_have_acted(err: crate::ax::bindings::AXError) -> bool {
    use crate::ax::bindings::{
        kAXErrorActionUnsupported, kAXErrorAttributeUnsupported, kAXErrorFailure,
    };
    [
        kAXErrorFailure,
        kAXErrorAttributeUnsupported,
        kAXErrorActionUnsupported,
    ]
    .contains(&err)
}

/// Whether a toggle's value moving from `before` to `now` is what pressing it
/// does: a checkbox changes state; a radio button becomes selected (a radio
/// turning off was another radio's press).
fn toggle_press_shows(role: &str, before: &str, now: &str) -> bool {
    now != before && (role != "AXRadioButton" || now == "1")
}

// ── Click evidence ───────────────────────────────────────────────────────────

/// A state change read after a click that ties the click to an effect.
/// Details never carry a title or a value from the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClickEvidence {
    /// A text input reads back as the focused element.
    Focused,
    /// A row or item changed from unselected to selected.
    Selected,
    /// A modified click changed a selected row to unselected.
    Deselected,
    /// The window title changed to the clicked item's name (a sidebar
    /// location, a folder that opened in place).
    TitleNamesTarget,
    /// A checkbox or radio button changed value on two reads.
    ToggleChanged,
    /// The sheet holding the pressed button closed: its window, listed
    /// before the press, is gone from a complete snapshot that still lists
    /// the window it was attached to.
    SheetClosed,
}

impl ClickEvidence {
    fn detail(self) -> &'static str {
        match self {
            ClickEvidence::Focused => "the text input reads back as the focused element",
            ClickEvidence::Selected => "row changed from unselected to selected",
            ClickEvidence::Deselected => "row changed from selected to unselected",
            ClickEvidence::TitleNamesTarget => "window title changed to the pressed item's name",
            ClickEvidence::ToggleChanged => "control value changed on two reads",
            ClickEvidence::SheetClosed => "the sheet holding the pressed button closed",
        }
    }

    fn kind(self) -> cua_driver_core::action_record::EvidenceKind {
        match self {
            ClickEvidence::SheetClosed => {
                cua_driver_core::action_record::EvidenceKind::WindowChange
            }
            _ => cua_driver_core::action_record::EvidenceKind::AccessibilityReadback,
        }
    }

    fn kind_name(self) -> &'static str {
        match self.kind() {
            cua_driver_core::action_record::EvidenceKind::WindowChange => "window_change",
            _ => "accessibility_readback",
        }
    }

    fn structured(self) -> serde_json::Value {
        serde_json::json!({ "kind": self.kind_name(), "detail": self.detail() })
    }

    fn record(
        self,
        transport: cua_driver_core::action_record::ActionTransport,
        requested: cua_driver_core::action_record::RequestedDelivery,
        actual: cua_driver_core::action_record::ActualDelivery,
    ) -> cua_driver_core::action_record::ActionExecutionRecord {
        use cua_driver_core::action_record::{ActionEffect, ActionEvidence, ActionExecutionRecord};
        ActionExecutionRecord::builder(ActionEffect::Confirmed, transport, requested)
            .actual_delivery(actual)
            .evidence(ActionEvidence {
                kind: self.kind(),
                detail: self.detail().to_owned(),
            })
            .build()
            .expect("a confirmed click record carries evidence")
    }
}

/// A click task that failed after the action may have run: never a plain
/// error the caller retries blindly.
fn click_task_failure(message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(serde_json::json!({
        "effect": "unverifiable",
        "code": "ax_action_unverified",
        "message": message,
    }))
}

/// The typed record for a click that was delivered and shows no readable
/// change.
fn unverifiable_click_record(
    transport: cua_driver_core::action_record::ActionTransport,
    requested: cua_driver_core::action_record::RequestedDelivery,
    actual: cua_driver_core::action_record::ActualDelivery,
) -> cua_driver_core::action_record::ActionExecutionRecord {
    use cua_driver_core::action_record::{ActionEffect, ActionExecutionRecord};
    ActionExecutionRecord::builder(ActionEffect::Unverifiable, transport, requested)
        .actual_delivery(actual)
        .build()
        .expect("an unverifiable click record is valid")
}

/// No physical input happened between dispatch and the last read, so the
/// change cannot be the person's. `idle_after` is the time since the last
/// physical input at the end of the observation, `elapsed_ms` the time since
/// dispatch started. An unknown read at either end never confirms.
fn input_quiet(idle_before: Option<u64>, idle_after: Option<u64>, elapsed_ms: u64) -> bool {
    idle_before.is_some() && idle_after.is_some_and(|after| after >= elapsed_ms)
}

/// One retained AX element, released on drop.
struct Retained(AXUIElementRef);

impl Retained {
    /// # Safety
    ///
    /// `element` must be a valid, live `AXUIElementRef`.
    unsafe fn retain(element: AXUIElementRef) -> Self {
        core_foundation::base::CFRetain(element as CFTypeRef);
        Retained(element)
    }
}

impl Drop for Retained {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

fn is_selectable_item_role(role: &str) -> bool {
    matches!(role, "AXRow" | "AXCell" | "AXListItem" | "AXImage")
}

/// Every selectable item at or above an element (the same roles and depth
/// the selection fallback walks), each with its AXSelected before the click.
/// Read back on the SAME retained elements, a selection write that landed on
/// another row cannot pass as this one.
struct SelectionSnapshot {
    items: Vec<(Retained, bool)>,
    /// The element is, or sits in, a selectable item role.
    in_item: bool,
}

impl SelectionSnapshot {
    /// # Safety
    ///
    /// `element` must be a valid, live `AXUIElementRef`.
    unsafe fn capture(element: AXUIElementRef) -> Self {
        let mut items = Vec::new();
        let mut in_item = false;
        let mut current = Retained::retain(element);
        for _ in 0..8 {
            let role = copy_string_attr(current.0, "AXRole").unwrap_or_default();
            if is_selectable_item_role(&role) {
                in_item = true;
                if let Some(selected) = copy_bool_attr(current.0, "AXSelected") {
                    items.push((Retained::retain(current.0), selected));
                }
            }
            let Some(parent) = copy_element_attr(current.0, "AXParent") else {
                break;
            };
            current = Retained(parent);
        }
        Self { items, in_item }
    }

    fn before(&self) -> Vec<bool> {
        self.items.iter().map(|(_, selected)| *selected).collect()
    }

    fn after(&self) -> Vec<Option<bool>> {
        self.items
            .iter()
            .map(|(item, _)| unsafe { copy_bool_attr(item.0, "AXSelected") })
            .collect()
    }

    fn transitioned(&self) -> bool {
        selection_transition_confirms(&self.before(), &self.after())
    }
}

/// Some retained item read unselected before and selected now.
fn selection_transition_confirms(before: &[bool], after: &[Option<bool>]) -> bool {
    before
        .iter()
        .zip(after)
        .any(|(before, after)| !*before && *after == Some(true))
}

/// The longest a click's effect is polled for after delivery. A click
/// spends at most this on its read-back, inside the click budget (200 ms
/// for an element click, 100 ms for a point click); an effect that appears
/// later leaves the click unverifiable.
const CLICK_OBSERVATION: std::time::Duration = std::time::Duration::from_millis(60);
const CLICK_OBSERVATION_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// What a click reads around its input to find an effect to report: the
/// selection of the pressed item, a toggle's value, the window title, and
/// the sheet a pressed button sits in. Each read costs accessibility round
/// trips, and the watch that follows costs time, to turn an unverifiable
/// click into a confirmed one. A click reads them only when the caller asks
/// (`read_back`); otherwise it is one action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ClickReads {
    selection: bool,
    toggle: bool,
    title: bool,
    sheet: bool,
}

impl ClickReads {
    const ALL: Self = Self {
        selection: true,
        toggle: true,
        title: true,
        sheet: true,
    };
    const NONE: Self = Self {
        selection: false,
        toggle: false,
        title: false,
        sheet: false,
    };

    /// The reads a click makes for the caller's `read_back` argument.
    pub(super) fn for_click(read_back: bool) -> Self {
        if read_back {
            Self::ALL
        } else {
            Self::NONE
        }
    }
}

/// How long a click's effects are watched for after delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservationScope {
    /// Something the click targets can still change (an unselected item,
    /// a toggle, a window title that may come to name the item): poll only
    /// those properties, returning as soon as one changes, for at most
    /// [`CLICK_OBSERVATION`].
    Watch,
    /// Nothing the click targets holds state (a plain button): the click is
    /// unverifiable at once, with no read and no wait.
    Nothing,
    /// A button inside a sheet: as [`Watch`](Self::Watch), for up to
    /// [`super::sheet_close::SHEET_CLOSE_OBSERVATION`], since the sheet's
    /// window takes longer to go than a property takes to change.
    WatchSheet,
}

/// Which scope a click's probe needs. `selection_before` is each retained
/// selectable item's AXSelected before the click, `toggle` whether a toggle
/// value is watched, `title_candidate` whether a window title probe was
/// taken.
fn observation_scope(
    selection_before: &[bool],
    toggle: bool,
    title_candidate: bool,
) -> ObservationScope {
    // Only an item read unselected can turn selected (a modified click's
    // deselection is judged by its own snapshot, not by this probe).
    let can_select = selection_before.iter().any(|selected| !selected);
    if can_select || toggle || title_candidate {
        ObservationScope::Watch
    } else {
        ObservationScope::Nothing
    }
}

/// Where a click's observation window starts: the end of its own pointer
/// post when it made one; for a watched sheet, when its `AXPress` returned
/// (the sheet's window takes a while to go, and the preparation reads must
/// not eat the window); otherwise dispatch.
fn observation_start(
    scope: ObservationScope,
    started: std::time::Instant,
    own_post_finished: Option<std::time::Instant>,
    sheet_press_at: Option<std::time::Instant>,
) -> std::time::Instant {
    match (own_post_finished, scope, sheet_press_at) {
        (Some(finished), _, _) => finished,
        (None, ObservationScope::WatchSheet, Some(pressed)) => pressed,
        _ => started,
    }
}

/// Poll `read` from `delivered` until it shows evidence, then return it at
/// once if an immediate second read agrees (no settle wait); give up after
/// [`CLICK_OBSERVATION`]. A change first seen after the deadline, or read by
/// a call that returned after it, is not the click's. `Nothing` reads
/// nothing.
fn watch_for_change<C: crate::ax::scroll_area::Clock>(
    scope: ObservationScope,
    delivered: std::time::Instant,
    clock: &C,
    mut read: impl FnMut() -> Option<ClickEvidence>,
) -> Option<ClickEvidence> {
    if scope == ObservationScope::Nothing {
        return None;
    }
    let deadline = delivered
        + match scope {
            ObservationScope::WatchSheet => super::sheet_close::SHEET_CLOSE_OBSERVATION,
            _ => CLICK_OBSERVATION,
        };
    loop {
        if clock.now() > deadline {
            return None;
        }
        // Both reads must return by the deadline: a read that returned after
        // it may show a later change.
        let seen = read();
        if clock.now() > deadline {
            return None;
        }
        if let Some(evidence) = seen {
            if read() == Some(evidence) && clock.now() <= deadline {
                return Some(evidence);
            }
        }
        // The last sleep ends at the deadline, for one last read there.
        let left = deadline.saturating_duration_since(clock.now());
        if left.is_zero() {
            return None;
        }
        clock.sleep(left.min(CLICK_OBSERVATION_POLL));
    }
}

/// What a click may change, read immediately before it is dispatched: the
/// target's own properties only (its selection, toggle value and window
/// title), never the window's tree.
pub(super) struct ClickProbe {
    title: Option<WindowTitleProbe>,
    toggle: Option<(Retained, String, String)>,
    selection: Option<SelectionSnapshot>,
    /// The sheet a pressed button sits in, listed before the click.
    sheet: Option<super::sheet_close::SheetWatch>,
    /// When the `AXPress` a sheet watch observes returned: the sheet's
    /// window is watched from there, not from before the preparation reads.
    sheet_press_at: Option<std::time::Instant>,
    scope: ObservationScope,
    /// The element under the point for a pixel click, as the public `hit`.
    hit: Option<serde_json::Value>,
    /// The physical-input clock, read before any baseline: input during the
    /// baseline reads also disqualifies evidence.
    idle_before: Option<u64>,
    input_from: std::time::Instant,
    /// Dispatch time: the observation deadline counts from here.
    started: std::time::Instant,
    /// For a click that posts its own events: when that post finished
    /// (`None` while it runs); the observation deadline counts from it.
    own_post: Option<Option<std::time::Instant>>,
}

// SAFETY: the probe holds retained AX element references and plain data.
// AXUIElement references are CoreFoundation objects that may be retained,
// released and queried from any thread, and the probe is used by one thread
// at a time (captured on one blocking task, observed on another).
unsafe impl Send for ClickProbe {}

/// The element under a screen point, and whether it is proven inside the
/// exact window (its window id read back as `window_id`). An element whose
/// window id cannot be read is kept, unproven; one read back in another
/// window is dropped.
///
/// # Safety
///
/// Calls into Accessibility for `pid`.
unsafe fn element_in_window_at(
    pid: i32,
    window_id: u32,
    x: f64,
    y: f64,
) -> Option<(Retained, bool)> {
    let element = Retained(element_at_screen_position(pid, x, y)?);
    match crate::ax::exact_target::element_window_id(element.0) {
        Some(id) if id == window_id => Some((element, true)),
        Some(_) => None,
        None => Some((element, false)),
    }
}

/// The public `hit` record of an element a pointer route hit: its role,
/// name and identifier, each omitted when unreadable; `None` when none is.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef`.
unsafe fn pointer_hit(element: AXUIElementRef) -> Option<serde_json::Value> {
    pointer_hit_record(
        copy_string_attr(element, "AXRole"),
        element_name(element),
        copy_string_attr(element, "AXIdentifier"),
    )
}

fn pointer_hit_record(
    role: Option<String>,
    label: Option<String>,
    identifier: Option<String>,
) -> Option<serde_json::Value> {
    let mut hit = serde_json::Map::new();
    for (key, value) in [("role", role), ("label", label), ("identifier", identifier)] {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            hit.insert(key.into(), serde_json::Value::String(value));
        }
    }
    (!hit.is_empty()).then_some(serde_json::Value::Object(hit))
}

impl ClickProbe {
    /// Read what the click may change. `element` is the element the click is
    /// dispatched to (`None`: nothing is watched); `toggle` when it is a
    /// checkbox or radio button whose value shows the press.
    ///
    /// # Safety
    ///
    /// `element`, when given, must be a valid, live `AXUIElementRef`.
    pub(super) unsafe fn capture(
        element: Option<AXUIElementRef>,
        toggle: bool,
        reads: ClickReads,
    ) -> Self {
        let idle_before = super::physical_input_idle_ms();
        let input_from = std::time::Instant::now();
        let selection = element
            .filter(|_| reads.selection)
            .map(|element| SelectionSnapshot::capture(element));
        let toggle = element
            .filter(|_| toggle && reads.toggle)
            .and_then(|element| {
                let role = copy_string_attr(element, "AXRole")?;
                let value =
                    crate::ax::bindings::copy_stringish_attr(element, "AXValue")?.state_value;
                Some((Retained::retain(element), role, value))
            });
        // A window retitles to an item's name when the item navigates (a
        // sidebar location, a folder opened in place): only an element that
        // is or sits in a selectable item gets a title probe.
        let title = element
            .filter(|_| reads.title && selection.as_ref().is_some_and(|s| s.in_item))
            .and_then(|element| WindowTitleProbe::of(element));
        let scope = observation_scope(
            &selection
                .as_ref()
                .map(SelectionSnapshot::before)
                .unwrap_or_default(),
            toggle.is_some(),
            title.is_some(),
        );
        Self {
            title,
            toggle,
            selection,
            sheet: None,
            sheet_press_at: None,
            scope,
            hit: None,
            idle_before,
            input_from,
            started: std::time::Instant::now(),
            own_post: None,
        }
    }

    /// Right before the click posts its own pointer events.
    pub(super) fn own_post_starts(&mut self) {
        self.own_post = Some(None);
    }

    /// Also watch the sheet a button sits in, for an `AXPress` sent to that
    /// button through accessibility (the only route its closing may confirm).
    ///
    /// # Safety
    ///
    /// `element` must be a valid, live `AXUIElementRef`.
    unsafe fn watching_sheet_for_press(mut self, element: AXUIElementRef, ax_action: &str) -> Self {
        if ax_action == "AXPress"
            && copy_string_attr(element, "AXRole").as_deref() == Some("AXButton")
        {
            self.sheet = super::sheet_close::SheetWatch::for_element(element);
            if self.sheet.is_some() {
                self.scope = ObservationScope::WatchSheet;
            }
        }
        self
    }

    /// Right after the `AXPress` returned, for a watched sheet.
    fn press_returned(&mut self) {
        if self.sheet.is_some() {
            self.sheet_press_at = Some(std::time::Instant::now());
        }
    }

    /// Right after the click's own pointer events were posted.
    pub(super) fn own_post_finished(&mut self) {
        if let Some(finished) = self.own_post.as_mut() {
            *finished = Some(std::time::Instant::now());
        }
    }

    /// [`capture`](Self::capture) for a pixel click at a screen point: the
    /// element there counts only when it is inside the exact window, and is
    /// reported as the click's `hit`.
    ///
    /// # Safety
    ///
    /// Calls into Accessibility for `pid`.
    pub(super) unsafe fn at_point(
        pid: i32,
        window_id: u32,
        x: f64,
        y: f64,
        reads: ClickReads,
    ) -> Self {
        let hit = element_in_window_at(pid, window_id, x, y);
        // The hit is reported either way; only a proven element is probed
        // for evidence.
        let element = hit
            .as_ref()
            .filter(|(_, proven)| *proven)
            .map(|(hit, _)| hit.0);
        let toggle = reads.toggle
            && element.is_some_and(|element| {
                matches!(
                    copy_string_attr(element, "AXRole").as_deref(),
                    Some("AXCheckBox" | "AXRadioButton")
                )
            });
        let mut probe = Self::capture(element, toggle, reads);
        probe.hit = hit.and_then(|(hit, _)| pointer_hit(hit.0));
        probe
    }

    /// The element under the point of a pixel click, as the public `hit`.
    pub(super) fn hit(&self) -> Option<serde_json::Value> {
        self.hit.clone()
    }

    fn read_once(&self) -> Option<ClickEvidence> {
        if self
            .selection
            .as_ref()
            .is_some_and(SelectionSnapshot::transitioned)
        {
            return Some(ClickEvidence::Selected);
        }
        if let Some((element, role, before)) = &self.toggle {
            let now = unsafe { crate::ax::bindings::copy_stringish_attr(element.0, "AXValue") }
                .map(|value| value.state_value);
            if now.is_some_and(|now| toggle_press_shows(role, before, &now)) {
                return Some(ClickEvidence::ToggleChanged);
            }
        }
        if let Some(title) = &self.title {
            if title.read().is_some_and(|now| title.names_target(&now)) {
                return Some(ClickEvidence::TitleNamesTarget);
            }
        }
        if self
            .sheet
            .as_ref()
            .is_some_and(|sheet| sheet.presence_now() == super::sheet_close::SheetPresence::Gone)
        {
            return Some(ClickEvidence::SheetClosed);
        }
        None
    }

    /// Watch for an effect (see [`watch_for_change`]); a change counts when
    /// no physical input happened from the baseline reads on.
    pub(super) fn observe(self) -> Option<ClickEvidence> {
        // A click that posts its own events observes from the end of that
        // post; a post whose end was not recorded never confirms.
        if self.own_post.is_some_and(|finished| finished.is_none()) {
            return None;
        }
        let sheet_front = self
            .sheet
            .as_ref()
            .map(super::sheet_close::SheetWatch::front);
        let (evidence, window) = self.observe_change()?;
        let quiet = window.quiet();
        match (evidence, sheet_front) {
            (ClickEvidence::SheetClosed, Some(front)) => {
                super::sheet_close::no_person_could_act(quiet, &front).then_some(evidence)
            }
            _ => quiet.then_some(evidence),
        }
    }

    /// The change read within the deadline, with the input window still to
    /// be checked.
    pub(super) fn observe_change(self) -> Option<(ClickEvidence, InputWindow)> {
        // From delivery: the end of the click's own post when it made one
        // (a double click's two clicks take a while), else dispatch.
        let delivered = observation_start(
            self.scope,
            self.started,
            self.own_post.flatten(),
            self.sheet_press_at,
        );
        let found = watch_for_change(
            self.scope,
            delivered,
            &crate::ax::scroll_area::SystemClock,
            || self.read_once(),
        );
        tracing::debug!(
            ?found,
            scope = ?self.scope,
            title_probe = self.title.is_some(),
            since_delivery_ms = elapsed_ms(delivered),
            "click observation"
        );
        let found = found?;
        Some((
            found,
            InputWindow {
                idle_before: self.idle_before,
                input_from: self.input_from,
            },
        ))
    }
}

/// When physical input would disqualify a click's evidence.
pub(super) struct InputWindow {
    idle_before: Option<u64>,
    input_from: std::time::Instant,
}

impl InputWindow {
    /// No physical input from the baseline reads on, the click's own post
    /// included: input during the post cannot be told apart from the
    /// click's effect. (The per-event-type clock this reads is not advanced
    /// by the driver's own pid-routed events; a route that advances it
    /// fails closed.)
    pub(super) fn quiet(&self) -> bool {
        let idle_now = super::physical_input_idle_ms();
        tracing::debug!(
            idle_before = ?self.idle_before,
            ?idle_now,
            since_baseline_ms = elapsed_ms(self.input_from),
            "click evidence input window"
        );
        input_quiet(self.idle_before, idle_now, elapsed_ms(self.input_from))
    }
}

fn pixel_activation_policy(
    button: &str,
    effective_foreground: bool,
    has_window: bool,
) -> PixelActivationPolicy {
    if effective_foreground {
        PixelActivationPolicy::ForegroundAssist
    } else if button == "left" && has_window {
        PixelActivationPolicy::AllowTargetWithoutRaise
    } else {
        PixelActivationPolicy::SuppressTarget
    }
}

async fn capture_before_pixel_prelude<B, P, Fut, T>(
    front: impl FnOnce() -> Option<i32>,
    baseline: impl FnOnce() -> B,
    prelude: P,
) -> (Option<i32>, B, T)
where
    P: FnOnce(Option<i32>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let prior = front();
    let captured = baseline();
    let prepared = prelude(prior).await;
    (prior, captured, prepared)
}

#[cfg(test)]
use super::pixel_focus::{
    background_pixel_restore_pid, restore_user_focus_with, user_focus_needs_restore,
    PixelRestoreOps,
};

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "click".into(),
        description: "Click a target pid. Prefer `element_token` (from get_window_state): it addresses one snapshot \
            element, works on backgrounded, minimized or off-Space windows, and with count 1 moves no cursor or focus \
            (count 2-3 may click the element's center; see `count`); \
            tokens are scoped to (pid, window_id) and go stale when that window is snapshotted again. `x, y` \
            are window-local pixels of the get_window_state screenshot, sent as CGEvent mouse events to a \
            visible window, for surfaces missing from the tree; `modifier` takes cmd/shift/option/ctrl, and \
            `from_zoom:true` takes zoom-image pixels.\n\
            \n\
            `button` defaults to left; on the AX path \"right\" maps to AXShowMenu and \"middle\" falls back \
            to a pixel click at the element center. `action`: press (default), show_menu, pick, confirm, \
            cancel, open, show_alternate_ui, show_default_ui, or any action the element lists (increment, \
            decrement, custom actions). AXRaise requires foreground delivery. show_alternate_ui reveals an \
            element's hover-only controls in the background. A pressed popup button stays open until an item \
            is chosen or it is dismissed; to choose in one call use `set_value`. A web `<select>` (Chrome, \
            Electron) is not opened in the background: refused with `select_menu_would_raise_window`; use \
            `set_value`.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            // `pid` is conditionally required — needed for window/element clicks
            // but omitted for windowless `scope:"desktop"` clicks — so it is NOT
            // in `required`; the code validates it with a clear error when needed.
            // (Keeps the contract consistent across platforms; see
            // cua_driver_core::tool_schema.)
            "required": [],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it." },
                "pid":           { "type": "integer", "description": "Target process ID. Optional with element_token (it carries the pid)." },
                "window_id":     { "type": "integer", "description": "Target window ID. Omit with element_token." },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "capture_id": { "type": "string", "description": "Optional capture ID from get_window_state or get_desktop_state. With x,y it is consumed atomically; stale or mismatched captures are refused." },
                "x":             { "type": "number",  "description": "X in screenshot pixels (get_window_state PNG, or get_desktop_state PNG for a desktop target)." },
                "y":             { "type": "number",  "description": "Y in screenshot pixels from the image selected by target." },
                "action":        { "type": "string",  "description": "AX action: press (default), show_menu, pick, confirm, cancel, open, show_alternate_ui, show_default_ui. show_alternate_ui reveals hover-only controls on an element that lists it, without the pointer." },
                "button":        {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "description": "Mouse button. Default left."
                },
                // The 1-3 range is enforced before dispatch (element_pointer::
                // click_count) and stated in the description; bounds in the
                // schema would narrow the protocol GOAT's pinned release speaks.
                "count":         {
                    "type": "integer",
                    "description": "Left clicks in one gesture, 1-3. Default 1. With element_token, count 2 sends AXOpen when the element lists it (no mouse clicks; works wherever AX actions work); otherwise, and for count 3, it hands that many left clicks (clickCount 1..N) to the element's center. On the pointer route the point is checked to be on the element in its window (the window must be on screen) before the window is activated; if that check fails with nothing sent yet, the call is refused. The window is then made active (without raising it in the background, where an activation that cannot be confirmed still goes on and is reported as uncertain; fronted in the foreground) and the point is checked again, and once more right before the first click, after the pointer move. A foreground count 2 whose AXOpen provably did nothing falls back to this pointer route after the window was fronted, so its first point check already counts as a later one. A failure after activation, or of the activation itself, sends no click on the element but returns an unverifiable result whose activation field (and events_sent, code element_changed_before_click for the last check) says what had already gone out. Needs action press, the left button and no modifier. Nothing reads back what the clicks opened."
                },
                "read_back": {
                    "type": "boolean",
                    "description": "Default false: the click is one action. It reads nothing before it and watches nothing after it, and a click that was sent returns unverifiable (observe the window to see its effect). true reads the pressed item's selection, a toggle's value, the window title and the sheet holding a pressed button around the input, and watches them for up to 60 ms (600 ms for a sheet), so a click that shows its effect returns confirmed. It costs accessibility reads and that wait."
                },
                "modifier": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Modifier keys: cmd, shift, option/alt, ctrl."
                },
                "from_zoom": {
                    "type": "boolean",
                    "description": "x and y are in the last zoom image for this pid."
                },
                "debug_image_out": {
                    "type": "string",
                    "description": "Write a screenshot with a crosshair at (x, y) to this path. Needs window_id; not with from_zoom."
                },
                "coordinate_space": super::window_points::coordinate_space_schema(),
                "window_basis": super::window_points::window_basis_schema(),
                "delivery_mode": {
                    "type": "string",
                    "enum": ["background", "foreground"],
                    "description": "Default \"background\" (no fronting). \"foreground\" briefly fronts the window, acts, then restores the prior app; needs window_id and is required for modified clicks. Escalate in order, checking state between rungs: AX element_token, the keyboard (press_key Return, arrows, Tab), background pixel, then delivery_mode:\"foreground\" last and only if the user allows it, because it brings the window to the front and, for pixels, moves the user's pointer. A click has no read-back except list-row selection."
                },
                "scope": {
                    "type": "string",
                    "enum": ["window", "desktop"],
                    "description": "Default \"window\". \"desktop\": x,y with no pid/window_id are true screen pixels from get_desktop_state."
                }
            },
            "additionalProperties": false
        }),
        read_only:   false,
        destructive: true,
        idempotent:  false,
        open_world:  true,
    })
}

#[async_trait]
impl Tool for ClickTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if !super::pixel_restore::current_is("click") {
            return super::pixel_restore::invoke(self, args).await;
        }
        let space = match super::window_points::point_space(&args) {
            Ok(space) => space,
            Err(refusal) => return refusal,
        };
        let point = args
            .get("x")
            .and_then(Value::as_f64)
            .zip(args.get("y").and_then(Value::as_f64));
        let result =
            super::window_points::annotate(self.invoke_in(args, space).await, space, point);
        with_screenshot_point(result, space, point)
    }
}

/// A pixel click addressed in screenshot pixels reports the point it aimed
/// at in that space (window-points calls get theirs from
/// `window_points::annotate`); refusals and element clicks carry none.
fn with_screenshot_point(
    mut result: ToolResult,
    space: super::window_points::PointSpace,
    point: Option<(f64, f64)>,
) -> ToolResult {
    let (super::window_points::PointSpace::Screenshot, Some((x, y))) = (space, point) else {
        return result;
    };
    if result.is_error == Some(true) {
        return result;
    }
    if let Some(object) = result
        .structured_content
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    {
        object
            .entry("point")
            .or_insert_with(|| serde_json::json!({"x": x, "y": y, "unit": "screenshot"}));
    }
    result
}

impl ClickTool {
    async fn invoke_in(&self, args: Value, space: super::window_points::PointSpace) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;

        // Every route reads one validated count; nothing is sent for a bad one.
        let count = match super::element_pointer::click_count(&args) {
            Ok(count) => count,
            Err(refusal) => return refusal,
        };
        // What the click reads around its input: nothing unless asked.
        let read_back = args.bool_or("read_back", false);
        let reads = ClickReads::for_click(read_back);

        // ── Window-less screen-absolute branch (scope="desktop") ──────
        // x,y given with NO pid and NO window_id → the coordinates are TRUE
        // SCREEN pixels. This is the foreground, vision-driven desktop-scope
        // path, the macOS peer of the Windows WindowFromPoint click. Gate on the
        // effective scope: under "window" return a structured
        // `desktop_scope_disabled` error (same contract as Windows) rather than
        // silently treating window-local pixels as screen pixels.
        let has_pid = args.get("pid").map(|v| !v.is_null()).unwrap_or(false);
        let has_window_id = args.get("window_id").map(|v| !v.is_null()).unwrap_or(false);
        let has_xy = args.get("x").map(|v| v.is_number()).unwrap_or(false)
            && args.get("y").map(|v| v.is_number()).unwrap_or(false);
        let capture_id = args.opt_str("capture_id");
        if capture_id.is_some() && !has_xy {
            return super::invalid_arguments(
                "click.capture_id requires pixel coordinates x and y.",
            );
        }
        if has_xy && !has_pid && !has_window_id {
            // `scope` is a per-call param now (default "window"); pass
            // scope="desktop" to enable screen-absolute clicks.
            let scope = args.str_or("scope", "window");
            if scope != "desktop" {
                return super::pre_dispatch_refusal(
                    "desktop_scope_disabled",
                    "click: x,y given with no pid/window_id, but scope is \"window\". \
                     Screen-absolute clicks require desktop scope. Pass scope=\"desktop\" \
                     (and use get_desktop_state with scope=\"desktop\" to read true \
                     screen pixels) first."
                        .to_string(),
                );
            }
            let input = match parse_legacy_click_input(&args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let sx_shot = input.x;
            let sy_shot = input.y;
            // ── Desktop-screenshot pixels → logical screen points ──────────────
            // The vision invariant: the pixel an agent reads off the screenshot it
            // was handed is the pixel that gets clicked. `get_desktop_state`
            // returns the display at NATIVE pixels (e.g. 3024×1964 on a 2× Retina
            // display whose logical size is 1512×982), but everything below — the
            // window-under-point hit test (logical CGWindow bounds), the cursor
            // warp, and the CGEvent post — operates in LOGICAL screen points. So
            // x,y arrive in desktop-SCREENSHOT space (what the agent reads off the
            // PNG) and must be divided by the screenshot↔logical ratio, or a
            // center-pixel pick warps to the corner (off by the backing scale).
            let (sx, sy) = if let Some(ref capture_id) = capture_id {
                match self
                    .state
                    .capture_bindings
                    .admit_desktop_click(capture_id, &args, sx_shot, sy_shot)
                {
                    Ok(point) => point,
                    Err(refusal) => return refusal,
                }
            } else {
                super::desktop_screenshot_point(sx_shot, sy_shot)
            };
            let button = match input.button.unwrap_or(ClickButton::Left) {
                ClickButton::Left => "left",
                ClickButton::Right => "right",
                ClickButton::Middle => "middle",
            }
            .to_owned();
            let count = input.count.unwrap_or(1) as usize;
            if count == 0 {
                return super::invalid_arguments("click.count must be at least 1.");
            }
            if let Some(refusal) =
                super::desktop_pointer_guard::check("click", &args, vec![(sx, sy)]).await
            {
                return refusal;
            }
            // Glide the session's agent cursor to the screen point for visibility.
            let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
            crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), sx, sy).await;
            self.state
                .cursor_registry
                .update_position(&cursor_key, sx, sy);
            // Press edge for the agent-cursor overlay (renders a click pulse on
            // viewers via the cursor hook).
            self.state.cursor_registry.note_press(&cursor_key, sx, sy);

            let btn = button.clone();
            let desktop_modifiers: Vec<String> = args.str_array("modifier");
            let result =
                latency::spawn_action_blocking(move || -> anyhow::Result<Option<Value>> {
                    // The element under the point in the window there, read
                    // before the post, as the public hit.
                    let hit = super::desktop_pointer_guard::window_under_point(sx, sy).and_then(
                        |window| unsafe {
                            element_in_window_at(window.pid, window.window_id, sx, sy)
                                .and_then(|(element, _)| pointer_hit(element.0))
                        },
                    );
                    // Desktop scope is explicitly foreground and vision-driven: post
                    // at the global HID tap so WindowServer delivers to the window
                    // actually visible at this point. PID-posting here would silently
                    // turn the foreground contract back into background delivery.
                    let modifier_refs: Vec<&str> =
                        desktop_modifiers.iter().map(String::as_str).collect();
                    crate::input::mouse::click_at_xy_desktop_with_modifiers(
                        sx,
                        sy,
                        count,
                        &btn,
                        &modifier_refs,
                    )?;
                    Ok(hit)
                })
                .await;
            let button_label = match button.as_str() {
                "right" => "right-click",
                "middle" => "middle-click",
                _ => "click",
            };
            return match result {
                Ok(Ok(hit)) => {
                    let mut structured = serde_json::json!({ "path": "cgevent_hid", "verified": false, "effect": "unverifiable" });
                    if let Some(hit) = hit {
                        structured["hit"] = hit;
                    }
                    ToolResult::text(format!(
                        "✅ Sent screen-absolute {button_label} at desktop-pixel \
                         ({sx_shot:.0},{sy_shot:.0}) → screen-point ({sx:.0},{sy:.0}) \
                         (desktop scope; not driver-verified)."
                    ))
                    .with_structured(structured)
                }
                Ok(Err(e)) => ToolResult::error(format!("desktop-scope click failed: {e}")),
                Err(e) => ToolResult::error(format!("task error: {e}")),
            };
        }

        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Resolve this action's cursor key so its click-pulse / glide land on
        // the calling session's cursor, not the shared "default" one.
        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);

        let window_id_arg = args.opt_u64("window_id");
        let resolved = match latency::resolve(|| self.state.snapshots.resolve(pid, &args)) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, element_guard) = resolved.into_parts(window_id_arg);
        if capture_id.is_some() && element_guard.is_some() {
            return super::invalid_arguments(
                "click.capture_id is valid only for the pixel x,y path, not element actions.",
            );
        }
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        let x = args
            .opt_f64("x")
            .or_else(|| args.opt_i64("x").map(|i| i as f64));
        let y = args
            .opt_f64("y")
            .or_else(|| args.opt_i64("y").map(|i| i as f64));
        let action = args.str_or("action", "press");
        if let Err(error) = validate_click_action(&action, element_guard.is_some()) {
            return super::invalid_arguments(format!("click: {error}"));
        }
        // Surface 5: optional `button` arg, default "left" preserves legacy behaviour.
        // Pixel path: routes to left/right/middle CGEvent primitives.
        // AX path: "right" delegates to AXShowMenu (same surface as right_click);
        // "middle" has no AX equivalent and falls back to a pixel middle-click
        // at the element's screen-space center.
        let button_str = args.str_or("button", "left").to_lowercase();
        // delivery_mode: per-call ladder rung. Foreground briefly activates the
        // target for both AX and pixel paths, then restores the prior app.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        // The person is working in this app: nothing is sent.
        if !delivery_mode.is_foreground() {
            if let Err(refusal) = super::gate_user_takeover(pid, window_id) {
                return refusal;
            }
        }
        // Reject unknown buttons explicitly so silent left-click fall-through can't
        // mask a typo. Keep "" → default left for old clients that never sent the field.
        if !matches!(button_str.as_str(), "" | "left" | "right" | "middle") {
            return super::invalid_arguments(format!(
                "click: unknown button \"{button_str}\" — expected one of left, right, middle."
            ));
        }
        let button_str = if button_str.is_empty() {
            "left".to_string()
        } else {
            button_str
        };
        // The window-addressed right and middle primitives post one pair.
        if count > 1 && button_str != "left" {
            return super::invalid_arguments(format!(
                "click: count {count} needs the left button; right and middle clicks send one \
                 click. Nothing was sent."
            ));
        }
        let from_zoom = args.bool_or("from_zoom", false);
        let debug_image_out = args.opt_str("debug_image_out");
        let modifiers: Vec<String> = args.str_array("modifier");

        // PID-routed key transitions can look correct for one AX poll and then
        // collapse to a plain click once AppKit resolves the gesture. Refuse
        // that false-success path. The explicit foreground rung uses physical
        // HID modifier transitions under an exact-window activation guard.
        if !modifiers.is_empty() && (!delivery_mode.is_foreground() || window_id.is_none()) {
            return ToolResult::error(
                "click modifiers require delivery_mode:\"foreground\" and window_id on macOS; \
                 background PID-routed events cannot preserve live modifier-key state",
            )
            .with_structured(serde_json::json!({
                "code": "background_unavailable",
                "effect": "refused",
                "escalation": {
                    "recommended": "foreground",
                    "reason": "macOS modifier clicks require exact-window HID delivery so the target observes live modifier state"
                }
            }));
        }

        if let (Some(idx), Some(wid), Some(element_guard)) =
            (element_index, window_id, element_guard)
        {
            // A double or triple click is one gesture: AXOpen or checked
            // pointer clicks, never a press or selection read back as the
            // effect.
            if count > 1 {
                if action != "press" || !modifiers.is_empty() {
                    return super::invalid_arguments(format!(
                        "click: count {count} on an element needs action press and no \
                         modifier. Nothing was sent."
                    ));
                }
                return super::element_pointer::click_element(
                    &self.state,
                    super::element_pointer::ElementClick {
                        pid,
                        window_id: wid,
                        idx,
                        element: element_guard,
                        count,
                        foreground: delivery_mode.is_foreground(),
                        cursor_key,
                    },
                )
                .await;
            }
            let element_ptr = element_guard.as_ptr();

            // ── Exact-target background gate (macOS background input v1) ──
            // The element branch is semantic AX delivery, except button=middle
            // which falls back to a routed pixel click at the element's center
            // and is therefore held to the stricter WindowPointer rung. Gate
            // BEFORE any cursor/dispatch work so a stale or sibling-owned
            // target refuses instead of acting on the wrong window.
            let mut _mutation_lease = if !delivery_mode.is_foreground() {
                let gate_action = if button_str == "middle" {
                    cua_driver_core::background_input::BackgroundAction::WindowPointer
                } else {
                    cua_driver_core::background_input::BackgroundAction::AxSemantic
                };
                match super::gate_background_window_action(pid, wid, Some(element_ptr), gate_action)
                    .await
                {
                    Ok(lease) => Some(lease),
                    Err(refusal_result) => return refusal_result,
                }
            } else {
                None
            };

            // Surface 5: button=right on the AX path → AXShowMenu (the same surface
            // the dedicated `right_click` tool dispatches). Threads through the
            // identical perform_ax_click code path with the action remapped.
            let effective_action = if button_str == "right" && action == "press" {
                "show_menu".to_string()
            } else {
                action.clone()
            };

            // Resolve the action against what this element lists right now,
            // before the cursor glides or the window can be fronted, so a name
            // it does not offer is refused with nothing sent.
            {
                let action_guard = element_guard.clone();
                let requested = effective_action.clone();
                let foreground = delivery_mode.is_foreground();
                let resolved = latency::spawn_action_blocking(move || {
                    let _span = latency::span(latency::Stage::Preflight);
                    let element = action_guard.as_ptr() as AXUIElementRef;
                    let advertised = unsafe { copy_action_names(element) };
                    crate::input::ax_actions::resolve_element_action(
                        &requested,
                        &advertised,
                        foreground,
                    )
                    .map(|_| ())
                })
                .await;
                if let Ok(Err(error)) = resolved {
                    let code = crate::input::ax_actions::element_action_refusal_code(&error)
                        .unwrap_or("invalid_arguments");
                    return super::pre_dispatch_refusal(code, format!("click: {error}"));
                }
            }

            // A background press or show-menu on a Chromium <select> would
            // open its menu and raise the browser window (CUA-1795): steer to
            // set_value before anything is sent.
            if !delivery_mode.is_foreground()
                && matches!(effective_action.as_str(), "press" | "show_menu")
            {
                let select_guard = element_guard.clone();
                let refusal = latency::spawn_action_blocking(move || {
                    let _span = latency::span(latency::Stage::Preflight);
                    let element = select_guard.as_ptr() as AXUIElementRef;
                    super::set_value::is_chromium_web_select(pid, wid, element).then(|| {
                        super::set_value::chromium_select_open_refusal(pid, wid, Some(idx), element)
                    })
                })
                .await
                .ok()
                .flatten();
                if let Some(refusal) = refusal {
                    return refusal;
                }
            }

            // Animate cursor to element center BEFORE firing AX action,
            // mirroring Swift's `performElementClick` → `animateAndWait(to:)`.
            let center_guard = element_guard.clone();
            // The element's screen rect rides along on the glide so motion
            // styles can time by target size and highlight the target.
            let (center, target_rect) = latency::spawn_action_blocking(move || unsafe {
                let _span = latency::span(latency::Stage::Preflight);
                let el = center_guard.as_ptr() as AXUIElementRef;
                (
                    crate::ax::bindings::element_screen_center(el),
                    element_screen_rect(el),
                )
            })
            .await
            .unwrap_or((None, None));

            // Surface 5: button=middle on the AX path has no AX equivalent.
            // Fall back to a pixel middle-click at the element's screen-space center
            // so the request still produces a real middle-button event (browser tab
            // close, autoscroll, etc.). If we can't resolve a center, error rather
            // than silently degrade to AXPress.
            if button_str == "middle" {
                let (cx, cy) = match center {
                    Some(c) => c,
                    None => {
                        return ToolResult::error(
                            "click(button=middle) on element_token: could not resolve element \
                         center for the pixel-middle-click fallback. Pass x, y directly.",
                        )
                    }
                };
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
                crate::cursor::overlay::animate_cursor_to_target(
                    cursor_key.clone(),
                    cx,
                    cy,
                    target_rect,
                )
                .await;
                self.state
                    .cursor_registry
                    .update_position(&cursor_key, cx, cy);
                self.state.cursor_registry.note_press(&cursor_key, cx, cy);

                let mods_owned = modifiers.clone();
                let foreground = delivery_mode.is_foreground();
                let result = latency::spawn_action_blocking(move || {
                    let m: Vec<&str> = mods_owned.iter().map(String::as_str).collect();
                    if foreground && !m.is_empty() {
                        crate::input::skylight::with_foreground_hid_activation(
                            pid as libc::pid_t,
                            wid,
                            || {
                                crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                                    cx, cy, 1, "middle", &m,
                                )
                            },
                        )
                    } else {
                        crate::input::mouse::middle_click_at_xy(pid, cx, cy, &m)
                    }
                })
                .await;
                return match result {
                    Ok(Ok(())) => ToolResult::text(format!(
                        "✅ Posted middle-click to pid {pid} at element [{idx}] center \
                         (background CGEvent; not driver-verified — confirm via screenshot)."
                    ))
                    .with_structured(serde_json::json!({ "path": "cgevent", "verified": false, "effect": "unverifiable" })),
                    Ok(Err(e)) => ToolResult::error(format!("Middle-click failed: {e}")),
                    Err(e)     => ToolResult::error(format!("Task error: {e}")),
                };
            }

            if let Some((cx, cy)) = center {
                // Pin overlay above target window first.
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
                crate::cursor::overlay::animate_cursor_to_target(
                    cursor_key.clone(),
                    cx,
                    cy,
                    target_rect,
                )
                .await;
                // Keep the registry in sync with the overlay so
                // get_agent_cursor_state reports a truthful position even when
                // the click was dispatched via the AX path (no pixel coords).
                self.state
                    .cursor_registry
                    .update_position(&cursor_key, cx, cy);
                self.state.cursor_registry.note_press(&cursor_key, cx, cy);
            }

            // Finder icon/list items can expose a readable AXSelected state
            // while refusing both AXSelected writes and AXPress. Resolve a
            // verified coordinate frame only for those collection-like
            // elements so perform_ax_click can cross that one failed semantic
            // rung internally and confirm the result by AX read-back.
            let selection_center = if effective_action == "press" {
                let selection_guard = element_guard.clone();
                latency::spawn_action_blocking(move || {
                    let _span = latency::span(latency::Stage::Preflight);
                    crate::input::ax_actions::nearest_container_selection_state(
                        selection_guard.as_ptr(),
                    )
                    .and_then(|_| nearest_selectable_container_center(selection_guard.as_ptr()))
                })
                .await
                .unwrap_or(None)
            } else {
                None
            };
            let mut selection_pixel = if let Some(screen_center) = selection_center {
                super::px_frame::resolve_or_refuse(wid)
                    .await
                    .ok()
                    .map(|frame| {
                        selection_pixel_target(screen_center, (frame.bounds.x, frame.bounds.y))
                    })
            } else {
                None
            };
            // The selection fallback delivers a routed window-local pixel
            // click — a stricter (WindowPointer) rung than the semantic gate
            // above. In background, drop the fallback rather than silently
            // escalate when the pointer rung would refuse (e.g. a
            // minimized/hidden target); the semantic path still runs.
            if selection_pixel.is_some()
                && !delivery_mode.is_foreground()
                && _mutation_lease
                    .as_ref()
                    .expect("background element actions hold the per-pid lease")
                    .gate_again(
                        wid,
                        Some(element_ptr),
                        cua_driver_core::background_input::BackgroundAction::WindowPointer,
                    )
                    .await
                    .is_err()
            {
                selection_pixel = None;
            }

            // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
            // Capture prior frontmost, arm the wildcard suppressor in the
            // snapshot, then arm a targeted suppressor across the AX action
            // itself via FocusGuard. After the action returns, detect any
            // new-window / foreground side-effects and append a one-liner
            // suffix matching Swift's wording.
            let prior_front = apps::frontmost_pid();
            let foreground = delivery_mode.is_foreground();
            let snapshot = if foreground {
                WindowChangeDetector::snapshot_without_suppression(prior_front)
            } else {
                WindowChangeDetector::snapshot(prior_front)
            };

            // Run AX work on a blocking thread (can't block async executor).
            // Use `effective_action` so button=right rewrites press → show_menu.
            let action_clone = effective_action.clone();
            // Thread the resolved session cursor key into the blocking AX path
            // so its ShowFocusRect + ClickPulse land on THIS session's cursor,
            // not the shared "default" one (which would light the wrong cursor
            // and stomp default for a non-default session).
            let ck = cursor_key.clone();
            let selection_modifiers = modifiers.clone();
            // The lease travels with the blocking action and comes back with
            // its result: a cancelled caller cannot release it while the
            // action (and a text input's key hand-back) still runs.
            let action_lease = _mutation_lease.take();
            let result = focus_guard::with_focus_suppressed(
                if foreground { None } else { Some(pid) },
                prior_front,
                "click.AXPress",
                || async move {
                    latency::spawn_action_blocking(move || {
                        let held = action_lease;
                        let element_ptr = element_guard.as_ptr();
                        let run = || -> anyhow::Result<(AxClickOutcome, bool)> {
                            if foreground {
                                let mut outcome = None;
                                let has_modifiers = !selection_modifiers.is_empty();
                                let action = || {
                                    outcome = Some(perform_ax_click(
                                        (element_ptr, idx),
                                        (pid, wid),
                                        &action_clone,
                                        &ck,
                                        selection_pixel,
                                        &selection_modifiers,
                                        foreground,
                                        read_back,
                                    )?);
                                    Ok(())
                                };
                                let fronted = if has_modifiers {
                                    crate::input::skylight::with_foreground_hid_activation(
                                        pid as libc::pid_t,
                                        wid,
                                        action,
                                    )?;
                                    true
                                } else {
                                    crate::input::skylight::with_foreground_assist(
                                        pid as libc::pid_t,
                                        wid,
                                        action,
                                    )?
                                };
                                let outcome = outcome.ok_or_else(|| {
                                    anyhow::anyhow!("foreground AX click did not execute")
                                })?;
                                Ok((outcome, fronted))
                            } else {
                                perform_ax_click(
                                    (element_ptr, idx),
                                    (pid, wid),
                                    &action_clone,
                                    &ck,
                                    selection_pixel,
                                    &selection_modifiers,
                                    false,
                                    read_back,
                                )
                                .map(|outcome| (outcome, false))
                            }
                        };
                        (run(), held)
                    })
                    .await
                },
            )
            .await;
            let result = result.map(|(outcome, held)| {
                _mutation_lease = held;
                outcome
            });

            // Drop the wildcard lease + detect window/foreground side-effects.
            let changes = snapshot.detect_sampled_async().await;

            match result {
                Ok(Ok(((mut msg, suspected_noop, evidence, selection_via_pixel), fronted))) => {
                    msg.push_str(&changes.result_suffix());
                    let result = ToolResult::text(msg).with_structured(ax_click_structured(
                        selection_via_pixel,
                        fronted,
                        evidence,
                        suspected_noop,
                    ));
                    match evidence {
                        Some(evidence) => {
                            use cua_driver_core::action_record::{
                                ActionTransport, ActualDelivery, RequestedDelivery,
                            };
                            let transport = match (selection_via_pixel, fronted) {
                                (false, _) => ActionTransport::MacosAxAction,
                                (true, false) => ActionTransport::MacosCgEventPid,
                                (true, true) => ActionTransport::MacosCgEventHid,
                            };
                            let requested = if delivery_mode.is_foreground() {
                                RequestedDelivery::Foreground
                            } else {
                                RequestedDelivery::Background
                            };
                            let actual = if fronted {
                                ActualDelivery::Foreground
                            } else {
                                ActualDelivery::Background
                            };
                            result.with_action_record(evidence.record(transport, requested, actual))
                        }
                        None => result,
                    }
                }
                Ok(Err(e)) => element_click_failure(&e),
                Err(e) => click_task_failure(format!(
                    "The accessibility click task failed ({e}); the action may have run. Read \
                     fresh state before clicking again."
                )),
            }
        } else if let (Some(mut cx), Some(mut cy)) = (x, y) {
            // ── Pixel path ─────────────────────────────────────────────────

            let mut validated_frame: Option<super::px_frame::WindowPxFrame> = None;
            if matches!(space, super::window_points::PointSpace::WindowPoints(_))
                && (capture_id.is_some() || from_zoom || debug_image_out.is_some())
            {
                return super::invalid_arguments(
                    "coordinate_space window_points cannot be combined with capture_id, from_zoom or \
                     debug_image_out: those name screenshot pixels.",
                );
            }
            if capture_id.is_some() && (from_zoom || debug_image_out.is_some()) {
                return super::invalid_arguments(
                    "click.capture_id is incompatible with from_zoom and debug_image_out; \
                     pass coordinates from the exact source capture directly.",
                );
            }

            // debug_image_out: capture fresh screenshot, overlay crosshair BEFORE
            // any coordinate translation (so it shows received coords in the same
            // space the caller was reasoning in).
            if let Some(ref dbg_path) = debug_image_out {
                if from_zoom {
                    return super::invalid_arguments(
                        "debug_image_out is incompatible with from_zoom — \
                         received (x, y) would be in zoom-crop space, not window-local.",
                    );
                }
                match window_id {
                    None => return super::invalid_arguments("debug_image_out requires window_id."),
                    Some(wid) => {
                        // Session-effective max dimension so debug_image_out
                        // matches the resize the calling session sees in
                        // get_window_state (precedence: session override > global).
                        let max_dim = self.state.session_config.effective_max_image_dimension(
                            args.opt_str("_session_id").as_deref(),
                            &self.state.config.read().unwrap(),
                        );
                        let dbg_path_c = dbg_path.clone();
                        let dbg_result = latency::spawn_action_blocking(move || {
                            let png = crate::capture::screenshot_window_bytes(wid)?;
                            let png = crate::capture::resize_png_if_needed(&png, max_dim)?;
                            crate::capture::write_crosshair_png(&png, cx, cy, &dbg_path_c)
                        })
                        .await;
                        match dbg_result {
                            Err(e) => {
                                return super::pre_dispatch_refusal(
                                    "px_capture_unavailable",
                                    format!(
                                        "debug_image_out task failed: {e}. Not dispatching click."
                                    ),
                                );
                            }
                            Ok(Err(e)) => {
                                return super::pre_dispatch_refusal(
                                    "px_capture_unavailable",
                                    format!(
                                        "debug_image_out write failed: {e}. Not dispatching click."
                                    ),
                                );
                            }
                            Ok(Ok(())) => {}
                        }
                    }
                }
            }

            if let Some(ref capture_id) = capture_id {
                let wid = match window_id {
                    Some(window_id) => window_id,
                    None => {
                        return ToolResult::error(
                            "window capture_id requires the matching pid and window_id.",
                        )
                        .with_structured(serde_json::json!({ "code": "invalid_arguments" }));
                    }
                };
                match self
                    .state
                    .capture_bindings
                    .admit_window_click(capture_id, &args, pid, wid, cx, cy)
                {
                    Ok((action_x, action_y)) => {
                        cx = action_x;
                        cy = action_y;
                    }
                    Err(refusal) => return refusal,
                }
            } else if matches!(space, super::window_points::PointSpace::WindowPoints(_)) {
                match super::window_points::to_window_pixels(
                    &self.state,
                    &args,
                    pid,
                    window_id,
                    space,
                    &[(cx, cy)],
                )
                .await
                {
                    Ok(resolved) => {
                        (cx, cy) = resolved.pixels[0];
                        validated_frame = resolved.frame;
                    }
                    Err(refusal) => return refusal,
                }
            } else {
                match super::window_click_point(
                    &self.state,
                    &args,
                    pid,
                    window_id,
                    cx,
                    cy,
                    from_zoom,
                ) {
                    Ok((wx, wy)) => {
                        cx = wx;
                        cy = wy;
                    }
                    Err(refusal) => return refusal,
                }
            }

            // ── Window-local → screen coordinate translation ──────────────────
            // `click_at_xy` accepts screen-space coordinates (top-left origin).
            // Callers supply window-local screenshot pixels; `px_frame` adds the
            // window's screen-origin (and divides out the Retina backing scale)
            // to produce the final screen position, or refuses when the window
            // has no live frame — see px_frame's module docs for why there is
            // no screen-absolute fallback.
            //
            // win_local_x/y: window-local logical-pixel coords used when the
            // Chromium recipe falls back to public PID posting. Its SkyLight
            // route uses the translated screen coordinates instead.
            let (screen_x, screen_y, win_local_x, win_local_y) = if let Some(wid) = window_id {
                match super::window_points::frame_or_resolve(validated_frame.clone(), wid).await {
                    Ok(frame) => {
                        let (sx, sy, lx, ly) = frame.to_screen(cx, cy);
                        // A window-local point outside the live frame would
                        // dispatch onto whatever occupies that screen point —
                        // the same wrong-surface misclick class as #2237.
                        // Refuse in background, where the caller cannot see
                        // what is actually under the translated point.
                        if !delivery_mode.is_foreground()
                            && (lx < 0.0
                                || ly < 0.0
                                || lx > frame.bounds.width
                                || ly > frame.bounds.height)
                        {
                            return super::invalid_arguments(format!(
                                "click: window-local point ({lx:.1}, {ly:.1}) pt lies outside \
                                 window {wid}'s {:.0}×{:.0} pt frame; background delivery \
                                 refused. Re-read coordinates from a fresh get_window_state \
                                 screenshot.",
                                frame.bounds.width, frame.bounds.height
                            ));
                        }
                        (sx, sy, lx, ly)
                    }
                    Err(refusal) => return refusal,
                }
            } else {
                // No window_id → treat x,y as screen coordinates (legacy behaviour).
                (cx, cy, cx, cy)
            };

            // ── Exact-target background gate (macOS background input v1) ──
            // A window-addressed background pixel action targets coordinates,
            // which only mean something while the exact window is current and
            // not minimized/hidden: a stale target would let the pid-scoped
            // hit-test or routed events land on a same-process sibling. Gate
            // BEFORE the AX hit-test backend and any cursor/dispatch work.
            // delivery_mode:"foreground" stays the explicit last resort.
            let mutation_lease_held = crate::background_mutation::held_by_current_task(pid);
            let _mutation_lease = if !delivery_mode.is_foreground() && !mutation_lease_held {
                if let Some(wid) = window_id {
                    match super::gate_background_window_action(
                        pid,
                        wid,
                        None,
                        cua_driver_core::background_input::BackgroundAction::WindowPointer,
                    )
                    .await
                    {
                        Ok(lease) => Some(lease),
                        Err(refusal_result) => return refusal_result,
                    }
                } else {
                    None
                }
            } else {
                None
            };

            // Pin the overlay above the target window BEFORE animating so
            // the cursor is already sandwiched correctly while it glides in.
            // Both PX deliveries below (AX hit-test and routed events) share
            // this glide, so the cursor shows whichever one lands the click.
            if let Some(wid) = window_id {
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
            }
            // Animate the visual cursor to the click point and wait for it to
            // arrive — mirrors Swift's `AgentCursor.shared.animateAndWait(to:)`.
            // A background pixel click on a Chromium <select> would open its
            // menu and raise the browser window (CUA-1795).
            if let Some(select_wid) = window_id.filter(|_| !delivery_mode.is_foreground()) {
                let refusal = latency::spawn_action_blocking(move || unsafe {
                    let element = element_at_screen_position(pid, screen_x, screen_y)?;
                    let refusal = (crate::ax::exact_target::element_window_id(element)
                        == Some(select_wid)
                        && super::set_value::is_chromium_web_select(pid, select_wid, element))
                    .then(|| {
                        super::set_value::chromium_select_open_refusal(
                            pid, select_wid, None, element,
                        )
                    });
                    CFRelease(element as _);
                    refusal
                })
                .await
                .ok()
                .flatten();
                if let Some(refusal) = refusal {
                    return refusal;
                }
            }
            crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), screen_x, screen_y).await;
            // Keep the registry in sync with the overlay (see AX path above).
            self.state
                .cursor_registry
                .update_position(&cursor_key, screen_x, screen_y);
            self.state
                .cursor_registry
                .note_press(&cursor_key, screen_x, screen_y);

            // A background PX action can still use an accessibility delivery
            // backend after resolving the requested screen point. This keeps
            // targeting (PX) orthogonal to delivery (AX) and avoids making a
            // Chromium/AppKit window key merely to satisfy first-mouse rules.
            if let Some(hit_test_wid) = window_id.filter(|_| {
                background_pixel_ax_eligible(
                    delivery_mode.is_foreground(),
                    &button_str,
                    count,
                    modifiers.is_empty(),
                    &action,
                )
            }) {
                let focus_only = action == "focus";
                if let Err(refusal) =
                    super::window_points::recheck_now(space, window_id, validated_frame.as_ref())
                {
                    return refusal;
                }
                let dispatch_guard = match super::window_points::DispatchGuard::new(
                    space,
                    window_id,
                    validated_frame.clone(),
                ) {
                    Ok(guard) => guard,
                    Err(refusal) => return refusal,
                };
                let ax_result = latency::spawn_action_blocking(move || unsafe {
                    let Some(element) = element_at_screen_position(pid, screen_x, screen_y) else {
                        return Ok::<PxAxPress, anyhow::Error>(PxAxPress::FallThrough);
                    };
                    // The pid-scoped hit-test can resolve an element from a
                    // same-process sibling overlapping the requested point.
                    // Require proven ancestry in the requested window before
                    // acting; otherwise fall through to the routed pixel path
                    // (already gated for this exact window).
                    if crate::ax::exact_target::element_window_id(element) != Some(hit_test_wid) {
                        CFRelease(element as _);
                        return Ok(PxAxPress::FallThrough);
                    }
                    if !background_hit_uses_ax(focus_only) {
                        CFRelease(element as _);
                        return Ok(PxAxPress::FallThrough);
                    }
                    // The element the point resolved to, read before the press.
                    let hit = pointer_hit(element);
                    // An element that does not advertise AXPress can still
                    // accept it as a no-op. On a toolkit that drops routed
                    // mouse events (LibreOffice's Calc cells) that no-op used
                    // to come back as a pressed click; fall through so the
                    // pixel route refuses instead.
                    if !focus_only
                        && !crate::ax::bindings::copy_action_names(element)
                            .iter()
                            .any(|action| action == "AXPress")
                        && crate::input::pointer_toolkit::detect(pid)
                            == Some(crate::input::pointer_toolkit::PointerReadingToolkit::Vcl)
                    {
                        CFRelease(element as _);
                        return Ok(PxAxPress::FallThrough);
                    }
                    // Last, after the hit test and its checks: the window must
                    // still be where the window points were translated.
                    if let Err(moved) = super::window_points::check_guard(&dispatch_guard) {
                        CFRelease(element as _);
                        return Err(moved.into());
                    }
                    let delivered = if focus_only {
                        if crate::input::ax_actions::focus_element(element as usize).is_ok() {
                            PxAxPress::Delivered(None, hit)
                        } else {
                            PxAxPress::FallThrough
                        }
                    } else {
                        match ax_press_with_ancestors(element, &dispatch_guard, pid, reads) {
                            Ok(AxPressOutcome::Delivered(evidence)) => {
                                PxAxPress::Delivered(evidence, hit)
                            }
                            Ok(AxPressOutcome::Unavailable) => PxAxPress::FallThrough,
                            Ok(AxPressOutcome::Indeterminate) => {
                                CFRelease(element as _);
                                return Ok(PxAxPress::Indeterminate(hit));
                            }
                            Err(moved) => {
                                CFRelease(element as _);
                                return Err(moved.into());
                            }
                        }
                    };
                    CFRelease(element as _);
                    Ok(delivered)
                })
                .await;
                if let Ok(Err(error)) = &ax_result {
                    if let Some(refusal) = super::window_points::moved_refusal(error, space) {
                        return refusal;
                    }
                }
                match ax_result {
                    Ok(Ok(PxAxPress::Delivered(evidence, hit))) => {
                        crate::cursor::overlay::send_command(
                            cursor_key.clone(),
                            cursor_overlay::OverlayCommand::ClickPulse {
                                x: screen_x,
                                y: screen_y,
                            },
                        );
                        let label = if focus_only { "focused" } else { "pressed" };
                        return px_ax_press_result(label, evidence, hit);
                    }
                    Ok(Ok(PxAxPress::FallThrough)) if focus_only => {
                        return ToolResult::error(
                            "Background PX focus is unavailable at the requested point.".to_owned(),
                        )
                        .with_structured(serde_json::json!({
                            "code": "background_unavailable"
                        }));
                    }
                    Ok(Err(error)) if !focus_only => {
                        // The AX task may have failed after an action: do
                        // not fall through to a second (pixel) click.
                        return ToolResult::text(format!(
                            "The accessibility action for the background click failed ({error});                              it may have run. Read fresh state before clicking again."
                        ))
                        .with_structured(serde_json::json!({
                            "path": "ax",
                            "effect": "unverifiable",
                            "code": "ax_action_unverified",
                        }))
                        .with_action_record(
                            cua_driver_core::action_record::ActionExecutionRecord::builder(
                                cua_driver_core::action_record::ActionEffect::Unverifiable,
                                cua_driver_core::action_record::ActionTransport::MacosAxAction,
                                cua_driver_core::action_record::RequestedDelivery::Background,
                            )
                            .actual_delivery(cua_driver_core::action_record::ActualDelivery::Background)
                            .build()
                            .expect("ax unverifiable record is valid"),
                        );
                    }
                    Ok(Err(error)) if focus_only => {
                        return ToolResult::error(format!("Background PX focus failed: {error}"))
                            .with_structured(serde_json::json!({
                                "code": "background_unavailable"
                            }));
                    }
                    Ok(Ok(PxAxPress::Indeterminate(hit))) => {
                        // (path and record below)

                        // An AX action may have been delivered: do NOT fall
                        // through to a second (pixel) click.
                        return ToolResult::text(
                            "The accessibility action for the background click may have run,                              but its selection could not be confirmed. Read fresh state before                              clicking again."
                                .to_owned(),
                        )
                        .with_structured({
                            let mut structured = serde_json::json!({
                                "path": "ax",
                                "effect": "unverifiable",
                                "code": "ax_action_unverified",
                            });
                            if let Some(hit) = hit {
                                structured["hit"] = hit;
                            }
                            structured
                        })
                        .with_action_record(
                            cua_driver_core::action_record::ActionExecutionRecord::builder(
                                cua_driver_core::action_record::ActionEffect::Unverifiable,
                                cua_driver_core::action_record::ActionTransport::MacosAxAction,
                                cua_driver_core::action_record::RequestedDelivery::Background,
                            )
                            .actual_delivery(cua_driver_core::action_record::ActualDelivery::Background)
                            .build()
                            .expect("ax unverifiable record is valid"),
                        );
                    }
                    Err(error) if !focus_only => {
                        // The task itself failed: it may have run an AX
                        // action first, so no pixel retry either.
                        return ToolResult::text(format!(
                            "The accessibility click task failed ({error}); it may have run.                              Read fresh state before clicking again."
                        ))
                        .with_structured(serde_json::json!({
                            "path": "ax",
                            "effect": "unverifiable",
                            "code": "ax_action_unverified",
                        }))
                        .with_action_record(
                            cua_driver_core::action_record::ActionExecutionRecord::builder(
                                cua_driver_core::action_record::ActionEffect::Unverifiable,
                                cua_driver_core::action_record::ActionTransport::MacosAxAction,
                                cua_driver_core::action_record::RequestedDelivery::Background,
                            )
                            .actual_delivery(cua_driver_core::action_record::ActualDelivery::Unknown)
                            .build()
                            .expect("ax unverifiable record is valid"),
                        );
                    }
                    Err(error) if focus_only => {
                        // The focus task may have failed after an AX focus:
                        // no pixel fallback, and no success for the caller's
                        // focus ladder.
                        return ToolResult::error(format!(
                            "The accessibility focus task failed ({error}); it may have run.                              Read fresh state before focusing again."
                        ))
                        .with_structured(serde_json::json!({
                            "path": "ax",
                            "effect": "unverifiable",
                            "code": "ax_action_unverified",
                        }))
                        .with_action_record(
                            cua_driver_core::action_record::ActionExecutionRecord::builder(
                                cua_driver_core::action_record::ActionEffect::Unverifiable,
                                cua_driver_core::action_record::ActionTransport::MacosAxAction,
                                cua_driver_core::action_record::RequestedDelivery::Background,
                            )
                            .actual_delivery(cua_driver_core::action_record::ActualDelivery::Unknown)
                            .build()
                            .expect("ax unverifiable record is valid"),
                        );
                    }
                    _ => {}
                }
            }

            // Resolve the effective delivery posture before observation. A
            // requested foreground click without a window id still degrades to
            // background, matching the existing contract and result label.
            let fg = delivery_mode.is_foreground() && window_id.is_some();
            // Background delivery cannot satisfy a toolkit that reads the
            // hardware pointer; refuse before any activation or dispatch.
            if let Err(refusal) =
                super::window_points::recheck_now(space, window_id, validated_frame.as_ref())
            {
                return refusal;
            }
            let route = match super::pixel_route::resolve(pid, fg, window_id, "mouse_click").await {
                Ok(route) => route,
                Err(refusal) => return refusal,
            };
            let activation_policy = pixel_activation_policy(&button_str, fg, window_id.is_some());
            let target_stamp = crate::ax::enablement::process_start_stamp(pid);
            // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
            // A pixel click can land on a "Sign In" button that opens a sheet
            // or a Safari link that activates a new tab — same side-effect
            // shape as the AX path, so we wrap identically.
            let prelude_cursor = cursor_key.clone();
            let (prior_front, restore_baseline, prepared) = capture_before_pixel_prelude(
                apps::frontmost_pid,
                super::pixel_restore::RestoreBaseline::capture,
                |prior_front| async move {
                    let snapshot = match activation_policy {
                        PixelActivationPolicy::SuppressTarget => {
                            WindowChangeDetector::snapshot(prior_front)
                        }
                        PixelActivationPolicy::AllowTargetWithoutRaise => {
                            WindowChangeDetector::snapshot_allowing_activation(prior_front, pid)
                        }
                        PixelActivationPolicy::ForegroundAssist => {
                            WindowChangeDetector::snapshot_without_suppression(prior_front)
                        }
                    };

                    // A raw click can open a menu (a popup drawn without AX, a canvas
                    // context menu); keep the user's key focus if it does.
                    let _key_focus = if fg {
                        None
                    } else {
                        latency::spawn_blocking(move || crate::key_focus_guard::protect(pid))
                            .await
                            .ok()
                            .flatten()
                    };

                    // The user's key window, read before the click, so the restore
                    // below hands key status back to exactly that window.
                    let prior_key_wid = match prior_front {
                        Some(prior)
                            if activation_policy
                                == PixelActivationPolicy::AllowTargetWithoutRaise
                                && prior != pid =>
                        {
                            latency::spawn_action_blocking(move || {
                                crate::input::skylight::key_window_of_pid(prior)
                            })
                            .await
                            .ok()
                            .flatten()
                        }
                        _ => None,
                    };

                    // Restore the Swift background-click prologue that was left
                    // disconnected in the original Rust port. It makes an opaque
                    // target AppKit-active without raising/restacking its window.
                    // Re-pin after the focus record because changing AppKit
                    // active state can disturb overlay ordering.
                    let focus_without_raise = if activation_policy
                        == PixelActivationPolicy::AllowTargetWithoutRaise
                    {
                        let wid = window_id.expect("activation policy requires window_id");
                        match latency::spawn_action_blocking(move || {
                            // A key hand-back still running would move key focus under
                            // this action's focus record: send nothing instead.
                            crate::input::skylight::await_key_restores().then(|| {
                                crate::input::mouse::prepare_background_pixel_click_fast(pid, wid)
                            })
                        })
                        .await
                        {
                            Ok(None) => {
                                return Err(super::key_restore_pending_refusal(
                                    "click",
                                    Some(pid),
                                    Some(wid),
                                ));
                            }
                            Ok(Some(activated)) => {
                                if !activated
                                    && crate::input::skylight::windowserver_connection_invalid()
                                {
                                    return Err(super::windowserver_connection_refusal(
                                        pid,
                                        Some(wid),
                                    ));
                                }
                                crate::cursor::overlay::send_command(
                                    prelude_cursor.clone(),
                                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                                );
                                activated
                            }
                            Err(error) => {
                                return Err(ToolResult::error(format!(
                                    "Background click activation task failed: {error}"
                                )));
                            }
                        }
                    } else {
                        false
                    };

                    Ok::<_, ToolResult>((snapshot, _key_focus, prior_key_wid, focus_without_raise))
                },
            )
            .await;
            let (snapshot, _key_focus, prior_key_wid, focus_without_raise) = match prepared {
                Ok(prepared) => prepared,
                Err(refusal) => return refusal,
            };
            let expected_front = prior_front;

            // Pulse only after the activation settle so it visually coincides
            // with the real target click rather than the private focus prelude.
            crate::cursor::overlay::send_command(
                cursor_key.clone(),
                cursor_overlay::OverlayCommand::ClickPulse {
                    x: screen_x,
                    y: screen_y,
                },
            );

            let mods_owned = modifiers.clone();
            // Surface 5: route to the right/middle CGEvent primitives when
            // button != left. Left-button path stays on the existing Chromium-
            // routed `click_at_xy_with_window_local` for back-compat.
            let button_kind = button_str.clone();
            let dispatch_guard = match super::window_points::DispatchGuard::new(
                space,
                window_id,
                validated_frame.clone(),
            ) {
                Ok(guard) => guard,
                Err(refusal) => return refusal,
            };
            let result = focus_guard::with_focus_suppressed(
                if activation_policy == PixelActivationPolicy::SuppressTarget {
                    Some(pid)
                } else {
                    None
                },
                prior_front,
                "click.pixel",
                || async move {
                    latency::spawn_action_blocking(move || {
                        // What a background left click may change, read right
                        // before it goes out. A HID click (foreground) resets
                        // the physical-input clock itself, so it never gets
                        // evidence and is not probed.
                        let mut probe = window_id
                            .filter(|_| {
                                // A multi-click's selection read-back would
                                // prove only its first click.
                                route != PixelClickRoute::ForegroundHid
                                    && button_kind == "left"
                                    && count == 1
                            })
                            .map(|wid| unsafe {
                                ClickProbe::at_point(pid, wid, screen_x, screen_y, reads)
                            });
                        // The element under the point, read before the post.
                        let hit = match probe.as_ref() {
                            Some(probe) => probe.hit(),
                            None => window_id.and_then(|wid| unsafe {
                                element_in_window_at(pid, wid, screen_x, screen_y)
                                    .and_then(|(element, _)| pointer_hit(element.0))
                            }),
                        };
                        if let Some(probe) = probe.as_mut() {
                            probe.own_post_starts();
                        }
                        let do_click = move || -> anyhow::Result<()> {
                            super::window_points::check_guard(&dispatch_guard)?;
                            let m: Vec<&str> = mods_owned.iter().map(String::as_str).collect();
                            if route == PixelClickRoute::ForegroundHid {
                                // Warp the hardware pointer to the mapped global
                                // point and post at the HID tap. The pointer stays
                                // at the target, as on Windows and X11, so apps that
                                // read the pointer when handling the event see it.
                                return crate::input::mouse::click_at_xy_desktop_with_modifiers(
                                    screen_x,
                                    screen_y,
                                    count,
                                    &button_kind,
                                    &m,
                                );
                            }
                            match button_kind.as_str() {
                                "right" => {
                                    if let Some(wid) = window_id {
                                        return crate::input::mouse::right_click_at_xy_with_window_local(
                                            pid, screen_x, screen_y, win_local_x, win_local_y, wid, &m,
                                        );
                                    }
                                    crate::input::mouse::right_click_at_xy(pid, screen_x, screen_y, &m)
                                }
                                "middle" => {
                                    if let Some(wid) = window_id {
                                        return crate::input::mouse::middle_click_at_xy_with_window_local(
                                            pid, screen_x, screen_y, win_local_x, win_local_y, wid, &m,
                                        );
                                    }
                                    crate::input::mouse::middle_click_at_xy(pid, screen_x, screen_y, &m)
                                }
                                // "left" (default) or anything else — preserve legacy left-click path.
                                _ => {
                                    // When we know the window_id, use the targeted primitive so
                                    // foreground delivery can stamp window-local coordinates and
                                    // background delivery can retain the translated screen point
                                    // while adding Chromium routing fields (f40, f51, f58, f91, f92).
                                    if let Some(wid) = window_id {
                                        if !fg {
                                            return crate::input::mouse::click_background_pixel_fast(
                                                pid, (screen_x, screen_y), (win_local_x, win_local_y),
                                                wid, count, &m,
                                                &mut || super::window_points::check_guard(&dispatch_guard).map_err(Into::into),
                                            );
                                        }
                                        return crate::input::mouse::click_at_xy_with_window_local(
                                            pid, screen_x, screen_y,
                                            win_local_x, win_local_y,
                                            wid, count, &m,
                                            crate::input::mouse::WindowClickDelivery::from_foreground(fg),
                                        );
                                    }
                                    crate::input::mouse::click_at_xy(pid, screen_x, screen_y, count, &m)
                                }
                            }
                        };
                        // Foreground rung: front the exact window → HID click →
                        // restore the prior front process. The HID tap has no
                        // pid addressing, so activation must be proven (the
                        // window is AX-focused) or no input is sent.
                        match (route, window_id) {
                            (PixelClickRoute::ForegroundHid, Some(wid)) => {
                                crate::input::skylight::with_foreground_hid_activation(
                                    pid as libc::pid_t,
                                    wid,
                                    do_click,
                                )
                            }
                            _ => do_click(),
                        }
                        .map(|()| {
                            if let Some(probe) = probe.as_mut() {
                                probe.own_post_finished();
                            }
                            (probe, hit)
                        })
                    })
                    .await
                },
            )
            .await;
            // Watch the click's effect from now, while the focus restore and
            // the window watch below run: the observation has its own
            // deadline from delivery.
            let mut hit = None;
            let (result, observing) = match result {
                Ok(Ok((probe, posted_hit))) => {
                    hit = posted_hit;
                    (
                        Ok(Ok(())),
                        probe.map(|probe| latency::spawn_action_blocking(move || probe.observe())),
                    )
                }
                Ok(Err(error)) => (Ok(Err(error)), None),
                Err(error) => (Err(error), None),
            };

            if activation_policy == PixelActivationPolicy::AllowTargetWithoutRaise
                && prior_front == Some(pid)
            {
                let epoch = super::pixel_restore::current_epoch().expect("input admission");
                super::pixel_restore::global().update_existing_target(
                    pid,
                    window_id,
                    target_stamp,
                    epoch,
                );
            }
            if activation_policy == PixelActivationPolicy::AllowTargetWithoutRaise
                && prior_front != Some(pid)
            {
                let epoch = super::pixel_restore::current_epoch().expect("input admission");
                super::pixel_restore::spawn_restore(
                    super::pixel_restore::global(),
                    super::pixel_restore::SpawnRestore {
                        pid,
                        epoch,
                        expected_front,
                        baseline: restore_baseline,
                        window_id,
                        target_stamp,
                    },
                    Arc::new(super::pixel_restore::SystemRestoreProbe),
                    move |target_pid, target_window| async move {
                        super::pixel_focus::restore_user_focus_after_background_pixel_now(
                            target_pid,
                            prior_front,
                            prior_key_wid,
                            target_window,
                            focus_without_raise,
                            Some((restore_baseline, expected_front)),
                        )
                        .await
                    },
                );
            }

            let changes = snapshot.detect_sampled_async().await;
            if let Ok(Err(error)) = &result {
                if let Some(refusal) = super::window_points::moved_refusal(error, space) {
                    return refusal;
                }
            }

            let button_label = match (button_str.as_str(), count) {
                ("right", _) => "right-click",
                ("middle", _) => "middle-click",
                (_, 2) => "double-click",
                (_, 3) => "triple-click",
                _ => "click",
            };
            match result {
                Ok(Ok(())) => {
                    let evidence = match observing {
                        Some(observing) => observing.await.ok().flatten(),
                        None => None,
                    };
                    let target = if route == PixelClickRoute::ForegroundHid {
                        format!("at screen-point ({screen_x:.0},{screen_y:.0}) for pid {pid}")
                    } else {
                        format!("to pid {pid}")
                    };
                    let read_back = evidence
                        .map(|evidence| format!(" Read back: {}.", evidence.detail()))
                        .unwrap_or_default();
                    let mut structured = serde_json::json!({
                        "path": super::pixel_route::path_label(route),
                        "verified": evidence.is_some(),
                        "effect": if evidence.is_some() { "confirmed" } else { "unverifiable" },
                        "focus_without_raise": focus_without_raise
                    });
                    if let Some(hit) = hit {
                        structured["hit"] = hit;
                    }
                    let result = ToolResult::text(format!(
                        "✅ Posted {button_label} {target} ({}).{read_back}{}",
                        super::pixel_route::delivery_note(route),
                        changes.result_suffix()
                    ));
                    match evidence {
                        Some(evidence) => {
                            use cua_driver_core::action_record::{
                                ActionTransport, ActualDelivery, RequestedDelivery,
                            };
                            structured["evidence"] = serde_json::json!([evidence.structured()]);
                            result
                                .with_structured(structured)
                                .with_action_record(evidence.record(
                                    ActionTransport::MacosCgEventPid,
                                    if delivery_mode.is_foreground() {
                                        RequestedDelivery::Foreground
                                    } else {
                                        RequestedDelivery::Background
                                    },
                                    ActualDelivery::Background,
                                ))
                        }
                        None => result.with_structured(structured),
                    }
                }
                Ok(Err(e)) if route == PixelClickRoute::ForegroundHid => {
                    super::pixel_route::foreground_unavailable(
                        button_label,
                        window_id.unwrap_or_default(),
                        &e.to_string(),
                    )
                }
                Ok(Err(e)) => ToolResult::error(format!("{button_label} failed: {e}")),
                Err(e) => click_task_failure(format!(
                    "The {button_label} task failed ({e}); the click may have been posted. Read \
                     fresh state before clicking again."
                )),
            }
        } else {
            super::invalid_arguments("Provide either element_token or (x + y). pid is required for x,y clicks; an element_token carries its own pid.")
        }
    }
}

// ── AX click implementation (blocking) ───────────────────────────────────────

/// Structured result of an element click from what `perform_ax_click`
/// reported.
///
/// AX dispatch went through, but AXPerformAction returning success does not
/// confirm the on-screen effect (many elements no-op silently). A click is
/// never driver-verifiable (no read-back) → verified:false stays for
/// back-compat. The tri-state `effect` is the richer signal:
///   * suspected_noop — the element didn't advertise the action, so the press
///     likely did nothing → cross to vision/pixel.
///   * unverifiable — dispatched, the driver just can't confirm; the caller
///     verifies via screenshot.
fn ax_click_structured(
    selection_via_pixel: bool,
    fronted: bool,
    evidence: Option<ClickEvidence>,
    suspected_noop: bool,
) -> serde_json::Value {
    let verified = evidence.is_some();
    let mut structured = serde_json::json!({
        "path": if selection_via_pixel {
            if fronted { "cgevent_fg" } else { "cgevent" }
        } else if fronted {
            "ax_fg"
        } else {
            "ax"
        },
        "verified": verified,
        "effect": if verified {
            "confirmed"
        } else if suspected_noop {
            "suspected_noop"
        } else {
            "unverifiable"
        },
    });
    if let Some(evidence) = evidence {
        structured["evidence"] = serde_json::json!([evidence.structured()]);
    }
    if suspected_noop {
        structured["escalation"] = serde_json::json!({
            "recommended": "px",
            "reason": "element does not advertise this action — the \
                       AX press likely no-op'd. Do an element px \
                       action: click by pixel (x,y) off the \
                       screenshot from get_window_state."
        });
    }
    structured
}

/// `perform_ax_click`'s outcome: `(summary_text, suspected_noop,
/// selection_verified, selection_via_pixel)`.
type AxClickOutcome = (String, bool, Option<ClickEvidence>, bool);

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The outcome of an AXSelected write by the selection fallback. `tied`
/// when a retained item read unselected before and selected after, with no
/// physical input in between; otherwise the write is real but cannot be
/// told apart from a row that was already selected.
fn selection_write_outcome(
    tied: bool,
    selected_role: &str,
    element: (usize, &str, &str),
    after_error: Option<crate::ax::bindings::AXError>,
) -> AxClickOutcome {
    let (idx, role, title) = element;
    let after = after_error
        .map(|err| format!(" after AXPress returned {err}"))
        .unwrap_or_default();
    if tied {
        return (
            format!(
                "✅ Selected nearest {selected_role} for [{idx}] {role} \"{title}\"{after}; \
                 confirmed AXSelected=true."
            ),
            false,
            Some(ClickEvidence::Selected),
            false,
        );
    }
    (
        format!(
            "⚠️ Selected nearest {selected_role} for [{idx}] {role} \"{title}\"{after}; \
             AXSelected=true reads back, but the change could not be tied to this click (it \
             may have been selected already, or other input happened). Read the window before \
             repeating it."
        ),
        false,
        None,
        false,
    )
}

/// Whether a non-success `AXUIElementPerformAction` return proves the app
/// never received the action. Only the accessibility client's own refusals
/// do: a bad argument, a dead element reference, or accessibility turned off.
/// Every other code comes back after the request reached the app, which may
/// have acted on it; Apple documents that `kAXErrorCannotComplete` (the app
/// did not answer within the messaging timeout) does not mean the action
/// failed, and Finder opens a row and returns `kAXErrorAttributeUnsupported`.
pub(super) fn perform_error_proves_nothing_was_sent(err: crate::ax::bindings::AXError) -> bool {
    use crate::ax::bindings::{
        kAXErrorAPIDisabled, kAXErrorIllegalArgument, kAXErrorInvalidUIElement,
    };
    [
        kAXErrorIllegalArgument,
        kAXErrorInvalidUIElement,
        kAXErrorAPIDisabled,
    ]
    .contains(&err)
}

/// The outcome of a coordinate selection click that was delivered but whose
/// AXSelected read-back did not settle. The click reached the app and may
/// have selected or acted, so this is an unconfirmed click, not a failure.
fn unconfirmed_selection_click(
    selected_role: &str,
    element: (usize, &str, &str),
    modified: bool,
    readback: &str,
) -> AxClickOutcome {
    let (idx, role, title) = element;
    let expected = if modified {
        "a stable AXSelected change that kept the prior selection"
    } else {
        "a stable AXSelected change"
    };
    (
        format!(
            "⚠️ Sent a coordinate click to the nearest {selected_role} for [{idx}] {role} \
             \"{title}\", but did not read back {expected} ({readback}). It may have selected \
             or acted: read the window before repeating it."
        ),
        false,
        None,
        true,
    )
}

/// The result of an element click whose AX task returned `error`. A typed
/// refusal is raised before the click sends its action or any selection
/// write (an unsupported or ambiguous action, a background-only action, a
/// background target that reads AXEnabled=false) and reaches the caller as a
/// coded refusal with `dispatched: false`. Any other error stays a plain error
/// that claims nothing about delivery: some follow an AX write or a posted
/// pointer event (the selection fallback, a text input's focus), so the
/// caller reads the window before acting again. An AX action that was sent
/// and came back with an error does not arrive here: it is an unverifiable
/// click (`unresolved_perform_error`) unless a read-back confirms it.
fn element_click_failure(error: &anyhow::Error) -> ToolResult {
    match crate::input::ax_actions::element_action_refusal_code(error) {
        Some(code) => super::pre_dispatch_refusal(code, format!("AX action refused: {error}")),
        None => ToolResult::error(format!("AX action failed: {error}")),
    }
}

/// The outcome of an AX action whose perform call returned `err` after it
/// was sent, when no read-back resolved what happened. An error that proves
/// the action never reached the app stays a plain failure, never a refusal:
/// the click's selection fallback may have written AXSelected before or
/// after the action, so the caller reads the window first. Any other error is
/// an unconfirmed action (`effect: "unverifiable"`, not an error result), so
/// the caller reads the window before repeating it instead of sending a
/// second click (cua-driver#21).
fn unresolved_perform_error(
    ax_action: &str,
    err: crate::ax::bindings::AXError,
    element: (usize, &str, &str),
) -> anyhow::Result<AxClickOutcome> {
    if perform_error_proves_nothing_was_sent(err) {
        anyhow::bail!("AXUIElementPerformAction({ax_action}) returned {err}");
    }
    let (idx, role, title) = element;
    Ok((
        format!(
            "⚠️ Sent {ax_action} to [{idx}] {role} \"{title}\", but the app returned AX error \
             {err}, which does not mean the action failed: it may have acted. Read the window \
             before repeating it."
        ),
        false,
        None,
        false,
    ))
}

/// Returns `(summary_text, suspected_noop, selection_verified,
/// selection_via_pixel)`.
///
/// `suspected_noop` is true when the element did not advertise the action we
/// dispatched — AXUIElementPerformAction returns success regardless, so this is
/// the driver's only signal that the press likely did nothing. The caller turns
/// it into `effect: "suspected_noop"` + an escalation hint so the agent crosses
/// to the vision/pixel path instead of trusting a hollow success.
///
/// `element` is the cached AX element pointer and its snapshot index; `window`
/// is the target (pid, window_id).
/// Whether a background AXPress needs its window to be the app's key window.
///
/// A toolbar or format-bar control in a native app sends its action to the
/// first responder through the key window (TextEdit's Bold checkbox applies
/// the trait to the text view that way). With the app behind the user's
/// window nothing is key, so the press flips the control but the document
/// does not change. Browsers and Electron shells run page content in their own
/// process and take no part in this.
fn press_needs_key_window(action: &str, front_pid: Option<i32>, pid: i32, native: bool) -> bool {
    action == "AXPress" && native && front_pid != Some(pid)
}

/// Make the target window AppKit-key without raising it, activating the app
/// or taking key status from the user's window (the focus record only, as the
/// background pixel click does), then let AppKit settle.
///
/// A press right after another one on the same window reuses that priming
/// (see [`KeyPrime`]): only an app activation (the person switching apps,
/// in either direction), a focus record or focused-window write the driver
/// sent anywhere since, or a change of the target app's focused window can
/// take key status away from it. All three are read again before each press;
/// the focused window must be the app's own `AXFocusedWindow`, never a
/// window-order guess.
fn prime_key_window_for_background_press(pid: i32, window_id: u32, action: &str) {
    // `native` (a browser or Electron lookup) is read last: a reused
    // priming was only ever recorded for a native app.
    if !press_needs_key_window(action, apps::frontmost_pid(), pid, true) {
        return;
    }
    let now = KeyPrime {
        pid,
        window_id,
        activations: crate::focus_steal::activation_count(),
        focus_generation: crate::input::skylight::focus_generation(),
        at: std::time::Instant::now(),
    };
    let mut last = LAST_KEY_PRIME.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|last| last.still_holds(&now))
        && crate::input::skylight::primed_key_still_held(pid, window_id)
        && crate::ax::bindings::focused_window_id_of_pid(pid) == Some(window_id)
    {
        return;
    }
    *last = None;
    let native = !super::type_text::is_browser_pid(pid)
        && !crate::browser::electron_js::ElectronJs::is_electron(pid);
    if !native {
        return;
    }
    if crate::input::skylight::activate_without_raise_with(pid, window_id, false) {
        crate::input::skylight::note_primed_key(pid, window_id);
        std::thread::sleep(KEY_PRIME_SETTLE);
        // Recorded after this priming's own focus record. An activation or
        // another focus change during the settle may already have undone it.
        let primed = KeyPrime {
            focus_generation: crate::input::skylight::focus_generation(),
            ..now
        };
        *last = (crate::focus_steal::activation_count() == now.activations).then_some(primed);
    }
}

/// How long AppKit gets to take the focus record before the press.
const KEY_PRIME_SETTLE: std::time::Duration = std::time::Duration::from_millis(80);

/// How long a key-window priming is reused for later presses on its window.
const KEY_PRIME_REUSE: std::time::Duration = std::time::Duration::from_secs(10);

static LAST_KEY_PRIME: std::sync::Mutex<Option<KeyPrime>> = std::sync::Mutex::new(None);

/// The last window made key for a background press, and the app-activation
/// count when it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyPrime {
    pid: i32,
    window_id: u32,
    activations: u64,
    /// `skylight::focus_generation` after the priming's own record.
    focus_generation: u64,
    at: std::time::Instant,
}

impl KeyPrime {
    /// Whether this priming still covers a press described by `now`: same
    /// window, no app activated since, and recent.
    fn still_holds(&self, now: &KeyPrime) -> bool {
        self.pid == now.pid
            && self.window_id == now.window_id
            && self.activations == now.activations
            && self.focus_generation == now.focus_generation
            && now.at.saturating_duration_since(self.at) < KEY_PRIME_REUSE
    }
}

/// Focus is the click's evidence only when the field read unfocused before
/// it (an unreadable baseline proves nothing) and no physical input arrived
/// meanwhile.
fn text_focus_evidence(focused_before: Option<bool>, input_quiet: bool) -> Option<ClickEvidence> {
    (focused_before == Some(false) && input_quiet).then_some(ClickEvidence::Focused)
}

/// Whether an AX press on a text input must wait for the page to move DOM
/// focus (800 ms). Only text inputs inside web content (an `AXWebArea`
/// ancestor) have a DOM focus to settle; waiting on a native field only
/// delays the result. `ancestor_roles` is read only for a text input pressed
/// with `AXPress`.
fn needs_web_focus_settle(
    ax_action: &str,
    role: &str,
    ancestor_roles: impl FnOnce() -> Vec<String>,
) -> bool {
    ax_action == "AXPress"
        && matches!(role, "AXTextField" | "AXTextArea")
        && ancestor_roles().iter().any(|role| role == "AXWebArea")
}

/// Roles of `element`'s ancestors, nearest first, up to the window. Bounded so
/// a cyclic or very deep tree cannot stall the click.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef`.
unsafe fn ancestor_roles(element: AXUIElementRef) -> Vec<String> {
    const MAX_DEPTH: usize = 40;
    let mut roles = Vec::new();
    let mut current = copy_element_attr(element, "AXParent");
    while let Some(parent) = current {
        if roles.len() >= MAX_DEPTH {
            CFRelease(parent as CFTypeRef);
            break;
        }
        let role = copy_string_attr(parent, "AXRole").unwrap_or_default();
        let next = copy_element_attr(parent, "AXParent");
        CFRelease(parent as CFTypeRef);
        let is_window = role == "AXWindow";
        roles.push(role);
        if is_window {
            if let Some(next) = next {
                CFRelease(next as CFTypeRef);
            }
            break;
        }
        current = next;
    }
    roles
}

#[allow(clippy::too_many_arguments)]
fn perform_ax_click(
    element: (usize, usize),
    window: (i32, u32),
    action_str: &str,
    cursor_key: &str,
    selection_pixel: Option<SelectionPixelTarget>,
    modifiers: &[String],
    foreground: bool,
    read_back: bool,
) -> anyhow::Result<AxClickOutcome> {
    let (element_ptr, idx) = element;
    let (pid, window_id) = window;
    let element = element_ptr as AXUIElementRef;
    let reads = ClickReads::for_click(read_back);

    // Read live capabilities before dispatch: custom actions and
    // AXIncrement/AXDecrement are valid only on elements that expose them.
    let preflight = latency::span(latency::Stage::Preflight);
    let advertised = unsafe { copy_action_names(element) };
    let ax_action =
        crate::input::ax_actions::resolve_element_action(action_str, &advertised, foreground)?;

    // Check the live value immediately before dispatch. Foreground assist can
    // enable menu items that were disabled in the cached snapshot, while a
    // background transition can disable them after that snapshot. macOS may
    // otherwise return success for a disabled action that did nothing.
    crate::input::ax_actions::ensure_ax_action_enabled(element_ptr, ax_action, foreground)?;

    // Capture advertised actions BEFORE dispatching so we can detect silent no-ops
    // (AX returns success even when the element doesn't advertise the action).
    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    // The name shown in the result: an untitled control (KakaoTalk's tabs)
    // is named by its description, help or identifier instead of "".
    let title = ["AXTitle", "AXDescription", "AXHelp", "AXIdentifier"]
        .iter()
        .find_map(|attr| {
            unsafe { copy_string_attr(element, attr) }.filter(|text| !text.trim().is_empty())
        })
        .unwrap_or_default();

    // A click on an AppKit collection item is frequently represented by a
    // label child or row that does not advertise AXPress. Prefer a bounded,
    // read-back-verified AXSelected write over dispatching a known hollow press
    // or forcing the caller onto a less stable pixel coordinate.
    if ax_action == "AXPress" && !advertised.iter().any(|action| action == ax_action) {
        if modifiers.is_empty() {
            let idle_before = super::physical_input_idle_ms();
            let started = std::time::Instant::now();
            let snapshot = reads
                .selection
                .then(|| unsafe { SelectionSnapshot::capture(element) });
            if let Some(selected_role) =
                crate::input::ax_actions::select_nearest_container(element_ptr)
            {
                let tied = snapshot
                    .as_ref()
                    .is_some_and(SelectionSnapshot::transitioned)
                    && input_quiet(
                        idle_before,
                        super::physical_input_idle_ms(),
                        elapsed_ms(started),
                    );
                return Ok(selection_write_outcome(
                    tied,
                    &selected_role,
                    (idx, &role, &title),
                    None,
                ));
            }
        }

        // The input clock starts before the selection baseline (the selected
        // peers included) is read.
        let selection_idle_before = super::physical_input_idle_ms();
        let selection_input_from = std::time::Instant::now();
        if let (Some(target), Some(selection)) = (
            selection_pixel,
            crate::input::ax_actions::capture_nearest_container_selection(element_ptr),
        ) {
            let selected_role = selection.role().to_owned();
            let idle_before = selection_idle_before;
            let input_from = selection_input_from;
            let Some((before, _)) = selection.observe() else {
                anyhow::bail!("selection target stopped exposing AXSelected before delivery");
            };
            let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
            if foreground && !modifier_refs.is_empty() {
                crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                    target.screen_x,
                    target.screen_y,
                    1,
                    "left",
                    &modifier_refs,
                )?;
            } else {
                crate::input::mouse::click_at_xy_with_window_local(
                    pid,
                    target.screen_x,
                    target.screen_y,
                    target.window_x,
                    target.window_y,
                    window_id,
                    1,
                    &modifier_refs,
                    crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
                )?;
            }
            if !reads.selection {
                return Ok(unconfirmed_selection_click(
                    &selected_role,
                    (idx, &role, &title),
                    !modifiers.is_empty(),
                    "no read-back was requested",
                ));
            }
            // Poll the selection from delivery and return on the sample that
            // shows the expected state, held on an immediate second read; a
            // modified selection additionally preserves every peer that was
            // selected before delivery.
            let modified = !modifiers.is_empty();
            let last_observation = std::cell::Cell::new(None);
            let found = watch_for_change(
                selection_fallback_scope(before, modified),
                std::time::Instant::now(),
                &crate::ax::scroll_area::SystemClock,
                || {
                    let (after, peers_preserved) = selection.observe()?;
                    last_observation.set(Some((after, peers_preserved)));
                    selection_readback_confirms(before, after, modified, peers_preserved).then_some(
                        if after {
                            ClickEvidence::Selected
                        } else {
                            ClickEvidence::Deselected
                        },
                    )
                },
            );
            // A person's click on the same row would read the same: no
            // physical input around the post.
            if let Some(evidence) = found.filter(|_| {
                input_quiet(
                    idle_before,
                    super::physical_input_idle_ms(),
                    elapsed_ms(input_from),
                )
            }) {
                return Ok((
                    format!(
                        "✅ Selected nearest {selected_role} for [{idx}] {role} \
                         \"{title}\"; AX selection write was unavailable, so a \
                         coordinate click was delivered and confirmed by AXSelected \
                         read-back."
                    ),
                    false,
                    Some(evidence),
                    true,
                ));
            }
            let last_observation = last_observation.get();
            return Ok(unconfirmed_selection_click(
                &selected_role,
                (idx, &role, &title),
                !modifiers.is_empty(),
                &format!("before_selected={before}, last_readback={last_observation:?}"),
            ));
        }
    }

    let toggle = is_toggle_press(ax_action, &role, &advertised);
    drop(preflight);
    let text_input_press = action_str == "press"
        && ax_action == "AXPress"
        && modifiers.is_empty()
        && text_input_click_role(&role)
        && !unsafe { super::set_value::file_name_cell(element) };
    let text_in_web_area = text_input_press
        && super::type_text::target_requires_pointer_focus(
            pid,
            Some((element_ptr, Some(idx))),
            Some(window_id),
        );
    // A native text input whose app's key window is another of its windows (a
    // second document) takes AXFocused only while its own window is key: hold
    // that window as the app's key window for the focus and its checks, then
    // hand key back. A window that is already key is held without posting
    // anything. The focus stays the window's first responder for the next
    // type_text, which takes its own key scope.
    let text_key_scope = text_input_press && !text_in_web_area && !foreground;
    if !foreground && !text_key_scope {
        latency::section(latency::Stage::KeyPrime, || {
            prime_key_window_for_background_press(pid, window_id, ax_action)
        });
    }
    if text_input_press {
        let in_web_area = text_in_web_area;
        // Focus counts as the click's effect only when the field was not
        // focused before and no physical input arrived meanwhile.
        // The input clock starts before the baseline read, so a person
        // focusing the field during the read also disqualifies it.
        let focus_idle_before = super::physical_input_idle_ms();
        let focus_input_from = std::time::Instant::now();
        // Only evidence reads it; a click that was not asked to read back
        // skips the read.
        let focused_before = reads
            .selection
            .then(|| unsafe { copy_bool_attr(element, "AXFocused") })
            .flatten();
        let scope = if text_key_scope {
            match crate::input::skylight::enter_exact_window_key_inline(
                pid as libc::pid_t,
                window_id,
            ) {
                Ok(scope) => Some(scope),
                Err(reason) => anyhow::bail!(
                    "text input [{idx}]: {reason}; AXFocused was not written and AXPress was not sent"
                ),
            }
        } else {
            None
        };
        // The focus step and its checks; the scope's hand-back is finished
        // and disclosed on every way out of it.
        let focus_step = || -> anyhow::Result<bool> {
            if in_web_area {
                super::type_text::click_element_center_for_focus_checked(
                    pid,
                    window_id,
                    element_ptr,
                    foreground,
                )?;
            } else {
                crate::input::ax_actions::focus_element(element_ptr)?;
            }
            // Focus can land shortly after delivery (a web field's DOM
            // focus): poll it from delivery and stop on the sample that
            // shows it.
            let holds = || {
                (unsafe { copy_bool_attr(element, "AXFocused") } == Some(true))
                    && super::type_text::element_in_requested_window(pid, window_id, element_ptr)
                    && crate::input::ax_actions::is_element_focused(pid, element_ptr)
                    && super::type_text::focused_window_matches_element(pid, window_id, element_ptr)
            };
            Ok(watch_for_change(
                ObservationScope::Watch,
                std::time::Instant::now(),
                &crate::ax::scroll_area::SystemClock,
                || holds().then_some(ClickEvidence::Focused),
            )
            .is_some())
        };
        let focused = focus_step();
        let hand_back = scope.map(|scope| {
            let entry = scope.entry();
            crate::input::skylight::scope_disclosure(entry, Some(scope.finish()))
        });
        let focused = match focused {
            Ok(focused) => focused,
            // The original error stays downcastable (refusal codes); the
            // disclosure is added as context.
            Err(error) => match hand_back.filter(|text| !text.is_empty()) {
                Some(text) => {
                    let message = format!("{error} {text}");
                    return Err(error.context(message));
                }
                None => return Err(error),
            },
        };
        if !focused {
            if in_web_area {
                return Ok((
                    format!(
                        "⚠️ Sent a window-local pointer focus to [{idx}] {role} \"{title}\", but focus could not be confirmed."
                    ),
                    false,
                    None,
                    true,
                ));
            }
            anyhow::bail!(
                "text input [{idx}] did not become the application's focused element; AXPress was not sent.{}",
                hand_back.map(|text| format!(" {text}")).unwrap_or_default()
            )
        }
        let evidence = text_focus_evidence(
            focused_before,
            input_quiet(
                focus_idle_before,
                super::physical_input_idle_ms(),
                elapsed_ms(focus_input_from),
            ),
        );
        let summary = if evidence.is_some() {
            format!("✅ Focused [{idx}] {role} \"{title}\" with AXFocused=true; focus read-back confirmed.")
        } else {
            format!(
                "✅ [{idx}] {role} \"{title}\" holds focus (AXFocused=true), but it was focused \
                 before the click or input arrived meanwhile, so nothing confirms the click."
            )
        };
        return Ok((
            format!(
                "{summary}{}",
                hand_back.map(|text| format!(" {text}")).unwrap_or_default()
            ),
            false,
            evidence,
            in_web_area,
        ));
    }
    // What the action may change (a selection, a toggle, the window title of
    // a navigation such as Finder's sidebar, a sheet or window it opens),
    // read right before dispatch so nothing earlier (a navigation still
    // finishing during the priming) counts. A row whose press is refused is
    // selected by the fallback below; its own snapshot ties that write.
    let mut probe = unsafe { ClickProbe::capture(Some(element), toggle, reads) };
    if reads.sheet {
        // Reading the ancestors up to a sheet and watching its window for up
        // to 600 ms exist only to turn a pressed sheet button into a
        // confirmed click: they run when the caller asks for the read-back.
        probe = unsafe { probe.watching_sheet_for_press(element, ax_action) };
    }
    let fallback_selection = (ax_action == "AXPress" && modifiers.is_empty()).then(|| {
        let idle_before = super::physical_input_idle_ms();
        let started = std::time::Instant::now();
        (
            reads
                .selection
                .then(|| unsafe { SelectionSnapshot::capture(element) }),
            idle_before,
            started,
        )
    });
    let err = latency::section(latency::Stage::AxPerform, || unsafe {
        crate::ax::bindings::perform_action(element, ax_action)
    });
    probe.press_returned();
    if err != crate::ax::bindings::kAXErrorSuccess {
        // Checked before the row-selection fallback, which writes AXSelected
        // on an ancestor: a press that landed (a toggle, a navigation) needs
        // no other write. Same evidence and input rules as a success.
        if press_error_may_have_acted(err) {
            // A press the app reported as failed never claims the sheet's
            // closing: only a delivered AXPress may.
            if let Some(evidence) = probe
                .observe()
                .filter(|evidence| *evidence != ClickEvidence::SheetClosed)
            {
                return Ok((
                    format!(
                        "✅ Performed {ax_action} on [{idx}] {role} \"{title}\"; {}, although \
                         the app returned AX error {err}.",
                        evidence.detail()
                    ),
                    false,
                    Some(evidence),
                    false,
                ));
            }
        }
        // Some collection rows claim a click-like action but Finder returns
        // kAXErrorCannotComplete. Use the same verified selection fallback
        // before surfacing the dispatch error.
        if let Some((snapshot, idle_before, started)) = fallback_selection {
            if let Some(selected_role) =
                crate::input::ax_actions::select_nearest_container(element_ptr)
            {
                let tied = snapshot
                    .as_ref()
                    .is_some_and(SelectionSnapshot::transitioned)
                    && input_quiet(
                        idle_before,
                        super::physical_input_idle_ms(),
                        elapsed_ms(started),
                    );
                return Ok(selection_write_outcome(
                    tied,
                    &selected_role,
                    (idx, &role, &title),
                    Some(err),
                ));
            }
        }
        return unresolved_perform_error(ax_action, err, (idx, &role, &title));
    }

    let mut summary = format!("✅ Performed {ax_action} on [{idx}] {role} \"{title}\".");
    drop(fallback_selection);
    let evidence = probe.observe();
    if let Some(evidence) = evidence {
        summary.push_str(&format!(" Read back: {}.", evidence.detail()));
    }

    // AXPopUpButton: list available options, redirect to set_value.
    if role == "AXPopUpButton" {
        let children = unsafe { copy_children(element) };
        if !children.is_empty() {
            let options: Vec<String> = children
                .iter()
                .filter_map(|&child| {
                    let t = unsafe { copy_string_attr(child, "AXTitle") }.unwrap_or_default();
                    let v = unsafe { copy_string_attr(child, "AXValue") }.unwrap_or_default();
                    if t.is_empty() && v.is_empty() {
                        return None;
                    }
                    Some(if v.is_empty() || v == t {
                        format!("\"{t}\"")
                    } else {
                        format!("\"{t}\" (value: {v})")
                    })
                })
                .collect();
            for &child in &children {
                unsafe {
                    CFRelease(child as _);
                }
            }

            if !options.is_empty() {
                let opt_list = options.join(", ");
                summary.push_str(
                    "\n\nThis is a popup/select button; its menu is open. Choose an item \
                     by clicking its AXMenuItem from a fresh get_window_state, or select \
                     in one call with:\n  set_value(pid, element_token, value)\n\
                     Available options: [",
                );
                summary.push_str(&opt_list);
                summary.push(']');
            }
        }
    }

    // Advertised-action warning: non-fatal but surfaces likely no-ops. Also the
    // machine-readable `suspected_noop` signal returned to the caller.
    let suspected_noop = !advertised.contains(&ax_action.to_string());
    if suspected_noop {
        let adv_list = if advertised.is_empty() {
            "none".into()
        } else {
            advertised.join(", ")
        };
        summary.push_str(&format!(
            "\n⚠️ Element does not advertise {ax_action} (actions: {adv_list}). \
             Action may have been a no-op."
        ));
    }

    // A text input in web content takes DOM focus after the press: poll its
    // AXFocused and return as soon as it reads focused (no fixed settle). A
    // native text field has no DOM focus to wait for.
    if needs_web_focus_settle(ax_action, &role, || unsafe { ancestor_roles(element) }) {
        watch_for_change(
            ObservationScope::Watch,
            std::time::Instant::now(),
            &crate::ax::scroll_area::SystemClock,
            || {
                (unsafe { copy_bool_attr(element, "AXFocused") } == Some(true))
                    .then_some(ClickEvidence::Focused)
            },
        );
    }

    // Show focus-rect highlight around the element (matches Swift showFocusRect).
    // Also move the cursor to the element center so the glide animation plays.
    if let Some(rect) = unsafe { element_screen_rect(element) } {
        // Drive THIS session's cursor (threaded in via `cursor_key`), matching
        // the keyed glide already played in the invoke body above. The keyed
        // glide already played on the session's cursor in the invoke body.
        crate::cursor::overlay::send_command(
            cursor_key.to_owned(),
            cursor_overlay::OverlayCommand::ShowFocusRect(Some(rect)),
        );
        // Animate cursor to element center.
        let cx = rect[0] + rect[2] / 2.0;
        let cy = rect[1] + rect[3] / 2.0;
        crate::cursor::overlay::send_command(
            cursor_key.to_owned(),
            cursor_overlay::OverlayCommand::ClickPulse { x: cx, y: cy },
        );
    }
    let _ = pid;
    let _ = window_id; // used by caller context

    Ok((summary, suspected_noop, evidence, false))
}

fn background_pixel_ax_eligible(
    foreground: bool,
    button: &str,
    count: usize,
    no_modifiers: bool,
    action: &str,
) -> bool {
    // Focus is a distinct request; a pixel press uses the window-addressed
    // event route so its requested click count reaches the target.
    !foreground && button == "left" && count == 1 && no_modifiers && action == "focus"
}

fn background_hit_uses_ax(focus_only: bool) -> bool {
    focus_only
}

fn text_input_click_role(role: &str) -> bool {
    matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXSearchField" | "AXSecureTextField" | "AXComboBox"
    )
}

/// An ancestor row's AXPress counts only with evidence read on that row
/// (a selection transition, the window retitled to its name, a sheet or
/// window it opened): an already-selected row that merely activates cannot
/// pass as a delivered click, and the press may still have acted.
fn ancestor_press_outcome(evidence: Option<ClickEvidence>) -> AxPressOutcome {
    match evidence {
        Some(evidence) => AxPressOutcome::Delivered(Some(evidence)),
        None => AxPressOutcome::Indeterminate,
    }
}

/// What the background PX hit-test route did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PxAxPress {
    /// Delivered through AX, with the evidence read after it, if any, and
    /// the element the point resolved to as the public `hit`.
    Delivered(Option<ClickEvidence>, Option<serde_json::Value>),
    /// Nothing was sent through AX: the routed pixel click goes next.
    FallThrough,
    /// An AX action may have been delivered without evidence: stop. Carries
    /// the element the point resolved to, as the public hit.
    Indeterminate(Option<serde_json::Value>),
}

/// The result of a background PX click delivered through AX: confirmed
/// with its evidence and a typed record, or unverifiable when the press
/// showed no readable change. `hit` is the element under the point.
fn px_ax_press_result(
    label: &str,
    evidence: Option<ClickEvidence>,
    hit: Option<serde_json::Value>,
) -> ToolResult {
    use cua_driver_core::action_record::{ActionTransport, ActualDelivery, RequestedDelivery};
    let (transport, requested, actual) = (
        ActionTransport::MacosAxAction,
        RequestedDelivery::Background,
        ActualDelivery::Background,
    );
    let mut result = match evidence {
        Some(evidence) => ToolResult::text(format!(
            "✅ PX hit-test {label} the background element via AX; {}.",
            evidence.detail()
        ))
        .with_structured(serde_json::json!({
            "path": "ax",
            "verified": true,
            "effect": "confirmed",
            "evidence": [evidence.structured()],
        }))
        .with_action_record(evidence.record(transport, requested, actual)),
        None => ToolResult::text(format!(
            "✅ PX hit-test {label} the background element via AX. No change the driver can \
             read followed it: check the window before repeating it."
        ))
        .with_structured(serde_json::json!({
            "path": "ax",
            "verified": false,
            "effect": "unverifiable"
        }))
        .with_action_record(unverifiable_click_record(transport, requested, actual)),
    };
    if let (Some(hit), Some(structured)) = (hit, result.structured_content.as_mut()) {
        structured["hit"] = hit;
    }
    result
}

/// Outcome of the AX press route for a point click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AxPressOutcome {
    /// The press was delivered; the evidence read after it, if any.
    Delivered(Option<ClickEvidence>),
    /// No actionable element: fall through to the routed pixel path.
    Unavailable,
    /// An AX action may have been delivered: stop, do not fall through.
    Indeterminate,
}

/// Which ancestor action a click should use, if any. Selection first:
/// AXPress on the nearest ancestor that advertises it. AXOpen only on a
/// Finder sidebar cell (AXCell below AXRow below AXOutline), where open IS
/// the single-click navigation gesture — an outline elsewhere can be a file
/// or document tree, where a click must select, not open.
fn ancestor_action_kind(
    chain: &[(Option<String>, Vec<String>, bool)],
    is_finder: bool,
) -> Option<(usize, &'static str)> {
    if !is_finder {
        return None;
    }
    // Every fallback action requires the Finder sidebar: the FIRST outline
    // above the candidate must be described as "sidebar". A press anywhere
    // else (content outlines, other apps, other controls) is not selection
    // evidence and falls through to the pixel route.
    if chain
        .iter()
        .find(|(role, _, _)| role.as_deref() == Some("AXOutline"))
        .is_some_and(|(_, _, sidebar)| !*sidebar)
    {
        return None;
    }
    let sidebar_above = |index: usize| {
        chain[index + 1..]
            .iter()
            .find(|(role, _, _)| role.as_deref() == Some("AXOutline"))
            .is_some_and(|(_, _, sidebar)| *sidebar)
    };
    // Selection first, across the whole chain.
    for (index, (role, actions, _)) in chain.iter().enumerate() {
        if matches!(role.as_deref(), Some("AXRow" | "AXCell"))
            && actions.iter().any(|a| a == "AXPress")
            && sidebar_above(index)
        {
            return Some((index, "AXPress"));
        }
    }
    for (index, (role, actions, _)) in chain.iter().enumerate() {
        if matches!(role.as_deref(), Some("AXRow" | "AXCell"))
            && actions.iter().any(|a| a == "AXOpen")
            && sidebar_above(index)
        {
            return Some((index, "AXOpen"));
        }
    }
    None
}

/// AXPress the element, or the nearest equivalent actionable ancestor. The
/// window-points guard is rechecked immediately before EVERY action and an
/// AX error that does not prove non-delivery stops as indeterminate.
unsafe fn ax_press_with_ancestors(
    element: crate::ax::bindings::AXUIElementRef,
    dispatch_guard: &Option<super::window_points::DispatchGuard>,
    pid: i32,
    reads: ClickReads,
) -> Result<AxPressOutcome, super::window_points::WindowMoved> {
    use crate::ax::bindings::{copy_action_names, copy_element_attr, copy_string_attr};
    use core_foundation::base::CFRelease;
    use core_foundation::string::CFString;

    let leaf_actions = copy_action_names(element);
    if leaf_actions.iter().any(|a| a == "AXPress") {
        let role = copy_string_attr(element, "AXRole").unwrap_or_default();
        let probe = ClickProbe::capture(
            Some(element),
            is_toggle_press("AXPress", &role, &leaf_actions),
            reads,
        );
        super::window_points::check_guard(dispatch_guard)?;
        let action = CFString::new("AXPress");
        let err = AXUIElementPerformAction(element, action.as_concrete_TypeRef());
        if err == kAXErrorSuccess {
            return Ok(AxPressOutcome::Delivered(probe.observe()));
        }
        if !perform_error_proves_nothing_was_sent(err) {
            // The press may have acted: only evidence read after it says so.
            return Ok(ancestor_press_outcome(probe.observe()));
        }
    }
    // The hit's OWN outline role counts in the boundary decision (it is
    // not part of the ancestor chain). Returning here retains nothing.
    if copy_string_attr(element, "AXRole").as_deref() == Some("AXOutline")
        && !copy_string_attr(element, "AXDescription")
            .is_some_and(|d| d.eq_ignore_ascii_case("sidebar"))
    {
        return Ok(AxPressOutcome::Unavailable);
    }
    // Collect the ancestor chain first, so an AXOpen decision can see the
    // outline above the cell rather than only the ancestors visited so far.
    let mut chain: Vec<(
        Option<String>,
        Vec<String>,
        crate::ax::bindings::AXUIElementRef,
        bool,
    )> = Vec::new();
    let mut current = element;
    for _ in 0..4 {
        let Some(parent) = copy_element_attr(current, "AXParent") else {
            break;
        };
        // Every parent stays retained in the chain until the single cleanup
        // below: no entry is used after release, and nothing is freed twice.
        current = parent;
        let role = copy_string_attr(current, "AXRole");
        let actions = copy_action_names(current);
        let sidebar = role.as_deref() == Some("AXOutline")
            && copy_string_attr(current, "AXDescription")
                .is_some_and(|d| d.eq_ignore_ascii_case("sidebar"));
        chain.push((role, actions, current, sidebar));
    }
    let roles_actions: Vec<(Option<String>, Vec<String>, bool)> = chain
        .iter()
        .map(|(role, actions, _, sidebar)| (role.clone(), actions.clone(), *sidebar))
        .collect();
    let is_finder = crate::apps::bundle_id_for_pid(pid).as_deref() == Some("com.apple.finder");
    let chosen = ancestor_action_kind(&roles_actions, is_finder);
    let Some((index, action_name)) = chosen else {
        for (_, _, el, _) in chain {
            CFRelease(el as _);
        }
        return Ok(AxPressOutcome::Unavailable);
    };
    let target = chain[index].2;
    // What the press may change, read on the pressed row or cell itself
    // (its selection, the window retitled to its name: Finder's sidebar).
    let probe = ClickProbe::capture(Some(target), false, reads);
    if let Err(moved) = super::window_points::check_guard(dispatch_guard) {
        for (_, _, el, _) in chain {
            CFRelease(el as _);
        }
        return Err(moved);
    }
    let action = CFString::new(action_name);
    let err = AXUIElementPerformAction(target, action.as_concrete_TypeRef());
    // Finder opens a sidebar location and still returns an AX error
    // (kAXErrorAttributeUnsupported): an error that does not prove the
    // action never reached the app is observed like a success.
    let may_have_acted = err == kAXErrorSuccess || !perform_error_proves_nothing_was_sent(err);
    let evidence = may_have_acted.then(|| probe.observe()).flatten();
    for (_, _, el, _) in chain {
        CFRelease(el as _);
    }
    if err == kAXErrorSuccess {
        if action_name == "AXPress" {
            return Ok(ancestor_press_outcome(evidence));
        }
        return Ok(AxPressOutcome::Delivered(evidence));
    }
    if !perform_error_proves_nothing_was_sent(err) {
        return Ok(ancestor_press_outcome(evidence));
    }
    Ok(AxPressOutcome::Unavailable)
}

#[cfg(test)]
mod ancestor_action_tests {
    use super::ancestor_action_kind;

    fn chain(items: &[(&str, &[&str], bool)]) -> Vec<(Option<String>, Vec<String>, bool)> {
        items
            .iter()
            .map(|(role, actions, sidebar)| {
                (
                    Some((*role).to_owned()),
                    actions.iter().map(|a| (*a).to_owned()).collect(),
                    *sidebar,
                )
            })
            .collect()
    }

    #[test]
    fn a_finder_sidebar_cell_opens_but_a_file_row_only_selects() {
        let sidebar = chain(&[
            ("AXRow", &[], false),
            ("AXCell", &["AXOpen"], false),
            ("AXRow", &[], false),
            ("AXOutline", &[], true),
        ]);
        assert_eq!(ancestor_action_kind(&sidebar, true), Some((1, "AXOpen")));
        assert_eq!(ancestor_action_kind(&sidebar, false), None);
        let file_row = chain(&[("AXRow", &["AXOpen"], false), ("AXTable", &[], false)]);
        assert_eq!(ancestor_action_kind(&file_row, true), None);
        let row = chain(&[
            ("AXCell", &["AXOpen"], false),
            ("AXRow", &["AXPress"], false),
            ("AXOutline", &[], true),
        ]);
        assert_eq!(ancestor_action_kind(&row, true), Some((1, "AXPress")));
    }

    #[test]
    fn a_content_outline_under_a_sidebar_does_not_open_and_other_controls_do_not_select() {
        let nested = chain(&[
            ("AXCell", &["AXOpen"], false),
            ("AXRow", &[], false),
            ("AXOutline", &[], false),
            ("AXOutline", &[], true),
        ]);
        assert_eq!(ancestor_action_kind(&nested, true), None);
        let button = chain(&[("AXButton", &["AXPress"], false)]);
        assert_eq!(ancestor_action_kind(&button, true), None);
        let row = chain(&[("AXRow", &["AXPress"], false)]);
        assert_eq!(ancestor_action_kind(&row, false), None);
        // A row press without any outline above is not used either.
        let no_outline = chain(&[
            ("AXCell", &["AXOpen"], false),
            ("AXRow", &["AXPress"], false),
        ]);
        assert_eq!(ancestor_action_kind(&no_outline, true), None);
    }

    #[test]
    fn an_ancestor_press_needs_evidence_read_on_the_pressed_row() {
        use super::{ancestor_press_outcome, AxPressOutcome, ClickEvidence};
        for evidence in [
            ClickEvidence::Selected,
            ClickEvidence::TitleNamesTarget,
            ClickEvidence::ToggleChanged,
        ] {
            assert_eq!(
                ancestor_press_outcome(Some(evidence)),
                AxPressOutcome::Delivered(Some(evidence))
            );
        }
        assert_eq!(ancestor_press_outcome(None), AxPressOutcome::Indeterminate);
    }
}

#[cfg(test)]
mod web_focus_settle_tests {
    use super::needs_web_focus_settle;

    fn roles(list: &[&str]) -> impl FnOnce() -> Vec<String> {
        let list: Vec<String> = list.iter().map(|role| (*role).to_owned()).collect();
        move || list
    }

    #[test]
    fn a_text_input_in_web_content_waits_for_dom_focus() {
        assert!(needs_web_focus_settle(
            "AXPress",
            "AXTextField",
            roles(&["AXGroup", "AXWebArea", "AXScrollArea", "AXWindow"])
        ));
        assert!(needs_web_focus_settle(
            "AXPress",
            "AXTextArea",
            roles(&["AXWebArea", "AXWindow"])
        ));
    }

    #[test]
    fn a_native_text_input_does_not_wait() {
        assert!(!needs_web_focus_settle(
            "AXPress",
            "AXTextField",
            roles(&["AXGroup", "AXScrollArea", "AXWindow"])
        ));
        assert!(!needs_web_focus_settle("AXPress", "AXTextArea", roles(&[])));
    }

    #[test]
    fn only_a_press_on_a_text_input_is_considered() {
        for (action, role) in [
            ("AXShowMenu", "AXTextField"),
            ("AXPress", "AXButton"),
            ("AXPress", "AXCheckBox"),
        ] {
            assert!(!needs_web_focus_settle(action, role, || {
                panic!("ancestors are not read for {action} on {role}")
            }));
        }
    }
}

#[cfg(test)]
mod text_and_web_routing_tests {
    use super::{background_hit_uses_ax, background_pixel_ax_eligible, text_input_click_role};

    #[test]
    fn only_unmodified_background_focus_uses_pixel_ax_prelude() {
        assert!(background_pixel_ax_eligible(
            false, "left", 1, true, "focus"
        ));
        assert!(!background_pixel_ax_eligible(
            false, "left", 1, true, "press"
        ));
        assert!(!background_pixel_ax_eligible(
            true, "left", 1, true, "focus"
        ));
        assert!(!background_pixel_ax_eligible(
            false, "left", 2, true, "focus"
        ));
        assert!(!background_pixel_ax_eligible(
            false, "right", 1, true, "focus"
        ));
        assert!(!background_pixel_ax_eligible(
            false, "left", 1, false, "focus"
        ));
    }

    #[test]
    fn pixel_presses_use_events_while_focus_uses_ax() {
        assert!(!background_hit_uses_ax(false));
        assert!(background_hit_uses_ax(true));
    }

    #[test]
    fn every_text_input_role_uses_focus_instead_of_a_hollow_press() {
        for role in [
            "AXTextField",
            "AXTextArea",
            "AXSearchField",
            "AXSecureTextField",
            "AXComboBox",
        ] {
            assert!(text_input_click_role(role), "{role}");
        }
        for role in ["AXButton", "AXRow", "AXStaticText", "AXPopUpButton"] {
            assert!(!text_input_click_role(role), "{role}");
        }
    }
}

#[cfg(test)]
mod selection_fallback_tests {
    use super::{selection_pixel_target, selection_readback_confirms};

    #[test]
    fn selection_pixel_uses_container_center_in_both_coordinate_spaces() {
        let target = selection_pixel_target((360.0, 240.0), (100.0, 80.0));
        assert_eq!(target.screen_x, 360.0);
        assert_eq!(target.screen_y, 240.0);
        assert_eq!(target.window_x, 260.0);
        assert_eq!(target.window_y, 160.0);
    }

    #[test]
    fn plain_click_requires_selected_readback() {
        assert!(selection_readback_confirms(false, true, false, true));
        assert!(
            !selection_readback_confirms(true, true, false, true),
            "a row that was already selected shows nothing about this click"
        );
        assert!(!selection_readback_confirms(false, false, false, true));
    }

    #[test]
    fn modified_click_requires_a_transition_and_preserves_prior_selection() {
        assert!(selection_readback_confirms(false, true, true, true));
        assert!(selection_readback_confirms(true, false, true, true));
        assert!(!selection_readback_confirms(true, true, true, true));
        assert!(!selection_readback_confirms(false, true, true, false));
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Check `click`'s `action` before any dispatch. An element click takes any
/// name here: whether the element offers it (increment, decrement, a custom
/// action) is checked against the live element before anything moves. A pixel
/// click takes the AX action names `map_action` knows, and also `focus`: the
/// background focus rung that pixel `type_text`, `press_key` and `scroll`
/// use to put the keyboard focus on a point (`focus_by_pixel`). It sets
/// AXFocused on the element under the point instead of pressing it.
fn validate_click_action(action: &str, element_target: bool) -> anyhow::Result<()> {
    if element_target {
        anyhow::ensure!(
            !action.trim().is_empty(),
            "action is empty: use an action the target element lists"
        );
        return Ok(());
    }
    if action == "focus" {
        return Ok(());
    }
    crate::input::ax_actions::map_action(action).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capture_precedes_prelude_front_change() {
        use std::cell::Cell;
        let front = Cell::new(1);
        let (expected, _, ()) =
            capture_before_pixel_prelude(|| Some(front.get()), || (), |_| async { front.set(2) })
                .await;
        assert_eq!(expected, Some(1));
        assert_eq!(front.get(), 2);
    }

    #[tokio::test]
    async fn baseline_capture_precedes_dispatch_reset() {
        use std::cell::Cell;
        let idle = Cell::new(1000);
        let (_, baseline, ()) =
            capture_before_pixel_prelude(|| Some(1), || idle.get(), |_| async { idle.set(0) })
                .await;
        assert_eq!(baseline, 1000);
        assert_eq!(idle.get(), 0);
    }

    struct FakeRestore {
        front: std::sync::atomic::AtomicI32,
        key: std::sync::atomic::AtomicI32,
        focus_posts: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl PixelRestoreOps for FakeRestore {
        fn front(&self) -> Option<i32> {
            Some(self.front.load(std::sync::atomic::Ordering::SeqCst))
        }
        fn key_focus(&self) -> Option<i32> {
            Some(self.key.load(std::sync::atomic::Ordering::SeqCst))
        }
        fn key_window(&self, _: i32) -> Option<u32> {
            Some(7)
        }
        fn allowed(&self, _: Option<i32>) -> bool {
            true
        }
        async fn activate(&self, pid: i32) {
            self.front.store(pid, std::sync::atomic::Ordering::SeqCst);
        }
        async fn focus(&self, previous_pid: i32, _: Option<u32>, _: i32, _: u32) {
            self.key
                .store(previous_pid, std::sync::atomic::Ordering::SeqCst);
            self.focus_posts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn two_background_clicks_restore_original_app_and_window() {
        use std::sync::{atomic::Ordering, Arc};
        let fence = Box::leak(Box::new(super::super::pixel_restore::Fence::default()));
        let ops = Arc::new(FakeRestore {
            front: 2.into(),
            key: 2.into(),
            focus_posts: 0.into(),
        });
        let (epoch, admission) = fence.admit_action(true).await;
        drop(admission);
        let worker = ops.clone();
        super::super::pixel_restore::spawn_restore_with(
            fence,
            2,
            epoch,
            Some(1),
            || super::super::pixel_restore::RestoreCheck::Valid,
            move || async move {
                restore_user_focus_with(
                    worker.as_ref(),
                    2,
                    Some(1),
                    Some(7),
                    Some(9),
                    true,
                    Some(1),
                )
                .await
            },
        );
        tokio::task::yield_now().await;
        let (_, action) = fence.admit_action(true).await;
        drop(action);
        let (_, click_b) = fence.admit_action(true).await;
        assert_eq!(ops.front(), Some(2)); // B observes its target already frontmost.
        drop(click_b);
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
        assert_eq!(ops.front(), Some(1));
        assert_eq!(ops.key_focus(), Some(1));
        assert_eq!(ops.focus_posts.load(Ordering::SeqCst), 1);
    }

    /// The pixel focus rung keeps working after unknown AX action names
    /// started refusing: background pixel typing into Electron depended on it.
    #[test]
    fn pixel_click_accepts_the_internal_focus_action() {
        assert!(validate_click_action("focus", false).is_ok());
        assert!(validate_click_action("press", false).is_ok());
        assert!(validate_click_action("show_alternate_ui", true).is_ok());
        assert!(validate_click_action("hover", false).is_err());
    }

    /// Increment, decrement and custom actions are valid only when the
    /// element lists them, which the click checks against the live element
    /// before anything moves; the entry check must let their names through.
    #[test]
    fn element_clicks_let_listed_action_names_through_the_entry_check() {
        for action in ["increment", "AXDecrement", "Show Details", "보내기"] {
            assert!(validate_click_action(action, true).is_ok(), "{action}");
        }
        assert!(validate_click_action("", true).is_err());
        assert!(validate_click_action("increment", false).is_err());
    }

    #[tokio::test]
    async fn an_unknown_action_is_refused_before_dispatch() {
        let tool = ClickTool::new(Arc::new(ToolState::new(false, false, None)));
        let result = tool
            .invoke(serde_json::json!({"pid": 123, "action": "hover"}))
            .await;
        let structured = result.structured_content.expect("structured refusal");
        assert_eq!(structured["code"], "invalid_arguments");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
    }

    /// #3835: Finder's toolbar view switcher applies an AXPress and returns
    /// -25205. Only a radio button or checkbox qualifies for the read-back.
    #[test]
    fn toggle_press_is_an_advertised_press_on_a_radio_or_checkbox() {
        let press = vec!["AXPress".to_owned()];
        assert!(is_toggle_press("AXPress", "AXRadioButton", &press));
        assert!(is_toggle_press("AXPress", "AXCheckBox", &press));
        assert!(
            !is_toggle_press("AXPress", "AXButton", &press),
            "a button has no value that shows the press"
        );
        assert!(!is_toggle_press("AXPick", "AXRadioButton", &press));
        assert!(
            !is_toggle_press("AXPress", "AXRadioButton", &[]),
            "AXPress not advertised"
        );
    }

    /// cua-driver#21: an AX action that was sent and came back with an error
    /// may still have acted (Finder opens a row and returns -25205; Apple
    /// documents that kAXErrorCannotComplete does not mean failure). It must
    /// report `effect: "unverifiable"` so the caller reads before clicking
    /// again. Only errors that prove nothing reached the app stay failures.
    #[test]
    fn perform_error_after_dispatch_is_unverifiable_unless_nothing_was_sent() {
        for modified in [false, true] {
            let (summary, suspected_noop, verified, via_pixel) =
                unconfirmed_selection_click("AXRow", (3, "AXCell", "a"), modified, "last=None");
            assert!(summary.contains("may have selected"), "{summary}");
            assert!(!suspected_noop && verified.is_none() && via_pixel);
            let structured = ax_click_structured(via_pixel, false, verified, suspected_noop);
            assert_eq!(structured["effect"], "unverifiable");
            assert_eq!(structured["path"], "cgevent");
        }
        use crate::ax::bindings::{
            kAXErrorAPIDisabled, kAXErrorActionUnsupported, kAXErrorAttributeUnsupported,
            kAXErrorCannotComplete, kAXErrorFailure, kAXErrorIllegalArgument,
            kAXErrorInvalidUIElement, kAXErrorNotImplemented,
        };
        for may_have_acted in [
            kAXErrorCannotComplete,
            kAXErrorAttributeUnsupported,
            kAXErrorFailure,
            kAXErrorActionUnsupported,
            kAXErrorNotImplemented,
            -25299, // a code this driver does not know
        ] {
            let (summary, suspected_noop, verified, via_pixel) =
                unresolved_perform_error("AXOpen", may_have_acted, (3, "AXRow", "Desktop"))
                    .unwrap_or_else(|error| panic!("{may_have_acted} must not fail: {error}"));
            assert!(
                summary.contains(&may_have_acted.to_string()),
                "the AX error stays visible: {summary}"
            );
            assert!(!summary.starts_with('✅'), "not a success: {summary}");
            assert!(!suspected_noop && verified.is_none() && !via_pixel);
            let structured = ax_click_structured(via_pixel, false, verified, suspected_noop);
            assert_eq!(structured["effect"], "unverifiable", "{may_have_acted}");
            assert_eq!(structured["verified"], false);
            assert_eq!(structured["path"], "ax");
            assert!(structured.get("escalation").is_none());
        }
        for nothing_sent in [
            kAXErrorIllegalArgument,
            kAXErrorInvalidUIElement,
            kAXErrorAPIDisabled,
        ] {
            let error = unresolved_perform_error("AXOpen", nothing_sent, (3, "AXRow", "Desktop"))
                .expect_err("an error that proves nothing reached the app stays a failure");
            assert!(error.to_string().contains(&nothing_sent.to_string()));
        }
    }

    /// The unverifiable outcome survives the dispatch seam: it stays a
    /// non-error, publishes `effect: "unverifiable"` on the accessibility
    /// route with the AX error in its summary, and gains the read-before-
    /// repeating hint instead of a refusal's (cua-driver#21).
    #[test]
    fn perform_error_after_dispatch_reaches_the_wire_as_an_unconfirmed_click() {
        use cua_driver_core::action_record::{ActionEffect, ActionExecutionRecord};
        use cua_driver_core::recovery_hint::{annotate, Seed};

        let args = serde_json::json!({ "pid": 42, "window_id": 7, "element_token": "s1:3" });
        let codes = [
            crate::ax::bindings::kAXErrorCannotComplete,
            crate::ax::bindings::kAXErrorFailure,
            crate::ax::bindings::kAXErrorNotImplemented,
            -25299, // a code this driver does not know
        ];
        for code in codes {
            for fronted in [false, true] {
                let case = format!("code={code} fronted={fronted}");
                let (summary, suspected_noop, verified, via_pixel) =
                    unresolved_perform_error("AXOpen", code, (3, "AXRow", "Desktop"))
                        .unwrap_or_else(|error| panic!("{case}: {error}"));
                let legacy = ax_click_structured(via_pixel, fronted, verified, suspected_noop);
                let record = ActionExecutionRecord::from_legacy("click", &args, &legacy)
                    .unwrap_or_else(|| panic!("{case}: the legacy click payload normalizes"));
                assert_eq!(record.effect, ActionEffect::Unverifiable, "{case}");

                // What the dispatch seam publishes: the projection plus the
                // producer's text as its summary, then the recovery hint.
                let mut public = record.public_result().expect("projects");
                public.summary = Some(summary.clone());
                let public = serde_json::to_value(public).expect("serializes");
                let mut result = ToolResult::text(summary.clone()).with_structured(public);
                annotate("click", &Seed::from_args(&args), &mut result);

                assert_ne!(result.is_error, Some(true), "{case}");
                let wire = result.structured_content.as_ref().expect("structured");
                assert_eq!(wire["effect"], "unverifiable", "{case}");
                assert_eq!(wire["route"], "accessibility", "{case}");
                assert!(wire.get("error").is_none(), "{case}");
                let wire_summary = wire["summary"].as_str().expect("summary");
                assert!(
                    wire_summary.contains(&code.to_string()),
                    "{case}: {wire_summary}"
                );
                let hint = wire["hint"]
                    .as_str()
                    .expect("an unconfirmed click carries a hint");
                assert!(
                    hint.contains("before clicking again"),
                    "{case}: the caller must read before resending: {hint}"
                );
            }
        }
    }

    /// What a strict MCP client receives for `result`: the tool result after
    /// the `tools/call` boundary normalizes it.
    fn click_wire(result: ToolResult) -> serde_json::Value {
        cua_driver_core::mcp_result::conforming_tool_result(
            "click",
            serde_json::to_value(result).expect("a tool result serializes"),
        )
    }

    /// What the default click path no longer reads, timed on the Dock's
    /// accessibility tree (read-only; nothing is pressed or focused). Run:
    /// BENCH_AX_PID=<pid> cargo test -p platform-macos --release --lib bench_click_reads -- --ignored --nocapture
    #[test]
    #[ignore = "timing benchmark over a live accessibility tree"]
    fn bench_click_reads() {
        use crate::ax::bindings::{
            copy_children, copy_element_attr, AXIsProcessTrusted, AXUIElementCreateApplication,
        };
        if !unsafe { AXIsProcessTrusted() } {
            println!("accessibility is not granted to this process: NOT MEASURED");
            return;
        }
        // The pid of a process whose accessibility tree is read (the Dock is
        // enough): BENCH_AX_PID. Nothing in it is pressed or focused.
        let Some(dock_pid) = std::env::var("BENCH_AX_PID")
            .ok()
            .and_then(|pid| pid.parse::<i32>().ok())
        else {
            println!("BENCH_AX_PID is not set: NOT MEASURED");
            return;
        };
        fn median_ms(mut f: impl FnMut()) -> f64 {
            for _ in 0..5 {
                f();
            }
            let mut samples: Vec<f64> = (0..60)
                .map(|_| {
                    let start = std::time::Instant::now();
                    f();
                    start.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
            samples[samples.len() / 2]
        }
        unsafe {
            let app = AXUIElementCreateApplication(dock_pid);
            let Some(list) = copy_children(app).into_iter().next() else {
                println!("the Dock exposes no children: NOT MEASURED");
                return;
            };
            let Some(item) = copy_children(list).into_iter().next() else {
                println!("the Dock list has no items: NOT MEASURED");
                return;
            };
            let role_read = median_ms(|| {
                let _ = copy_string_attr(item, "AXRole");
            });
            let parent_read = median_ms(|| {
                if let Some(parent) = copy_element_attr(item, "AXParent") {
                    CFRelease(parent as _);
                }
            });
            // The old default: probe capture with every read, the sheet
            // ancestor walk, and the fallback selection snapshot.
            let old_reads = median_ms(|| {
                let probe = ClickProbe::capture(Some(item), false, ClickReads::ALL)
                    .watching_sheet_for_press(item, "AXPress");
                let _snapshot = SelectionSnapshot::capture(item);
                drop(probe);
            });
            // The new default: no read at all.
            let new_reads = median_ms(|| {
                let probe = ClickProbe::capture(Some(item), false, ClickReads::NONE);
                drop(probe);
            });
            println!("one AXRole read: median {role_read:.3} ms; one AXParent read: median {parent_read:.3} ms");
            println!("old default click reads (probe + sheet walk + fallback snapshot) on a depth-2 element: median {old_reads:.3} ms");
            println!("new default click reads: median {new_reads:.3} ms");
        }
    }

    #[test]
    fn a_click_that_was_not_asked_to_read_back_reads_nothing() {
        // No selection, toggle, title or sheet read, and so no watch after
        // the input: the click is one action.
        assert_eq!(ClickReads::for_click(false), ClickReads::NONE);
        assert_eq!(ClickReads::for_click(true), ClickReads::ALL);
    }

    #[test]
    fn a_click_reads_nothing_back_unless_the_caller_asks() {
        let read_back = &def().input_schema["properties"]["read_back"];
        assert_eq!(read_back["type"], "boolean");
        assert!(read_back["description"]
            .as_str()
            .is_some_and(|text| text.contains("Default false")));
    }

    /// Calendar's toolbar view switcher reads AXEnabled=false while its window
    /// is in the background. A background click refuses before anything is
    /// sent, and the caller must see that: an uncoded error reached the wire
    /// as `tool_invocation_failed`, which reads as a write that may have
    /// landed and blocks the caller's next input.
    #[test]
    fn a_background_click_on_a_disabled_target_is_a_coded_refusal_on_the_wire() {
        let error = crate::input::ax_actions::disabled_action_refusal("AXPress", false);
        let result = element_click_failure(&error);
        assert_eq!(result.is_error, Some(true));
        let wire = click_wire(result);
        assert_eq!(wire["isError"], true, "{wire}");
        let structured = &wire["structuredContent"];
        assert_eq!(structured["code"], "background_unavailable", "{wire}");
        assert_eq!(structured["effect"], "refused", "{wire}");
        assert_eq!(structured["dispatched"], false, "{wire}");
        let text = wire["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("AXEnabled=false"), "{text}");
        assert!(text.contains("AXPress was not sent"), "{text}");
        assert!(text.contains("delivery_mode:\"foreground\""), "{text}");
    }

    /// An AX error that proves the action itself never reached the app stays
    /// a plain failure, never a refusal: the click can have written
    /// AXSelected on an ancestor before or after the action (the selection
    /// fallback), so it does not prove that nothing changed. The same holds
    /// for a disabled target in the foreground (the window may already have
    /// been activated and raised) and for an uncoded error from the click's
    /// other steps. This guards against widening the refusal above.
    #[test]
    fn an_ax_error_that_proves_nothing_was_sent_stays_a_plain_failure() {
        use crate::ax::bindings::{
            kAXErrorAPIDisabled, kAXErrorIllegalArgument, kAXErrorInvalidUIElement,
        };
        let mut errors: Vec<anyhow::Error> = [
            kAXErrorIllegalArgument,
            kAXErrorInvalidUIElement,
            kAXErrorAPIDisabled,
        ]
        .into_iter()
        .map(|code| {
            unresolved_perform_error("AXPress", code, (3, "AXRadioButton", "Day"))
                .expect_err("an error that proves nothing reached the app stays a failure")
        })
        .collect();
        errors.push(anyhow::anyhow!(
            "selection target stopped exposing AXSelected before delivery"
        ));
        errors.push(crate::input::ax_actions::disabled_action_refusal(
            "AXPress", true,
        ));
        for error in errors {
            let result = element_click_failure(&error);
            assert_eq!(result.is_error, Some(true), "{error}");
            assert!(result.structured_content.is_none(), "{error}");
            let wire = click_wire(result);
            assert_eq!(wire["structuredContent"]["code"], "tool_invocation_failed");
            assert!(
                wire["structuredContent"].get("dispatched").is_none(),
                "{wire}"
            );
        }
    }

    /// cua-driver#22: the delivery_mode ladder goes element_token, keyboard,
    /// background pixel, and the foreground last with its pointer takeover.
    #[test]
    fn delivery_mode_ladder_puts_foreground_last() {
        let mode = def().input_schema["properties"]["delivery_mode"]["description"]
            .as_str()
            .expect("delivery_mode description");
        crate::tools::pixel_route::tests::assert_foreground_last(
            mode,
            "Escalate in order",
            &["element_token", "keyboard", "background pixel"],
        );
    }

    /// Only errors an app returns after acting qualify; errors that mean the
    /// request never reached the app keep failing (#3835).
    #[test]
    fn only_errors_an_app_returns_after_acting_qualify() {
        assert!(press_error_may_have_acted(-25200));
        assert!(press_error_may_have_acted(-25205));
        assert!(press_error_may_have_acted(-25206));
        for refused in [-25201, -25202, -25204, -25211] {
            assert!(!press_error_may_have_acted(refused), "{refused}");
        }
    }

    /// A checkbox press shows as any state change; a radio press only as the
    /// radio becoming selected.
    #[test]
    fn toggle_press_shows_as_the_value_the_press_produces() {
        assert!(toggle_press_shows("AXCheckBox", "0", "1"));
        assert!(toggle_press_shows("AXCheckBox", "1", "0"));
        assert!(toggle_press_shows("AXCheckBox", "2", "1"), "mixed state");
        assert!(!toggle_press_shows("AXCheckBox", "1", "1"));
        assert!(toggle_press_shows("AXRadioButton", "0", "1"));
        assert!(
            !toggle_press_shows("AXRadioButton", "1", "0"),
            "a radio turning off was another radio's press"
        );
        assert!(!toggle_press_shows("AXRadioButton", "1", "1"));
    }

    /// Surface 5: schema must advertise the new `button` field with the three
    /// canonical values and default to "left". Hermes / Codex / Claude Code
    /// consumers branch on this enum being present.
    #[test]
    fn only_a_window_title_that_becomes_the_target_name_confirms() {
        assert!(title_names_target("Recents", "Downloads", "Downloads"));
        assert!(title_names_target("Recents", " downloads ", "Downloads"));
        assert!(
            !title_names_target("Recents", "AirDrop", "Downloads"),
            "another change"
        );
        assert!(
            !title_names_target("Downloads", "Downloads", "Downloads"),
            "no change"
        );
    }

    #[test]
    fn a_key_window_priming_is_reused_only_for_the_same_window_without_activations() {
        let at = std::time::Instant::now();
        let primed = KeyPrime {
            pid: 2,
            window_id: 7,
            activations: 5,
            focus_generation: 9,
            at,
        };
        let later = |pid, window_id, activations, secs| KeyPrime {
            pid,
            window_id,
            activations,
            focus_generation: 9,
            at: at + std::time::Duration::from_secs(secs),
        };
        assert!(primed.still_holds(&later(2, 7, 5, 1)));
        assert!(!primed.still_holds(&later(2, 8, 5, 1)), "other window");
        assert!(!primed.still_holds(&later(3, 7, 5, 1)), "other app");
        assert!(!primed.still_holds(&later(2, 7, 6, 1)), "an app activated");
        assert!(!primed.still_holds(&later(2, 7, 5, 10)), "too old");
        let refocused = KeyPrime {
            focus_generation: 10,
            ..later(2, 7, 5, 1)
        };
        assert!(
            !primed.still_holds(&refocused),
            "the driver moved some app's key focus since"
        );
    }

    #[test]
    fn background_press_needs_the_key_window_only_in_a_native_background_app() {
        assert!(press_needs_key_window("AXPress", Some(1), 2, true));
        assert!(
            !press_needs_key_window("AXPress", Some(2), 2, true),
            "frontmost"
        );
        assert!(
            !press_needs_key_window("AXPress", Some(1), 2, false),
            "browser or Electron"
        );
        assert!(!press_needs_key_window("AXShowMenu", Some(1), 2, true));
    }

    #[test]
    fn schema_advertises_button_enum() {
        let d = def();
        let props = d.input_schema.get("properties").expect("properties");
        let button = props.get("button").expect("button field present");
        let kind = button.get("type").and_then(|v| v.as_str());
        assert_eq!(kind, Some("string"));
        let enum_vals: Vec<&str> = button
            .get("enum")
            .and_then(|v| v.as_array())
            .expect("button.enum present")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(enum_vals.contains(&"left"));
        assert!(enum_vals.contains(&"right"));
        assert!(enum_vals.contains(&"middle"));
        assert_eq!(props["capture_id"]["type"], "string");
    }

    /// Surface 5 hard constraint: the tool description must mention the
    /// `button` argument and the "left" default so MCP introspection (which
    /// pipes description into LLM prompts) carries the back-compat note.
    #[test]
    fn description_mentions_button_default() {
        let d = def();
        let desc = d.description.to_ascii_lowercase();
        assert!(
            desc.contains("button"),
            "description should mention button arg"
        );
        assert!(
            desc.contains("left"),
            "description should mention left default"
        );
        assert!(
            desc.contains("middle"),
            "description should mention middle button"
        );
    }

    /// Regression for the Swift→Rust port gap: only a raw background left
    /// click with an exact window may intentionally activate the target
    /// without raising it. Other background buttons retain strict suppression,
    /// and the explicit foreground rung owns its separate activation.
    #[test]
    fn raw_background_left_click_restores_focus_without_raise_policy() {
        assert_eq!(
            pixel_activation_policy("left", false, true),
            PixelActivationPolicy::AllowTargetWithoutRaise
        );
        assert_eq!(
            pixel_activation_policy("left", false, false),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("right", false, true),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("middle", false, true),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("left", true, true),
            PixelActivationPolicy::ForegroundAssist
        );
    }

    /// Bench v2: re-posting the focus record to a user window that stayed key
    /// re-activated the user's app after every background pixel click.
    #[test]
    fn user_focus_record_only_when_focus_moved() {
        assert!(!user_focus_needs_restore(false, true, Some(7), 7));
        assert!(!user_focus_needs_restore(false, true, None, 7));
        assert!(!user_focus_needs_restore(false, false, Some(42), 7));
        assert!(user_focus_needs_restore(false, true, Some(42), 7));
        assert!(user_focus_needs_restore(true, false, Some(7), 7));
    }

    /// The no-foreground contract must not depend on the private activation
    /// recipe reporting full success. If that recipe is unavailable or only
    /// partially succeeds but the target is nevertheless observed frontmost,
    /// restore the user's prior app.
    #[test]
    fn failed_private_activation_still_restores_observed_target_focus() {
        assert_eq!(
            background_pixel_restore_pid(
                PixelActivationPolicy::AllowTargetWithoutRaise,
                Some(7),
                42,
                Some(42),
            ),
            Some(7)
        );

        assert_eq!(
            background_pixel_restore_pid(
                PixelActivationPolicy::AllowTargetWithoutRaise,
                Some(7),
                42,
                Some(99),
            ),
            None,
            "do not overwrite an unrelated app that became frontmost"
        );
        assert_eq!(
            background_pixel_restore_pid(
                PixelActivationPolicy::SuppressTarget,
                Some(7),
                42,
                Some(42),
            ),
            None,
            "strict-suppression paths retain their existing ownership"
        );
    }
}

#[cfg(test)]
mod click_evidence_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::time::{Duration, Instant};

    /// A clock that advances only when slept on.
    struct FakeClock {
        now: Cell<Instant>,
        sleeps: RefCell<Vec<Duration>>,
    }

    impl crate::ax::scroll_area::Clock for FakeClock {
        fn now(&self) -> Instant {
            self.now.get()
        }
        fn sleep(&self, duration: Duration) {
            self.sleeps.borrow_mut().push(duration);
            self.now.set(self.now.get() + duration);
        }
    }

    /// Watch with `read(elapsed)`: the result, the reads made, the time the
    /// watch took and its sleeps.
    fn run_watch(
        scope: ObservationScope,
        read: impl Fn(Duration) -> Option<ClickEvidence>,
    ) -> (Option<ClickEvidence>, usize, Duration, Vec<Duration>) {
        let start = Instant::now();
        let clock = FakeClock {
            now: Cell::new(start),
            sleeps: RefCell::new(Vec::new()),
        };
        let reads = Cell::new(0);
        let found = watch_for_change(scope, start, &clock, || {
            reads.set(reads.get() + 1);
            read(clock.now.get() - start)
        });
        let elapsed = clock.now.get() - start;
        (found, reads.get(), elapsed, clock.sleeps.take())
    }

    #[test]
    fn a_click_on_a_plain_button_is_unverifiable_without_a_read_or_a_wait() {
        // Calculator's All Clear: no selectable item, no toggle, no title probe.
        let scope = observation_scope(&[], false, false);
        assert_eq!(scope, ObservationScope::Nothing);
        let (found, reads, elapsed, sleeps) = run_watch(scope, |_| None);
        assert_eq!(found, None);
        assert_eq!(reads, 0);
        assert_eq!(elapsed, Duration::ZERO);
        assert!(sleeps.is_empty(), "{sleeps:?}");
    }

    #[test]
    fn only_a_probe_that_can_still_change_is_watched() {
        assert_eq!(
            observation_scope(&[true], false, false),
            ObservationScope::Nothing,
            "an already selected item cannot turn selected"
        );
        assert_eq!(
            observation_scope(&[true, false], false, false),
            ObservationScope::Watch
        );
        assert_eq!(observation_scope(&[], true, false), ObservationScope::Watch);
        assert_eq!(observation_scope(&[], false, true), ObservationScope::Watch);
    }

    #[test]
    fn a_change_returns_at_once_with_no_settle_wait() {
        // Visible on the first read: two reads, no sleep at all.
        let (found, reads, elapsed, sleeps) = run_watch(ObservationScope::Watch, |_| {
            Some(ClickEvidence::ToggleChanged)
        });
        assert_eq!(found, Some(ClickEvidence::ToggleChanged));
        assert_eq!(reads, 2, "the sighting and its immediate second read");
        assert_eq!(elapsed, Duration::ZERO);
        assert!(sleeps.is_empty(), "{sleeps:?}");
        // Visible from 20 ms: returned at 20 ms, every sleep one poll step.
        let at = Duration::from_millis(20);
        let (found, reads, elapsed, sleeps) = run_watch(ObservationScope::Watch, |elapsed| {
            (elapsed >= at).then_some(ClickEvidence::Selected)
        });
        assert_eq!(found, Some(ClickEvidence::Selected));
        assert_eq!(reads, 4, "reads at 0, 10 and 20 ms, then the second read");
        assert_eq!(elapsed, at);
        assert!(sleeps.iter().all(|sleep| *sleep == CLICK_OBSERVATION_POLL));
    }

    #[test]
    fn a_sighting_the_second_read_does_not_repeat_is_not_evidence() {
        let reads = Cell::new(0);
        let (found, _, elapsed, _) = run_watch(ObservationScope::Watch, |_| {
            reads.set(reads.get() + 1);
            (reads.get() == 1).then_some(ClickEvidence::TitleNamesTarget)
        });
        assert_eq!(found, None);
        assert!(elapsed <= CLICK_OBSERVATION);
    }

    /// Watch where every read takes `cost` on the fake clock.
    fn run_costly_watch(
        cost: Duration,
        read: impl Fn(Duration) -> Option<ClickEvidence>,
    ) -> (Option<ClickEvidence>, Duration) {
        let start = Instant::now();
        let clock = FakeClock {
            now: Cell::new(start),
            sleeps: RefCell::new(Vec::new()),
        };
        let found = watch_for_change(ObservationScope::Watch, start, &clock, || {
            clock.now.set(clock.now.get() + cost);
            read(clock.now.get() - start)
        });
        (found, clock.now.get() - start)
    }

    #[test]
    fn a_confirming_read_that_returns_after_the_deadline_does_not_count() {
        // 7 ms reads: the change is first read at 58 ms, and its second read
        // returns at 65 ms, past the 60 ms deadline.
        let (found, elapsed) = run_costly_watch(Duration::from_millis(7), |elapsed| {
            (elapsed >= Duration::from_millis(50)).then_some(ClickEvidence::Selected)
        });
        assert_eq!(found, None);
        assert!(
            elapsed <= CLICK_OBSERVATION + Duration::from_millis(7),
            "{elapsed:?}"
        );
        // Both reads inside the deadline confirm.
        let (found, _) = run_costly_watch(Duration::from_millis(6), |elapsed| {
            (elapsed >= Duration::from_millis(40)).then_some(ClickEvidence::Selected)
        });
        assert_eq!(found, Some(ClickEvidence::Selected));
    }

    #[test]
    fn focus_confirms_a_text_click_only_as_a_change_with_no_input() {
        assert_eq!(
            text_focus_evidence(Some(false), true),
            Some(ClickEvidence::Focused)
        );
        assert_eq!(
            text_focus_evidence(Some(true), true),
            None,
            "already focused"
        );
        assert_eq!(text_focus_evidence(None, true), None, "unreadable baseline");
        assert_eq!(
            text_focus_evidence(Some(false), false),
            None,
            "input meanwhile"
        );
    }

    #[test]
    fn a_plain_click_on_a_selected_row_is_not_watched() {
        assert_eq!(
            selection_fallback_scope(true, false),
            ObservationScope::Nothing
        );
        assert_eq!(
            selection_fallback_scope(false, false),
            ObservationScope::Watch
        );
        assert_eq!(
            selection_fallback_scope(true, true),
            ObservationScope::Watch
        );
    }

    #[test]
    fn a_first_read_that_returns_after_the_deadline_ends_the_watch() {
        // 27 ms reads: the second read starts at 37 ms and returns at 64 ms,
        // past the deadline, showing a change. No confirming read follows.
        let reads = Cell::new(0);
        let (found, elapsed) = run_costly_watch(Duration::from_millis(27), |elapsed| {
            reads.set(reads.get() + 1);
            (elapsed >= Duration::from_millis(40)).then_some(ClickEvidence::Selected)
        });
        assert_eq!(found, None);
        assert_eq!(reads.get(), 2);
        assert_eq!(elapsed, Duration::from_millis(64));
    }

    #[test]
    fn an_unchanged_watch_stops_inside_the_click_budget_and_late_changes_do_not_count() {
        // Point clicks have 100 ms in all; the read-back takes well under it.
        assert!(CLICK_OBSERVATION < Duration::from_millis(100));
        let (found, _, elapsed, sleeps) = run_watch(ObservationScope::Watch, |_| None);
        assert_eq!(found, None);
        assert_eq!(
            elapsed, CLICK_OBSERVATION,
            "the last read is at the deadline"
        );
        assert!(sleeps.iter().all(|sleep| *sleep <= CLICK_OBSERVATION_POLL));
        let (found, _, _, _) = run_watch(ObservationScope::Watch, |elapsed| {
            (elapsed > CLICK_OBSERVATION).then_some(ClickEvidence::Selected)
        });
        assert_eq!(found, None);
    }

    #[test]
    fn a_sheet_button_watch_waits_for_the_sheet_window_up_to_its_own_bound() {
        let started = std::time::Instant::now();
        let pressed = started + Duration::from_millis(450);
        // Slow preparation: the sheet is watched from the press, not from
        // the reads before it.
        assert_eq!(
            observation_start(ObservationScope::WatchSheet, started, None, Some(pressed)),
            pressed
        );
        // Other probes keep counting from dispatch, and a pointer post's end
        // still wins.
        assert_eq!(
            observation_start(ObservationScope::Watch, started, None, Some(pressed)),
            started
        );
        let posted = started + Duration::from_millis(30);
        assert_eq!(
            observation_start(
                ObservationScope::WatchSheet,
                started,
                Some(posted),
                Some(pressed)
            ),
            posted
        );
        assert_eq!(
            observation_start(ObservationScope::WatchSheet, started, None, None),
            started
        );
        use super::super::sheet_close::SHEET_CLOSE_OBSERVATION;
        let late = CLICK_OBSERVATION + Duration::from_millis(40);
        let (found, _, elapsed, _) = run_watch(ObservationScope::WatchSheet, |elapsed| {
            (elapsed >= late).then_some(ClickEvidence::SheetClosed)
        });
        assert_eq!(found, Some(ClickEvidence::SheetClosed));
        assert!(elapsed < SHEET_CLOSE_OBSERVATION, "{elapsed:?}");
        let (found, _, elapsed, _) = run_watch(ObservationScope::WatchSheet, |_| None);
        assert_eq!(found, None);
        assert_eq!(elapsed, SHEET_CLOSE_OBSERVATION);
        let (found, _, _, _) = run_watch(ObservationScope::WatchSheet, |elapsed| {
            (elapsed > SHEET_CLOSE_OBSERVATION).then_some(ClickEvidence::SheetClosed)
        });
        assert_eq!(found, None);
        assert_eq!(
            ClickEvidence::SheetClosed.kind(),
            cua_driver_core::action_record::EvidenceKind::WindowChange
        );
    }

    #[test]
    fn evidence_needs_known_idle_and_no_input_since_dispatch() {
        assert!(input_quiet(Some(5_000), Some(400), 350));
        assert!(
            !input_quiet(Some(5_000), Some(100), 350),
            "input after dispatch"
        );
        assert!(!input_quiet(None, Some(5_000), 350), "unknown before");
        assert!(!input_quiet(Some(5_000), None, 350), "unknown after");
    }

    #[test]
    fn selection_counts_only_a_retained_item_going_from_unselected_to_selected() {
        assert!(selection_transition_confirms(&[false], &[Some(true)]));
        assert!(
            !selection_transition_confirms(&[true], &[Some(true)]),
            "already selected"
        );
        assert!(!selection_transition_confirms(&[false], &[Some(false)]));
        assert!(
            !selection_transition_confirms(&[false], &[None]),
            "unreadable after"
        );
        // The write landed on an ancestor outside the snapshot: the
        // retained rows show no transition.
        assert!(!selection_transition_confirms(
            &[true, true],
            &[Some(true), Some(true)]
        ));
        assert!(selection_transition_confirms(
            &[true, false],
            &[Some(true), Some(true)]
        ));
        assert!(!selection_transition_confirms(&[], &[]));
    }

    #[test]
    fn a_selection_write_without_a_tied_transition_is_unverifiable() {
        let (text, _, evidence, _) =
            selection_write_outcome(false, "AXRow", (3, "AXCell", "a"), Some(-25204));
        assert!(evidence.is_none());
        assert!(text.contains("could not be tied to this click"), "{text}");
        assert!(text.contains("-25204"));
        let structured = ax_click_structured(false, false, evidence, false);
        assert_eq!(structured["effect"], "unverifiable");
        let (text, _, evidence, _) =
            selection_write_outcome(true, "AXRow", (3, "AXCell", "a"), None);
        assert_eq!(evidence, Some(ClickEvidence::Selected));
        assert!(text.starts_with('✅'));
    }

    #[test]
    fn evidence_maps_to_public_kinds_without_app_text() {
        use cua_driver_core::action_record::{
            ActionEffect, ActionTransport, ActualDelivery, EvidenceKind, RequestedDelivery,
        };
        for (evidence, kind) in [
            (ClickEvidence::Focused, EvidenceKind::AccessibilityReadback),
            (ClickEvidence::Selected, EvidenceKind::AccessibilityReadback),
            (
                ClickEvidence::Deselected,
                EvidenceKind::AccessibilityReadback,
            ),
            (
                ClickEvidence::TitleNamesTarget,
                EvidenceKind::AccessibilityReadback,
            ),
            (
                ClickEvidence::ToggleChanged,
                EvidenceKind::AccessibilityReadback,
            ),
        ] {
            assert_eq!(evidence.kind(), kind);
            let record = evidence.record(
                ActionTransport::MacosAxAction,
                RequestedDelivery::Background,
                ActualDelivery::Background,
            );
            assert_eq!(record.effect, ActionEffect::Confirmed);
            assert_eq!(record.evidence[0].detail, evidence.detail());
            let structured = ax_click_structured(false, false, Some(evidence), false);
            assert_eq!(structured["effect"], "confirmed");
            assert_eq!(structured["evidence"][0]["kind"], evidence.kind_name());
        }
    }

    #[test]
    fn a_px_ax_press_without_evidence_stays_unverifiable() {
        use cua_driver_core::action_record::ActionEffect;
        let hit = pointer_hit_record(
            Some("AXButton".into()),
            Some("All Clear".into()),
            Some("AllClear".into()),
        );
        let plain = px_ax_press_result("pressed", None, hit);
        let structured = plain.structured_content.as_ref().expect("structured");
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(
            structured["hit"],
            serde_json::json!({"role": "AXButton", "label": "All Clear", "identifier": "AllClear"})
        );
        assert_eq!(
            plain.action_record.as_ref().map(|r| r.effect),
            Some(ActionEffect::Unverifiable)
        );
        let sidebar = px_ax_press_result("pressed", Some(ClickEvidence::TitleNamesTarget), None);
        let structured = sidebar.structured_content.as_ref().expect("structured");
        assert_eq!(structured["effect"], "confirmed");
        assert_eq!(structured["verified"], true);
        assert!(
            structured.get("hit").is_none(),
            "no hit reported, none published"
        );
        assert_eq!(
            sidebar.action_record.as_ref().map(|r| r.effect),
            Some(ActionEffect::Confirmed)
        );
    }

    #[test]
    fn a_pointer_hit_names_role_label_and_identifier_and_omits_blanks() {
        assert_eq!(
            pointer_hit_record(Some("AXButton".into()), Some(" ".into()), None),
            Some(serde_json::json!({"role": "AXButton"}))
        );
        assert_eq!(pointer_hit_record(None, Some(String::new()), None), None);
    }
}

#[cfg(test)]
mod finder_title_tests {
    use super::{title_names_target, window_title_name};

    #[test]
    fn a_title_already_naming_the_item_is_not_probed() {
        assert!(!super::title_can_come_to_name("Downloads", "downloads"));
        assert!(!super::title_can_come_to_name(
            "\u{2068}Desktop\u{2069} — Local",
            "Desktop"
        ));
        assert!(super::title_can_come_to_name("Recents", "Downloads"));
    }

    #[test]
    fn finder_location_titles_name_their_sidebar_item() {
        assert_eq!(
            window_title_name("\u{2068}Desktop\u{2069} — Local"),
            "desktop"
        );
        assert!(title_names_target(
            "Recents",
            "\u{2068}Desktop\u{2069} — Local",
            "Desktop"
        ));
        assert!(title_names_target(
            "cua_400f1_wp2_finder",
            "BetaFolder",
            "BetaFolder"
        ));
        assert!(
            !title_names_target(
                "Desktop — Local",
                "\u{2068}Desktop\u{2069} — Local",
                "Desktop"
            ),
            "unchanged"
        );
        assert!(
            !title_names_target("Recents", "Downloads — Local", "Desktop"),
            "another location"
        );
    }
}
