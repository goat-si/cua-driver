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
use super::content_attrs::{read_content_attrs, ContentAttrs};
use super::dense_rows::{
    self, rows_windowed, with_dense_rows_windowed, ChildPlan, Chunk, DeferredRow,
};
use super::launch::{is_still_launching, LaunchStep, LaunchWait};
use super::snapshot::RetainedElement;
use super::window_scope::{
    decide_window_scope, is_window_like_role, ScopeDecision, TopLevelCandidate, WindowScope,
};
use core_foundation::{
    base::{CFEqual, CFGetTypeID, CFRelease, CFRetain, CFTypeRef, TCFType},
    string::CFString,
};
use cua_driver_core::snapshot_compaction::{
    compact_tab_strip, fold_actionless_text_subtrees, format_omitted_tabs_line,
    is_secure_text_field, redact_if_secure, should_probe_secure_text_role, CompactableNode,
    FieldSecurityMeta, TabChildMeta, TabStripCompaction, TAB_STRIP_COMPACTION_THRESHOLD,
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
#[derive(Debug, Clone)]
pub struct AXNode {
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
    mut budget: WalkBudget,
    include_web_content: bool,
) -> TreeWalkResult {
    let ledger = SkipLedger::start();
    let mut nodes: Vec<AXNode> = Vec::new();
    let mut lines: Vec<(usize, String)> = Vec::new(); // (depth, line)
    let mut index_counter = 0usize;
    let mut deferred = Vec::new();
    let mut offscreen_rows = Vec::new();
    let mut batching_disabled = false;
    let mut window_scope: Option<WindowScope> = None;

    unsafe {
        let app_elem = AXUIElementCreateApplication(pid);
        if app_elem.is_null() {
            return TreeWalkResult {
                offscreen_rows: Vec::new(),
                tree_markdown: String::new(),
                nodes,
                truncated: false,
                nodes_skipped: true,
                walk: budget.outcome(),
                // No application AX element at all, so a requested window
                // certainly did not resolve.
                window_scope: window_id.map(|_| WindowScope::AxUnresolved { ax_window_count: 0 }),
            };
        }
        set_messaging_timeout(app_elem);

        // A window-scoped walk of an app that is still launching waits, within
        // the caller's budget, for the app to answer AX (see `super::launch`).
        let mut launch_wait = LaunchWait::new(budget.time_limit());
        let (mut top_level, walk_these) = loop {
            // Chromium/Electron apps (Arc, VS Code, Electron shells) ship their
            // web-content AX tree OFF and only build it once an assistive client
            // asks for it. Without this, the first walk of such an app returns an
            // empty/title-bar-only tree (#1616). Flip the enablement attribute,
            // then — only when the flip actually took and only the first time we
            // see this process lifetime — let the asynchronously-built tree settle
            // before we read it. Native Cocoa apps reject the attribute, so they
            // pay no settle cost. This relies on the MAX_ELEMENTS node cap to keep
            // the now-materialized (potentially large) tree bounded. An app that
            // has not started answering AX rejects the flip without caching the
            // refusal, so each launch-wait attempt asks again.
            let attempt_started = Instant::now();
            super::enablement::ensure_chromium_ax_enabled(pid, app_elem);

            let recovery_deadline = budget
                .time_limit()
                .and_then(|limit| attempt_started.checked_add(limit));
            let mut top_level = copy_top_level(app_elem, pid, window_id, recovery_deadline);

            // Scope: keep non-window children (menu bar) + the target window —
            // but ONLY once the target window has actually been identified. When
            // nothing claims the requested id, `decide_window_scope` reports why
            // and walks nothing; it must never fall back to "everything that isn't
            // a window", which is how issue #2237 returned menu bars as panels.
            let Some(wid) = window_id else {
                let walk = top_level.clone();
                break (top_level, walk);
            };
            let mut decision = scope_top_level(&top_level, pid, wid);
            // A save sheet has a CGWindowID of its own but lives under its
            // document's AXWindow. Walk that parent so the live sheet root and
            // its controls are published instead of returning an empty panel.
            if matches!(decision.scope, WindowScope::AxUnresolved { .. }) {
                if let Some((parent_window_id, sheet)) = window_holding_sheet(&top_level, wid) {
                    release_all(std::mem::take(&mut top_level));
                    top_level.push(sheet);
                    decision = ScopeDecision {
                        scope: WindowScope::SheetOfWindow { parent_window_id },
                        walk: vec![0],
                    };
                }
            }
            if matches!(decision.scope, WindowScope::AxUnresolved { .. }) {
                match launch_wait.step(attempt_started, Instant::now(), || is_still_launching(pid))
                {
                    LaunchStep::Retry(pause) => {
                        release_all(top_level);
                        std::thread::sleep(pause);
                        continue;
                    }
                    // Still unresolved because the app never finished
                    // launching: say so instead of blaming the window scope.
                    LaunchStep::TimedOut => budget.stop_for_app_lookup_timeout(),
                    LaunchStep::Proceed => {}
                }
            }
            let walk = decision
                .walk
                .iter()
                .map(|&index| top_level[index])
                .collect();
            window_scope = Some(decision.scope);
            break (top_level, walk);
        };

        // Walk each top-level child at depth 0. Its own frame, read only if
        // a large container needs it, bounds what is on screen for children
        // whose app does not list the visible ones.
        for child in walk_these {
            let surface = rows_windowed().then_some(child as usize);
            dense_rows::with_visible_clip(surface, || {
                walk_element(
                    child,
                    0,
                    None,
                    false,
                    &mut nodes,
                    &mut lines,
                    &mut index_counter,
                    &mut budget,
                    &mut batching_disabled,
                    max_depth,
                    include_web_content,
                    &mut deferred,
                )
            });
        }

        // An open context menu of the walked window hangs off a view that
        // does not list it as a child: publish it after the window, each
        // under the same budget, so its items get element tokens.
        let discovery = super::context_menus::discovery_deadline(
            Instant::now(),
            budget.outcome().truncated(),
            budget.remaining(),
        );
        if let (Some(wid), Some(deadline)) = (window_id.filter(|_| !nodes.is_empty()), discovery) {
            let mut walked_menus: Vec<[f64; 4]> = nodes
                .iter()
                .filter(|node| node.role == "AXMenu")
                .filter_map(|node| node.frame)
                .collect();
            for frame in super::context_menus::open_menu_window_frames(pid) {
                let Some((menu, menu_frame, owner)) =
                    super::context_menus::menu_in_window(pid, frame, deadline)
                else {
                    continue;
                };
                // Released with the other top-level elements after the walk.
                top_level.push(menu);
                if !super::context_menus::publish_menu(menu_frame, frame, owner, wid, &walked_menus)
                {
                    continue;
                }
                walked_menus.extend(menu_frame);
                walk_element(
                    menu,
                    0,
                    None,
                    false,
                    &mut nodes,
                    &mut lines,
                    &mut index_counter,
                    &mut budget,
                    &mut batching_disabled,
                    max_depth,
                    include_web_content,
                    &mut deferred,
                );
            }
        }

        if !deferred.is_empty() {
            let search_offscreen = query.is_some_and(|q| !q.trim().is_empty());
            let skipped = with_dense_rows_windowed(false, || {
                dense_rows::walk_deferred(&mut deferred, search_offscreen, |row| {
                    let mut chunk = Chunk {
                        lines: Vec::new(),
                        nodes: Vec::new(),
                    };
                    walk_element(
                        row.element.as_ptr() as AXUIElementRef,
                        row.depth,
                        row.parent_index,
                        row.in_web_content,
                        &mut chunk.nodes,
                        &mut chunk.lines,
                        &mut index_counter,
                        &mut budget,
                        &mut batching_disabled,
                        max_depth,
                        include_web_content,
                        &mut Vec::new(),
                    );
                    chunk
                })
            });
            if skipped {
                note_skipped();
            }
            let (spliced_lines, spliced_nodes, runs) =
                dense_rows::splice_and_renumber(lines, nodes, deferred);
            lines = spliced_lines;
            nodes = spliced_nodes;
            offscreen_rows = runs;
        }

        // Release all top-level elements (copy_children / copy_ax_windows both retain).
        release_all(top_level);

        CFRelease(app_elem as CFTypeRef);
    }

    let walk = budget.outcome();
    let nodes_skipped = ledger.finish();
    // Folding merges text rows into their parent; an exhaustive walk keeps
    // every row it visited.
    if !exhaustive_nodes() {
        fold_actionless_text_subtrees(&mut lines, &mut nodes);
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

/// The application's top-level AX children: `AXChildren` ∪ `AXWindows`.
///
/// `AXChildren` omits windows when the app isn't frontmost (AppKit
/// limitation); `AXWindows` returns the window list regardless of activation
/// state, so the union is the only way to see background windows. Every
/// returned element is retained; release them with [`release_all`].
unsafe fn copy_top_level(
    app_elem: AXUIElementRef,
    pid: i32,
    window_id: Option<u32>,
    recovery_deadline: Option<Instant>,
) -> Vec<AXUIElementRef> {
    let mut top_level = children_or_skipped(copy_children_checked(app_elem));
    // A requested window on another Space is absent from AXWindows; the
    // `_including` variant recovers it by exact CGWindowID.
    let from_windows = match window_id {
        Some(wid) => copy_ax_windows_including(app_elem, pid, wid),
        None => copy_ax_windows(app_elem),
    };
    for w in from_windows {
        // AXChildren and AXWindows can return different proxy pointers for
        // the same native window. CFEqual compares their AX identity;
        // pointer equality alone duplicates the whole subtree and can turn
        // one exact dialog action into a false ambiguity.
        if !top_level
            .iter()
            .any(|&e| CFEqual(e as CFTypeRef, w as CFTypeRef) != 0)
        {
            top_level.push(w);
        } else {
            // Already present — release the extra retain from copy_ax_windows.
            CFRelease(w as CFTypeRef);
        }
    }

    if let Some(wid) = window_id {
        let already_present = top_level.iter().any(|&element| {
            set_messaging_timeout(element);
            ax_get_window_id(element) == Some(wid)
        });
        if !already_present && !recovery_deadline_reached(recovery_deadline, Instant::now()) {
            if let Some(bounds) = crate::windows::window_bounds_by_id(wid) {
                let hit = window_center_hit_if_live(recovery_deadline, Instant::now(), || {
                    element_at_screen_position(
                        pid,
                        bounds.x + bounds.width * 0.5,
                        bounds.y + bounds.height * 0.5,
                    )
                });
                if let Some(root) = hit.flatten().and_then(|element| {
                    recover_window_root_at_center(element, wid, recovery_deadline)
                }) {
                    if top_level
                        .iter()
                        .any(|&element| CFEqual(element as CFTypeRef, root as CFTypeRef) != 0)
                    {
                        CFRelease(root as CFTypeRef);
                    } else {
                        top_level.push(root);
                    }
                }
            }
        }
    }
    top_level
}

const MAX_WINDOW_RECOVERY_ANCESTORS: usize = 32;

fn recovery_deadline_reached(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|deadline| now >= deadline)
}

fn window_center_hit_if_live<T>(
    deadline: Option<Instant>,
    now: Instant,
    hit: impl FnOnce() -> T,
) -> Option<T> {
    (!recovery_deadline_reached(deadline, now)).then(hit)
}

fn topmost_window_match(window_ids: &[Option<u32>], requested: u32) -> Option<usize> {
    window_ids
        .iter()
        .rposition(|window_id| *window_id == Some(requested))
}

/// Recover the highest matching ancestor of a center-hit element. The hit and
/// every parent are retained; keep only one extra retain on the returned root.
unsafe fn recover_window_root_at_center(
    hit: AXUIElementRef,
    requested: u32,
    deadline: Option<Instant>,
) -> Option<AXUIElementRef> {
    let mut ancestors = vec![hit];
    for _ in 1..MAX_WINDOW_RECOVERY_ANCESTORS {
        if recovery_deadline_reached(deadline, Instant::now()) {
            release_all(ancestors);
            return None;
        }
        let current = *ancestors.last().expect("the hit is always present");
        set_messaging_timeout(current);
        let Some(parent) = copy_element_attr(current, "AXParent") else {
            break;
        };
        ancestors.push(parent);
        if recovery_deadline_reached(deadline, Instant::now()) {
            release_all(ancestors);
            return None;
        }
    }

    let mut window_ids = Vec::with_capacity(ancestors.len());
    for &element in &ancestors {
        if recovery_deadline_reached(deadline, Instant::now()) {
            release_all(ancestors);
            return None;
        }
        set_messaging_timeout(element);
        window_ids.push(ax_get_window_id(element));
        if recovery_deadline_reached(deadline, Instant::now()) {
            release_all(ancestors);
            return None;
        }
    }
    let root = topmost_window_match(&window_ids, requested).map(|index| {
        CFRetain(ancestors[index] as CFTypeRef);
        ancestors[index]
    });
    release_all(ancestors);
    root
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
unsafe fn scope_top_level(top_level: &[AXUIElementRef], pid: i32, wid: u32) -> ScopeDecision {
    let candidates: Vec<TopLevelCandidate> = top_level
        .iter()
        .map(|&child| {
            set_messaging_timeout(child);
            let role = copy_string_attr(child, "AXRole").unwrap_or_default();
            let subrole = copy_string_attr(child, "AXSubrole");
            let identifier = copy_string_attr(child, "AXIdentifier");
            // Match AX window element → CGWindowID via private SPI.
            // Only windows carry one, so skip the round-trip elsewhere.
            let ax_window_id = if is_window_like_role(&role) {
                ax_get_window_id(child)
            } else {
                None
            };
            TopLevelCandidate {
                role,
                subrole,
                identifier,
                ax_window_id,
            }
        })
        .collect();
    decide_window_scope(&candidates, wid, || {
        crate::windows::resolve_window_owner(pid, wid)
    })
}

/// The CGWindowID of the app window holding an `AXSheet` whose own
/// CGWindowID is `wid`.
unsafe fn window_holding_sheet(
    top_level: &[AXUIElementRef],
    wid: u32,
) -> Option<(u32, AXUIElementRef)> {
    for &window in top_level {
        if copy_string_attr(window, "AXRole").as_deref() != Some("AXWindow") {
            continue;
        }
        let children = copy_children(window);
        let mut sheet = None;
        for child in children {
            set_messaging_timeout(child);
            if sheet.is_none()
                && copy_string_attr(child, "AXRole").as_deref() == Some("AXSheet")
                && ax_get_window_id(child) == Some(wid)
            {
                sheet = Some(child);
            } else {
                CFRelease(child as CFTypeRef);
            }
        }
        if let Some(sheet) = sheet {
            if let Some(parent) = ax_get_window_id(window).filter(|&parent| parent != wid) {
                return Some((parent, sheet));
            }
            CFRelease(sheet as CFTypeRef);
        }
    }
    None
}

/// Release elements retained by [`copy_top_level`].
unsafe fn release_all(elements: Vec<AXUIElementRef>) {
    for element in elements {
        CFRelease(element as CFTypeRef);
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn walk_element(
    element: AXUIElementRef,
    depth: usize,
    parent_index: Option<usize>,
    in_web_content: bool,
    nodes: &mut Vec<AXNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
    max_depth: usize,
    include_web_content: bool,
    deferred: &mut Vec<DeferredRow>,
) {
    if depth > max_depth {
        note_skipped();
        return;
    }
    // Node and time budget: a refused node is counted as discovered but not
    // visited, and the walk unwinds with the partial tree it has.
    if !budget.admit() {
        return;
    }

    // Messaging timeouts are per AX object, not inherited from the application
    // element, so every descendant must be bounded before any attribute read.
    set_messaging_timeout(element);

    let role = copy_string_attr(element, "AXRole").unwrap_or_else(|| {
        // A selector on the role cannot be answered for this node.
        note_skipped();
        "AXUnknown".into()
    });

    let in_web_content = in_web_content || is_web_content_role(&role);
    if in_web_content && !include_web_content {
        note_skipped();
        return;
    }

    // A scroll area that can scroll is a row of its own: it is what scroll
    // targets, and its bar position tells an agent, and a diff read, whether
    // a scroll moved anything. One that cannot scroll is layout only.
    if role == "AXScrollArea" && !in_web_content {
        let vertical = super::scroll_area::bar_position(element, "down");
        let horizontal = super::scroll_area::bar_position(element, "right");
        if let Some(label) = super::scroll_area::position_label(vertical, horizontal) {
            let idx = *counter;
            *counter += 1;
            CFRetain(element as CFTypeRef);
            // An exhaustive walk keeps the area's own labels, read with their
            // failures, so a selector naming it is answered; the bar
            // position stays the row's value.
            let ((title, description, own_value, identifier), exhaustive) = if exhaustive_nodes() {
                match read_content_attrs(element, budget, batching_disabled) {
                    Some(attrs) => {
                        let (labels, complete) = scroll_area_labels(attrs);
                        if !complete {
                            note_skipped();
                        }
                        (labels, true)
                    }
                    None => {
                        note_skipped();
                        ((None, None, None, None), true)
                    }
                }
            } else {
                (
                    (
                        None,
                        trimmed_label(copy_string_attr(element, "AXDescription")),
                        None,
                        copy_string_attr(element, "AXIdentifier"),
                    ),
                    false,
                )
            };
            let node = AXNode {
                element_index: Some(idx),
                role: role.clone(),
                title,
                // A verify read keeps the area's own label order (its value,
                // then its identifier); the bar position is in value_state,
                // which is what the row's structured `value` reports.
                value: if exhaustive { own_value } else { Some(label) },
                description,
                identifier,
                help: None,
                actions: copy_action_names(element),
                element_ptr: element as usize,
                depth,
                parent_element_index: parent_index,
                frame: element_screen_rect(element),
                value_state: vertical.or(horizontal).map(|v| format!("{v:.3}")),
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
                element,
                &role,
                depth + 1,
                Some(idx),
                in_web_content,
                nodes,
                lines,
                counter,
                budget,
                batching_disabled,
                max_depth,
                include_web_content,
                deferred,
            );
            return;
        }
    }

    // Skip pure layout containers that have no interesting content.
    if role == "AXScrollArea" || role == "AXGroup" {
        // An exhaustive walk keeps the container as a display-only row, so a
        // selector naming it is answered rather than assumed absent.
        if exhaustive_nodes() {
            let node = match read_content_attrs(element, budget, batching_disabled) {
                Some(attrs) => {
                    if attrs.read_failed {
                        note_skipped();
                    }
                    display_node(&role, attrs, element, depth, parent_index, in_web_content)
                }
                None => {
                    note_skipped();
                    role_only_node(
                        role.clone(),
                        element as usize,
                        depth,
                        parent_index,
                        in_web_content,
                    )
                }
            };
            lines.push((depth, format_node_line(&node)));
            nodes.push(node);
        }
        // Still recurse — children may be interesting. Layout containers
        // collapse, so children inherit the parent's depth AND the same
        // parent_index (no actionable node was emitted here).
        walk_children(
            element,
            &role,
            depth,
            parent_index,
            in_web_content,
            nodes,
            lines,
            counter,
            budget,
            batching_disabled,
            max_depth,
            include_web_content,
            deferred,
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
        placeholder,
        description,
        identifier,
        help,
        read_failed,
    }) = read_content_attrs(element, budget, batching_disabled)
    else {
        // Only the role is known: a selector on its label cannot be answered.
        note_skipped();
        let node = role_only_node(role, element as usize, depth, parent_index, in_web_content);
        lines.push((depth, format_node_line(&node)));
        nodes.push(node);
        return;
    };
    if read_failed {
        // A label read failed: a selector on it cannot be answered.
        note_skipped();
    }
    let actions = copy_action_names(element);
    let (subrole, placeholder) = if should_probe_secure_text_role(&role, None) {
        let subrole = copy_string_attr(element, "AXSubrole");
        let placeholder = placeholder.or_else(|| copy_string_attr(element, "AXPlaceholderValue"));
        (subrole, placeholder)
    } else {
        (None, placeholder)
    };

    let visible_title = title.as_deref().unwrap_or("").trim().to_owned();
    let visible_description = description.as_deref().unwrap_or("").trim().to_owned();
    let visible_value = value.as_deref().unwrap_or("").trim().to_owned();
    let visible_description =
        window_chrome_button_description(&role, &visible_title, &visible_description, || {
            copy_string_attr(element, "AXSubrole")
        })
        .map(str::to_owned)
        .unwrap_or(visible_description);

    let has_content =
        !visible_title.is_empty() || !visible_description.is_empty() || !visible_value.is_empty();
    // Some native controls expose no AX action names but do expose a writable
    // AXValue. Finder's transient inline-rename field is the important case:
    // rendering it without an element_index leaves an agent able to see the
    // field but unable to call set_value on it. Probe writability only for the
    // small family of value controls so arbitrary display nodes do not pay an
    // extra AX round trip.
    let value_settable = actions.is_empty()
        && role_supports_value_addressing(&role)
        && is_attribute_settable(element, "AXValue");
    // A closed submenu can keep its descendants in AXChildren while reporting
    // those controls disabled. Never assign such a row a live element index:
    // the same native state also causes dispatch to refuse it, and exposing an
    // index for it invites agents to retain an unusable menu target.
    let actionless_control =
        actions.is_empty() && !value_settable && is_actionless_control_role(&role);
    // A row that may be indexed reads its control state and frame in one
    // batched call; any other row reads only its frame, later.
    // A batch that failed past the deadline stops the walk: like a content
    // read that timed out, the row keeps only its role, without an index or
    // its children.
    let control = if !actions.is_empty() || value_settable || actionless_control {
        match super::control_attrs::read_control_attrs(element, budget, batching_disabled) {
            Some(control) => Some(control),
            None => {
                push_timed_out_row(
                    role,
                    element,
                    depth,
                    parent_index,
                    in_web_content,
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
            let node = AXNode {
                identifier: identifier.clone(),
                help: help.clone(),
                ..role_only_node(
                    role.clone(),
                    element as usize,
                    depth,
                    parent_index,
                    in_web_content,
                )
            };
            lines.push((depth, format_node_line(&node)));
            nodes.push(node);
        }
        walk_children(
            element,
            &role,
            depth + 1,
            parent_index,
            in_web_content,
            nodes,
            lines,
            counter,
            budget,
            batching_disabled,
            max_depth,
            include_web_content,
            deferred,
        );
        return;
    }

    let element_ptr = element as usize;
    let frame = match &control {
        Some(control) => control.frame,
        None => match super::control_attrs::read_frame(element, budget, batching_disabled) {
            Some(frame) => frame,
            None => {
                push_timed_out_row(
                    role,
                    element,
                    depth,
                    parent_index,
                    in_web_content,
                    nodes,
                    lines,
                );
                return;
            }
        },
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
            value_description: control
                .value_description
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty()),
            min_value: control.min_value,
            max_value: control.max_value,
            enabled,
            selected: control.selected,
        }
    });
    let security_meta = FieldSecurityMeta {
        role: &role,
        subrole: subrole.as_deref(),
        title: (!visible_title.is_empty()).then_some(visible_title.as_str()),
        description: (!visible_description.is_empty()).then_some(visible_description.as_str()),
        placeholder: placeholder.as_deref(),
        identifier: identifier.as_deref(),
        help: help.as_deref(),
        is_password_flag: false,
    };
    let (redacted_value, redacted_value_state) = redact_if_secure(
        &security_meta,
        (!visible_value.is_empty()).then(|| visible_value.clone()),
        control_state.value_state.clone(),
    );
    let node = if is_actionable {
        let idx = *counter;
        *counter += 1;
        // Retain so the element stays alive in the cache after `copy_children`
        // releases the per-child ref at the end of the caller's loop.
        CFRetain(element as CFTypeRef);
        AXNode {
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
        && copy_bool_attr(element, "AXSelected") != Some(true);
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
        element,
        &role,
        depth + 1,
        next_parent,
        in_web_content,
        nodes,
        lines,
        counter,
        budget,
        batching_disabled,
        max_depth,
        include_web_content,
        deferred,
    );
}

#[allow(clippy::too_many_arguments)]
unsafe fn walk_children(
    element: AXUIElementRef,
    role: &str,
    depth: usize,
    parent_index: Option<usize>,
    in_web_content: bool,
    nodes: &mut Vec<AXNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
    max_depth: usize,
    include_web_content: bool,
    deferred: &mut Vec<DeferredRow>,
) {
    if depth > max_depth {
        // A tree that ends exactly at the cap is still complete.
        match copy_children_checked(element) {
            Some(below) if below.is_empty() => {}
            Some(below) => {
                note_skipped();
                release_all(below);
            }
            None => note_skipped(),
        }
        return;
    }
    let Some(children) = copy_children_checked(element) else {
        note_skipped();
        return;
    };
    let tab_plan = if !in_web_content && children.len() >= TAB_STRIP_COMPACTION_THRESHOLD {
        plan_macos_tab_strip(element, role, &children)
    } else {
        None
    };
    let plan = if tab_plan.is_none()
        && dense_rows::windows_children(rows_windowed(), in_web_content, role, children.len())
    {
        Some(dense_rows::plan_collection_children(
            element, role, &children, budget,
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
                CFRelease(child as CFTypeRef);
                continue;
            }
        }
        let action = plan.as_ref().map_or(ChildPlan::WalkNow, |plan| plan[index]);
        match action {
            ChildPlan::Defer { visible } => deferred.push(DeferredRow {
                anchor: nodes.len(),
                seq: deferred.len(),
                position: index,
                collection: element as usize,
                element: RetainedElement::retain(child as usize),
                depth,
                parent_index,
                in_web_content,
                visible,
                walked: None,
            }),
            ChildPlan::WalkNow => walk_element(
                child,
                depth,
                parent_index,
                in_web_content,
                nodes,
                lines,
                counter,
                budget,
                batching_disabled,
                max_depth,
                include_web_content,
                deferred,
            ),
        }
        CFRelease(child as CFTypeRef);
    }
}

unsafe fn plan_macos_tab_strip(
    parent: AXUIElementRef,
    parent_role: &str,
    children: &[AXUIElementRef],
) -> Option<TabStripCompaction> {
    if !matches!(
        parent_role,
        "AXTabGroup" | "AXGroup" | "AXScrollArea" | "AXToolbar"
    ) {
        return None;
    }
    let parent_title = copy_string_attr(parent, "AXTitle");
    let parent_desc = copy_string_attr(parent, "AXDescription");
    let parent_role_desc = copy_string_attr(parent, "AXRoleDescription");

    let mut child_roles = Vec::with_capacity(children.len());
    let mut child_role_descs = Vec::with_capacity(children.len());
    let mut child_selected = Vec::with_capacity(children.len());
    let mut child_values = Vec::with_capacity(children.len());
    let mut tab_like_count = 0usize;

    for &child in children {
        set_messaging_timeout(child);
        let r = copy_string_attr(child, "AXRole").unwrap_or_default();
        let rd = if r == "AXTab" || r == "AXRadioButton" {
            None
        } else {
            copy_string_attr(child, "AXRoleDescription")
        };
        if cua_driver_core::snapshot_compaction::is_tab_like_child(parent_role, &r, rd.as_deref()) {
            tab_like_count += 1;
            child_selected.push(copy_bool_attr(child, "AXSelected"));
            child_values.push(copy_stringish_attr(child, "AXValue").map(|v| v.state_value));
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

/// Trimmed text, or `None` when empty.
/// A row whose attribute read ran past the walk's deadline: its role only,
/// with no index and no children, as for a content read that timed out.
fn push_timed_out_row(
    role: String,
    element: AXUIElementRef,
    depth: usize,
    parent_index: Option<usize>,
    in_web_content: bool,
    nodes: &mut Vec<AXNode>,
    lines: &mut Vec<(usize, String)>,
) {
    note_skipped();
    let node = role_only_node(role, element as usize, depth, parent_index, in_web_content);
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
    element: AXUIElementRef,
    depth: usize,
    parent_element_index: Option<usize>,
    in_web_content: bool,
) -> AXNode {
    let label = |text: Option<String>| {
        text.map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
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
        title,
        description,
        value,
        identifier: attrs.identifier,
        help: attrs.help,
        ..role_only_node(
            role.to_owned(),
            element as usize,
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
            std::ptr::null_mut(),
            2,
            Some(4),
            false,
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
            read_failed,
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
            read_failed: false,
        };
        let node = super::display_node("AXGroup", attrs, std::ptr::null_mut(), 1, None, false);
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
