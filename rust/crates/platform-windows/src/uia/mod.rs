//! UI Automation (UIA) tree walking for Windows.
//!
//! Produces the same Markdown format as the macOS AX tree:
//!   `INDENT- [N] ControlType "Name" [value="..." id=... actions=[...]]`
//!   `INDENT- ControlType = "Value"` (non-indexed read-only elements)
//!
//! Uses IUIAutomation COM interface (available Windows 7+).
//! Uses IUIAutomationCacheRequest to batch-fetch all properties in one RPC
//! (avoids per-property cross-process calls that make Chrome's 5000-node tree
//!  take >4s when reading each property individually).

use crate::secure_field::{project_node_text, ProjectedNodeText, RawNodeText};
use cua_driver_core::snapshot_compaction::{
    compact_tab_strip, fold_actionless_text_subtrees, format_omitted_tabs_line,
    format_uncounted_offscreen_rows_line, is_compaction_summary_line, is_uia_dense_collection,
    is_uia_offscreen_exempt, plan_uia_collection_children, renumber_indexed_tree,
    splice_deferred_chunks, CompactableNode, DeferredChunk, RenumberableNode, TabChildMeta,
    UiaChildCandidate, UiaChildPlan, TAB_STRIP_COMPACTION_THRESHOLD,
};
use cua_driver_core::walk_budget::WalkBudget;
use windows::core::{Interface, BSTR};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::{
    ExpandCollapseState_Collapsed, ExpandCollapseState_Expanded,
    ExpandCollapseState_PartiallyExpanded, IUIAutomation, IUIAutomationCacheRequest,
    IUIAutomationElement, IUIAutomationElementArray, IUIAutomationExpandCollapsePattern,
    IUIAutomationInvokePattern, IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern,
    ToggleState_Indeterminate, ToggleState_Off, ToggleState_On, TreeScope, TreeScope_Children,
    TreeScope_Element, TreeScope_Subtree, UIA_AutomationIdPropertyId,
    UIA_BoundingRectanglePropertyId, UIA_ControlTypePropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId, UIA_ExpandCollapsePatternId,
    UIA_FullDescriptionPropertyId, UIA_HelpTextPropertyId, UIA_InvokePatternId,
    UIA_IsEnabledPropertyId, UIA_IsOffscreenPropertyId, UIA_IsPasswordPropertyId,
    UIA_LegacyIAccessibleDescriptionPropertyId, UIA_NamePropertyId, UIA_ProcessIdPropertyId,
    UIA_RangeValuePatternId, UIA_ScrollPatternId, UIA_SelectionItemIsSelectedPropertyId,
    UIA_SelectionItemPatternId, UIA_TextPatternId, UIA_TogglePatternId,
    UIA_ToggleToggleStatePropertyId, UIA_ValuePatternId, UIA_ValueValuePropertyId,
};

pub(crate) mod client;
pub mod fg_bypass;
pub(crate) mod focused_document;
pub mod scroll;
pub mod snapshot;
pub mod windows_enum;
pub use snapshot::Snapshots;
pub use windows_enum::enumerate_top_level_windows;

/// Default cap; callers can override via [`walk_tree_bounded`].
pub const DEFAULT_MAX_DEPTH: usize = 25;
/// Default cap; callers can override via [`walk_tree_bounded`].
pub const DEFAULT_MAX_TOTAL_ELEMENTS: usize = 5000;

/// A TogglePattern state as UIA reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckedState {
    On,
    Off,
    Mixed,
}

impl CheckedState {
    /// The `toggle_state` value on the wire and the `toggle=` attribute in the tree.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Mixed => "mixed",
        }
    }
}

/// A single node in the accessibility tree.
///
/// Same shape for the UIA primary path AND the MSAA fallback (used for
/// SAL/VCL window classes — see `msaa.rs`). MSAA-only fields use the
/// `_ptr is IAccessible` / `msaa_role = Some(...)` discriminator.
#[derive(Clone)]
pub struct UiaNode {
    pub element_index: Option<usize>,
    pub control_type: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub automation_id: Option<String>,
    pub help_text: Option<String>,
    pub actions: Vec<String>,
    /// Enabled state reported by UIA. `None` is reserved for fallback
    /// backends that cannot establish it.
    pub enabled: Option<bool>,
    /// Toggle/selection state when the element exposes one of those patterns.
    pub selected: Option<bool>,
    /// TogglePattern state, including indeterminate, which `selected` cannot express.
    /// `None` when the element has no TogglePattern or UIA does not report it.
    pub checked: Option<CheckedState>,
    /// ExpandCollapsePattern state: `PartiallyExpanded` counts as expanded. `None` for a
    /// leaf node, or when the element has no ExpandCollapsePattern.
    pub expanded: Option<bool>,
    /// Raw COM pointer (IUIAutomationElement for UIA path, IAccessible for
    /// MSAA path) as usize. Retained — `Snapshots` Drop releases it via
    /// the `kind`-appropriate vtable.
    pub element_ptr: usize,
    /// Screen-coordinate center, captured at walk time to avoid later COM calls.
    pub center_x: i32,
    pub center_y: i32,
    /// Full screen-coord rect (left, top, right, bottom). Available for
    /// elements that report a meaningful bounding box. Used by the click
    /// tool when `action:"expand"` needs the right-edge offset.
    pub rect: Option<(i32, i32, i32, i32)>,
    /// MSAA role code (e.g. 0x38 = `ROLE_SYSTEM_BUTTONDROPDOWN`). `Some`
    /// iff this node came from the MSAA walker — the click tool checks
    /// this to route `action:"expand"` to a right-edge SendInput click
    /// instead of an unsupported UIA pattern lookup.
    pub msaa_role: Option<i32>,
    /// Depth in the rendered markdown tree (matches the `lines` indent
    /// level). Defaults to 0 when the node came from a builder that
    /// doesn't track depth.
    pub depth: usize,
    /// `element_index` of the nearest actionable ancestor, if any.
    /// Mirrors the markdown's parent-of-this-row.
    pub parent_element_index: Option<usize>,
    /// True when this node is below a UIA Document control. Browser-owned
    /// consent chrome must never match renderer-controlled descendants.
    pub in_web_content: bool,
}

impl CompactableNode for UiaNode {
    fn element_index(&self) -> Option<usize> {
        self.element_index
    }
    fn role(&self) -> &str {
        &self.control_type
    }
    fn depth(&self) -> usize {
        self.depth
    }
    fn set_depth(&mut self, depth: usize) {
        self.depth = depth;
    }
    fn title(&self) -> Option<&str> {
        self.name.as_deref()
    }
    fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }
    fn set_value(&mut self, value: Option<String>) {
        self.value = value;
    }
    fn description(&self) -> Option<&str> {
        self.help_text.as_deref()
    }
    fn format_line(&self) -> String {
        format_node_line(self)
    }
}

impl RenumberableNode for UiaNode {
    fn element_index(&self) -> Option<usize> {
        self.element_index
    }
    fn set_element_index(&mut self, index: Option<usize>) {
        self.element_index = index;
    }
    fn parent_element_index(&self) -> Option<usize> {
        self.parent_element_index
    }
    fn set_parent_element_index(&mut self, parent: Option<usize>) {
        self.parent_element_index = parent;
    }
}

pub struct UiaTreeResult {
    pub tree_markdown: String,
    pub nodes: Vec<UiaNode>,
    /// True when the unprojected native walk visited every node exposed by
    /// the selected root without a bound or enumeration failure skipping a
    /// node/subtree. A `query` only projects the response and does not change
    /// this underlying snapshot property.
    pub elements_complete: bool,
}

#[derive(Debug)]
struct WalkState<'a> {
    budget: &'a mut WalkBudget,
    max_depth: usize,
    complete: bool,
}

impl<'a> WalkState<'a> {
    fn new(budget: &'a mut WalkBudget, max_depth: usize) -> Self {
        Self {
            budget,
            max_depth,
            complete: true,
        }
    }

    /// Admit one node to the walk. Reaching a bound exactly is not itself
    /// truncation; completeness changes only when a node beyond that bound is
    /// actually encountered and skipped.
    fn enter(&mut self, depth: usize) -> bool {
        if depth > self.max_depth || !self.budget.admit() {
            self.complete = false;
            return false;
        }
        true
    }

    fn mark_incomplete(&mut self) {
        self.complete = false;
    }
}

/// Walk the UIA tree for the window with the given HWND.
pub fn walk_tree(hwnd: u64, query: Option<&str>) -> UiaTreeResult {
    walk_tree_bounded(hwnd, query, DEFAULT_MAX_TOTAL_ELEMENTS, DEFAULT_MAX_DEPTH)
}

fn is_menu_control(control_type: &str) -> bool {
    matches!(control_type, "Menu" | "MenuBar" | "MenuItem")
}

fn exact_menu_path_matches(nodes: &[UiaNode], path: &[String]) -> Vec<usize> {
    let by_element_index: std::collections::HashMap<usize, usize> = nodes
        .iter()
        .enumerate()
        .filter_map(|(node_index, node)| node.element_index.map(|index| (index, node_index)))
        .collect();

    nodes
        .iter()
        .enumerate()
        .filter_map(|(node_index, node)| {
            if !is_menu_control(&node.control_type)
                || node.enabled == Some(false)
                || node.name.as_deref().map(str::trim) != path.last().map(String::as_str)
            {
                return None;
            }
            let mut lineage = Vec::new();
            let mut cursor = Some(node_index);
            while let Some(current) = cursor {
                let ancestor = &nodes[current];
                if is_menu_control(&ancestor.control_type) {
                    if let Some(name) = ancestor.name.as_deref().map(str::trim) {
                        if !name.is_empty() {
                            lineage.push(name);
                        }
                    }
                }
                cursor = ancestor
                    .parent_element_index
                    .and_then(|index| by_element_index.get(&index).copied());
            }
            lineage.reverse();
            lineage.dedup();
            (lineage == path.iter().map(String::as_str).collect::<Vec<_>>()).then_some(node_index)
        })
        .collect()
}

unsafe fn release_walk_nodes(nodes: Vec<UiaNode>) {
    use windows::Win32::UI::Accessibility::IAccessible;
    for node in nodes {
        if node.element_ptr == 0 {
            continue;
        }
        if node.msaa_role.is_some() {
            drop(IAccessible::from_raw(node.element_ptr as *mut _));
        } else {
            drop(IUIAutomationElement::from_raw(node.element_ptr as *mut _));
        }
    }
}

#[derive(Debug)]
pub(crate) struct MenuPathError {
    pub message: String,
    pub completed_hops: usize,
    pub failure: crate::uia_mutation::MenuFailure,
    pub transport: cua_driver_core::action_record::ActionTransport,
}

unsafe fn menu_pattern<T: Interface>(
    element: &IUIAutomationElement,
    id: windows::Win32::UI::Accessibility::UIA_PATTERN_ID,
) -> Result<Option<T>, i32> {
    match element
        .GetCurrentPattern(id)
        .and_then(|pattern| pattern.cast::<T>())
    {
        Ok(pattern) => Ok(Some(pattern)),
        Err(error) if crate::uia_mutation::pattern_absent(error.code().0) => Ok(None),
        Err(error) => Err(error.code().0),
    }
}

unsafe fn invoke_menu_element(
    element_ptr: usize,
    final_segment: bool,
) -> Result<
    cua_driver_core::action_record::ActionTransport,
    (
        String,
        crate::uia_mutation::MenuFailure,
        cua_driver_core::action_record::ActionTransport,
        bool,
    ),
> {
    use crate::uia_mutation::{classify_write_hresult, MenuFailure, WriteVerdict};
    use cua_driver_core::action_record::ActionTransport;
    let element =
        std::mem::ManuallyDrop::new(IUIAutomationElement::from_raw(element_ptr as *mut _));
    macro_rules! attempt {
        ($ty:ty, $id:expr, $method:ident, $transport:expr) => {{
            let transport = $transport;
            match menu_pattern::<$ty>(&element, $id) {
                Err(hr) => {
                    return Err((
                        format!("pattern acquisition failed: HRESULT 0x{:08X}", hr as u32),
                        MenuFailure::Refused,
                        transport,
                        false,
                    ))
                }
                Ok(None) => {}
                Ok(Some(pattern)) => match pattern.$method() {
                    Ok(()) => return Ok(transport),
                    Err(error) => {
                        let hr = error.code().0;
                        match classify_write_hresult(hr) {
                            WriteVerdict::AffordanceAbsent => {}
                            WriteVerdict::Refused(_) => {
                                return Err((
                                    format!("{} failed: {error}", stringify!($method)),
                                    MenuFailure::Refused,
                                    transport,
                                    true,
                                ))
                            }
                            _ => {
                                return Err((
                                    format!("{} failed: {error}", stringify!($method)),
                                    MenuFailure::Uncertain,
                                    transport,
                                    true,
                                ))
                            }
                        }
                    }
                },
            }
        }};
    }
    if !final_segment {
        attempt!(
            IUIAutomationExpandCollapsePattern,
            UIA_ExpandCollapsePatternId,
            Expand,
            ActionTransport::WindowsUiaExpandCollapse
        );
    }
    attempt!(
        IUIAutomationInvokePattern,
        UIA_InvokePatternId,
        Invoke,
        ActionTransport::WindowsUiaInvoke
    );
    if final_segment {
        attempt!(
            IUIAutomationSelectionItemPattern,
            UIA_SelectionItemPatternId,
            Select,
            ActionTransport::WindowsUiaSelection
        );
    }
    Err((
        "element exposes no usable native menu pattern".into(),
        MenuFailure::Refused,
        ActionTransport::WindowsUiaInvoke,
        false,
    ))
}

/// Resolve and invoke an exact application menu path from fresh UIA state at every hop.
pub(crate) fn invoke_menu_path(hwnd: u64, path: &[String]) -> Result<(), MenuPathError> {
    use crate::uia_mutation::MenuFailure;
    use cua_driver_core::action_record::ActionTransport;
    let mut last_transport = ActionTransport::WindowsUiaInvoke;
    for depth in 0..path.len() {
        let result = walk_tree(hwnd, None);
        let matches = exact_menu_path_matches(&result.nodes, &path[..=depth]);
        let target_index = match matches.as_slice() {
            [index] => *index,
            _ => {
                let message = if matches.is_empty() {
                    format!("menu path segment {depth} was not found")
                } else {
                    format!("menu path segment {depth} is ambiguous")
                };
                unsafe { release_walk_nodes(result.nodes) };
                return Err(MenuPathError {
                    message,
                    completed_hops: depth,
                    failure: MenuFailure::Refused,
                    transport: last_transport,
                });
            }
        };
        let target = &result.nodes[target_index];
        let action = if target.enabled == Some(false) {
            Err((
                format!("menu path segment {depth} is disabled"),
                MenuFailure::Refused,
                last_transport,
                false,
            ))
        } else if target.msaa_role.is_some() {
            Err((
                format!(
                    "menu path segment {depth} is exposed only through MSAA, not UI Automation"
                ),
                MenuFailure::Refused,
                last_transport,
                false,
            ))
        } else {
            unsafe { invoke_menu_element(target.element_ptr, depth + 1 == path.len()) }
        };
        unsafe { release_walk_nodes(result.nodes) };
        match action {
            Ok(transport) => last_transport = transport,
            Err((message, failure, transport, called)) => {
                return Err(MenuPathError {
                    message: format!("menu path segment {depth}: {message}"),
                    completed_hops: depth,
                    failure,
                    transport: if called || depth == 0 {
                        transport
                    } else {
                        last_transport
                    },
                })
            }
        }
        if depth + 1 != path.len() {
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
    }
    Ok(())
}

/// Walk the UIA tree with caller-supplied caps. `max_elements`/`max_depth`
/// truncate the walk and the rendered markdown identically. Issue #22865:
/// caps protect against Electron / large web apps that produce 10k+
/// element trees and blow context windows.
pub fn walk_tree_bounded(
    hwnd: u64,
    query: Option<&str>,
    max_elements: usize,
    max_depth: usize,
) -> UiaTreeResult {
    let mut budget = WalkBudget::nodes_only(max_elements);
    walk_tree_budgeted(hwnd, query, max_depth, &mut budget)
}

/// [`walk_tree_bounded`] under a caller's [`WalkBudget`], checked per node of
/// the cached (and MSAA) walk. The bulk `BuildUpdatedCache` fetch before it is
/// setup the budget does not cover; the caller's backstop bounds it. Read the
/// outcome from `budget` afterwards.
pub fn walk_tree_budgeted(
    hwnd: u64,
    query: Option<&str>,
    max_depth: usize,
    budget: &mut WalkBudget,
) -> UiaTreeResult {
    unsafe { walk_tree_unsafe(hwnd, query, max_depth, budget) }
}

unsafe fn walk_tree_unsafe(
    hwnd: u64,
    query: Option<&str>,
    max_depth: usize,
    budget: &mut WalkBudget,
) -> UiaTreeResult {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

    let automation: IUIAutomation = match client::new_uia_client() {
        Ok(a) => a,
        Err(e) => {
            return UiaTreeResult {
                tree_markdown: format!("UIA init failed: {e}"),
                nodes: Vec::new(),
                elements_complete: false,
            }
        }
    };

    // Build a cache request that fetches everything we need in ONE bulk RPC.
    let (cache_req, mut cache_request_complete) = match node_cache_request(&automation) {
        Ok(r) => r,
        Err(e) => {
            return UiaTreeResult {
                tree_markdown: format!("CreateCacheRequest failed: {e}"),
                nodes: Vec::new(),
                elements_complete: false,
            }
        }
    };

    // Fetch entire subtree in one call.
    cache_request_complete &= cache_req.SetTreeScope(TreeScope_Subtree).is_ok();

    // Apply control-view filter (same as ControlViewWalker).
    cache_request_complete &= automation
        .ControlViewCondition()
        .and_then(|condition| cache_req.SetTreeFilter(&condition))
        .is_ok();

    let hwnd_win = windows::Win32::Foundation::HWND(hwnd as *mut _);

    // SAL (LibreOffice / OpenOffice) fallback: ALL SAL-class windows go
    // through the MSAA walker.
    //
    // Two reasons:
    //   1. **Hang avoidance** for SALSUBFRAME / SALMENU / SALTMPSUBFRAME —
    //      VCL's UIA provider hangs on `BuildUpdatedCache(TreeScope.Subtree)`
    //      under the daemon's MTA pool, wasting the timeout_ms walk budget.
    //   2. **Role fidelity** for SALFRAME (main document window, Recovery
    //      dialog) — UIA technically walks fine, but the built-in
    //      MSAA→UIA proxy collapses `ROLE_SYSTEM_BUTTONDROPDOWN` (0x38) to
    //      a featureless `SplitButton` with no separable dropdown
    //      affordance. MSAA via oleacc.dll preserves the role, letting
    //      the click tool route `action:"expand"` to a right-edge
    //      SendInput click that opens the dropdown half (e.g. LO Writer
    //      "Font Color" → SALTMPSUBFRAME color picker).
    //
    // The UIA pattern dispatches we lose for SALFRAME (Toggle / Select /
    // ExpandCollapse) only matter for WinUI3 controls, which VCL doesn't
    // host. Net win.
    let sal_class: bool = {
        use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hwnd_win, &mut buf);
        n > 0 && String::from_utf16_lossy(&buf[..n as usize]).starts_with("SAL")
    };
    if sal_class {
        return crate::msaa::walk_msaa_tree_budgeted(hwnd, budget);
    }

    let failure = match client::probe_root(hwnd_win) {
        crate::uia_root::RootVerdict::Ok => None,
        crate::uia_root::RootVerdict::WindowGone => Some("ElementFromHandle failed: window no longer exists".to_owned()),
        crate::uia_root::RootVerdict::NoAnswer => Some(format!("window 0x{hwnd:x} did not answer a no-op message within 1 s (the app may be hung); no accessibility tree was read")),
    };
    if let Some(tree_markdown) = failure {
        return UiaTreeResult {
            tree_markdown,
            nodes: Vec::new(),
            elements_complete: false,
        };
    }

    // A native ListView's bulk subtree cache materializes every row and
    // pattern before our walk budget can run. Large responsive lists can
    // therefore fail with UIA_E_TIMEOUT and lose unrelated sibling controls.
    if contains_native_list_view(hwnd_win) {
        return walk_list_view_window(&automation, hwnd_win, query, max_depth, budget);
    }

    // Two-call sequence (ElementFromHandle + BuildUpdatedCache) instead of
    // the atomic ElementFromHandleBuildCache. Empirically, the single-call
    // batched variant hangs against SAL even in CLI in-process tests;
    // the two-call path works in-process. Kept for non-SAL targets where
    // the difference matters for performance-sensitive providers.
    let uncached = match automation.ElementFromHandle(hwnd_win) {
        Ok(e) => e,
        Err(e) => {
            return UiaTreeResult {
                tree_markdown: format!("ElementFromHandle failed: {e}"),
                nodes: Vec::new(),
                elements_complete: false,
            }
        }
    };
    // A single transient provider error (commonly E_FAIL / 0x80004005 from a
    // control rebuilding its automation subtree mid-walk) must not take down
    // the whole snapshot — the same call usually succeeds a beat later. Retry
    // the bulk cache build a few times with a short backoff before giving up,
    // so one misbehaving provider doesn't force the agent down to pixel mode.
    // See #1881. (A per-node partial-tree fallback — returning elements=N for
    // the subtrees that did resolve — remains a larger follow-up.)
    let root_elem = {
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt = 0u32;
        loop {
            match uncached.BuildUpdatedCache(&cache_req) {
                Ok(e) => break e,
                Err(e) => {
                    attempt += 1;
                    if attempt >= MAX_ATTEMPTS {
                        return UiaTreeResult {
                            tree_markdown: format!(
                                "BuildUpdatedCache failed after {attempt} attempts: {e}"
                            ),
                            nodes: Vec::new(),
                            elements_complete: false,
                        };
                    }
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
            }
        }
    };

    let mut nodes: Vec<UiaNode> = Vec::new();
    let mut lines: Vec<(usize, String)> = Vec::new();
    let mut counter = 0usize;
    let mut walk_state = WalkState::new(budget, max_depth);
    if !cache_request_complete {
        walk_state.mark_incomplete();
    }
    let search_offscreen = query.is_some_and(|q| !q.trim().is_empty());

    walk_cached_bounded_with_priority(
        &root_elem,
        0,
        None,
        false,
        &mut nodes,
        &mut lines,
        &mut counter,
        &mut walk_state,
        search_offscreen,
    );

    // Fallback for CoreWindow-class apps (Calculator, Settings, older UWPs).
    // `ElementFromHandle(hwnd)` on a `Windows.UI.Core.CoreWindow` HWND returns
    // an empty wrapper — the actual XAML tree is registered at the desktop
    // root as a separate UIA element with the same ProcessId, not as a child
    // of the hwnd's UIA element. inspect.exe walks from root for these apps;
    // we mirror that here when the primary path yields nothing actionable.
    //
    // Trigger: walk produced zero actionable nodes (the primary path may have
    // still pushed a wrapper-only node — that's why we filter on
    // `element_index.is_some()`).
    //
    // Stage the fallback walk into fresh accumulators and only swap them in
    // if the fallback actually finds actionable elements. Otherwise the
    // wrapper-only node from the primary walk stays the result. Both paths
    // share the caller's time and node budget.
    if nodes.iter().filter(|n| n.element_index.is_some()).count() == 0 {
        // An empty HWND wrapper cannot prove a complete window snapshot. The
        // fallback may discover controls in other windows of the process, so
        // it remains discovery evidence rather than an exact-window proof.
        walk_state.mark_incomplete();
        // SAL windows already returned through the MSAA path above.
        if !walk_state.budget.expired() {
            if let Some(target_pid) = pid_from_hwnd(hwnd_win) {
                let mut fallback_nodes: Vec<UiaNode> = Vec::new();
                let mut fallback_lines: Vec<(usize, String)> = Vec::new();
                let mut fallback_counter = 0usize;

                tracing::debug!(
                    target: "uia",
                    "ElementFromHandle returned empty tree for hwnd 0x{hwnd:x}; \
                     falling back to GetRootElement + filter ProcessId={target_pid}"
                );
                walk_root_by_pid(
                    &automation,
                    &cache_req,
                    target_pid,
                    &mut fallback_nodes,
                    &mut fallback_lines,
                    &mut fallback_counter,
                    &mut walk_state,
                    search_offscreen,
                );

                if fallback_nodes.iter().any(|n| n.element_index.is_some()) {
                    nodes = fallback_nodes;
                    lines = fallback_lines;
                }
            }
        }
    }

    fold_actionless_text_subtrees(&mut lines, &mut nodes);
    nodes.retain(|n| n.control_type != "UiaTabStripSummary");
    let raw_md = render_lines(&lines);
    let tree_markdown = if let Some(q) = query {
        filter_tree(&raw_md, q)
    } else {
        raw_md
    };

    UiaTreeResult {
        tree_markdown,
        nodes,
        elements_complete: walk_state.complete,
    }
}

/// Resolve the owning process id of `hwnd` via `GetWindowThreadProcessId`.
/// Returns `None` if the API fails or the HWND is invalid.
unsafe fn pid_from_hwnd(hwnd: windows::Win32::Foundation::HWND) -> Option<u32> {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let mut pid: u32 = 0;
    let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if tid == 0 || pid == 0 {
        None
    } else {
        Some(pid)
    }
}

fn native_list_view_class(class: &str) -> bool {
    class.eq_ignore_ascii_case("SysListView32")
        || class
            .to_ascii_lowercase()
            .starts_with("windowsforms10.syslistview32.")
}

unsafe fn contains_native_list_view(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW};
    unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> BOOL {
        let mut class = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut class);
        if len > 0 && native_list_view_class(&String::from_utf16_lossy(&class[..len as usize])) {
            *(param.0 as *mut bool) = true;
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut found = false;
    let param = LPARAM(&mut found as *mut bool as isize);
    let _ = visit(hwnd, param);
    if !found {
        let _ = EnumChildWindows(hwnd, Some(visit), param);
    }
    found
}

unsafe fn walk_list_view_window(
    automation: &IUIAutomation,
    hwnd: windows::Win32::Foundation::HWND,
    query: Option<&str>,
    max_depth: usize,
    budget: &mut WalkBudget,
) -> UiaTreeResult {
    let mut nodes = Vec::new();
    let mut lines = Vec::new();
    let mut counter = 0;
    let mut state = WalkState::new(budget, max_depth);
    let result = (|| -> windows::core::Result<()> {
        let (cache, complete) = node_cache_request(automation)?;
        cache.SetTreeScope(TreeScope_Element)?;
        cache.SetTreeFilter(&automation.ControlViewCondition()?)?;
        if !complete {
            state.mark_incomplete();
        }
        let root = automation
            .ElementFromHandle(hwnd)?
            .BuildUpdatedCache(&cache)?;
        walk_list_view_level(
            automation,
            &cache,
            &root,
            0,
            None,
            false,
            &mut nodes,
            &mut lines,
            &mut counter,
            &mut state,
            query.is_some_and(|q| !q.trim().is_empty()),
        );
        Ok(())
    })();
    if let Err(error) = result {
        state.mark_incomplete();
        lines.push((0, format!("UIA shallow cache failed: {error}")));
    }
    fold_actionless_text_subtrees(&mut lines, &mut nodes);
    let raw = render_lines(&lines);
    UiaTreeResult {
        tree_markdown: query.map_or_else(|| raw.clone(), |q| filter_tree(&raw, q)),
        nodes,
        elements_complete: state.complete,
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn walk_list_view_level(
    automation: &IUIAutomation,
    cache: &IUIAutomationCacheRequest,
    element: &IUIAutomationElement,
    depth: usize,
    parent: Option<usize>,
    in_web: bool,
    nodes: &mut Vec<UiaNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
    state: &mut WalkState<'_>,
    search_offscreen: bool,
) {
    if !state.enter(depth) {
        return;
    }
    let (kind, parent) = emit_cached_node(element, depth, parent, in_web, nodes, lines, counter);
    let next_web = in_web || kind.eq_ignore_ascii_case("Document");
    let dense = is_uia_dense_collection(&kind);
    let native_children = dense
        .then(|| native_visible_list_rows(automation, cache, element))
        .flatten();
    let children = native_children.map(Ok).unwrap_or_else(|| {
        (|| -> windows::core::Result<Vec<IUIAutomationElement>> {
            let control = automation.ControlViewCondition()?;
            let condition = if dense {
                // Filter before fetching patterns/values: offscreen rows must
                // never enter the expensive bulk cache transaction.
                let visible = automation.CreatePropertyCondition(
                    UIA_IsOffscreenPropertyId,
                    &windows::core::VARIANT::from(false),
                )?;
                state.mark_incomplete();
                automation.CreateAndCondition(&control, &visible)?
            } else {
                control
            };
            let children = element.FindAllBuildCache(TreeScope_Children, &condition, cache)?;
            (0..children.Length()?)
                .map(|i| children.GetElement(i))
                .collect()
        })()
    });
    let children = match children {
        Ok(children) => children,
        Err(error) => {
            state.mark_incomplete();
            lines.push((depth + 1, format!("(UIA children unavailable: {error})")));
            return;
        }
    };
    let mut pending = children;
    // Visit sibling controls (including editors below a list) before rows
    // consume the remaining budget. Stable ordering within each group.
    pending.sort_by_key(|child| is_uia_dense_collection(&read_cached_control_type(child)));
    for child in pending {
        walk_list_view_level(
            automation,
            cache,
            &child,
            depth + 1,
            parent,
            next_web,
            nodes,
            lines,
            counter,
            state,
            search_offscreen,
        );
    }
    if dense {
        state.mark_incomplete();
        let search = search_offscreen && !state.budget.expired();
        if !search {
            lines.push((depth + 1, format_uncounted_offscreen_rows_line()));
        }
        if search {
            // Query mode may inspect offscreen rows too, but only one row's
            // cache at a time, after visible rows and sibling editors.
            let walker = (|| -> windows::core::Result<_> {
                let offscreen = automation.CreatePropertyCondition(
                    UIA_IsOffscreenPropertyId,
                    &windows::core::VARIANT::from(true),
                )?;
                automation.CreateTreeWalker(
                    &automation
                        .CreateAndCondition(&automation.ControlViewCondition()?, &offscreen)?,
                )
            })();
            if let Ok(walker) = walker {
                let mut row = walker.GetFirstChildElementBuildCache(element, cache);
                while let Ok(child) = row {
                    let outcome = state.budget.outcome();
                    if state.budget.expired() || outcome.stop.is_some() {
                        break;
                    }
                    walk_list_view_level(
                        automation,
                        cache,
                        &child,
                        depth + 1,
                        parent,
                        next_web,
                        nodes,
                        lines,
                        counter,
                        state,
                        false,
                    );
                    row = walker.GetNextSiblingElementBuildCache(&child, cache);
                }
            }
        }
    }
}

// Native report-view child IDs are one-based item indices. Resolve only the
// viewport through the MSAA/UIA bridge instead of asking UIA to test the
// IsOffscreen property of thousands of rows before returning any of them.
unsafe fn native_visible_list_rows(
    automation: &IUIAutomation,
    cache: &IUIAutomationCacheRequest,
    element: &IUIAutomationElement,
) -> Option<Vec<IUIAutomationElement>> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::Accessibility::{AccessibleObjectFromWindow, IAccessible};
    use windows::Win32::UI::Controls::{LVM_GETCOUNTPERPAGE, LVM_GETITEMCOUNT, LVM_GETTOPINDEX};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowLongPtrW, SendMessageTimeoutW, GWL_STYLE, SMTO_ABORTIFHUNG,
        SMTO_BLOCK,
    };
    let hwnd = element.CurrentNativeWindowHandle().ok()?;
    let mut class = [0u16; 256];
    let len = GetClassNameW(hwnd, &mut class);
    if len <= 0
        || !native_list_view_class(&String::from_utf16_lossy(&class[..len as usize]))
        || GetWindowLongPtrW(hwnd, GWL_STYLE) & 3 != 1
    // LVS_TYPEMASK / LVS_REPORT
    {
        return None;
    }
    let read = |message| {
        let mut value = 0usize;
        (SendMessageTimeoutW(
            hwnd,
            message,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            100,
            Some(&mut value),
        )
        .0 != 0)
            .then_some(value)
    };
    let count = read(LVM_GETITEMCOUNT)?;
    let top = read(LVM_GETTOPINDEX)?;
    let page = read(LVM_GETCOUNTPERPAGE)?;
    if count == 0 || top >= count || page == 0 || page > DEFAULT_MAX_TOTAL_ELEMENTS {
        return None;
    }
    let mut ptr = std::ptr::null_mut();
    AccessibleObjectFromWindow(hwnd, 0xFFFF_FFFC, &IAccessible::IID, &mut ptr).ok()?;
    if ptr.is_null() {
        return None;
    }
    let accessible = IAccessible::from_raw(ptr);
    let walker = automation.ControlViewWalker().ok()?;
    let mut children = Vec::new();
    let mut child = walker.GetFirstChildElementBuildCache(element, cache);
    // Header/scrollbar siblings precede or follow the contiguous row block.
    // Never traverse the row block to reach the opposite end.
    for _ in 0..16 {
        let current = child.ok()?;
        if read_cached_control_type(&current) == "ListItem" {
            break;
        }
        child = walker.GetNextSiblingElementBuildCache(&current, cache);
        children.push(current);
    }
    for index in top..count.min(top.saturating_add(page).saturating_add(1)) {
        children.push(
            automation
                .ElementFromIAccessibleBuildCache(
                    &accessible,
                    i32::try_from(index + 1).ok()?,
                    cache,
                )
                .ok()?,
        );
    }
    let mut tail = Vec::new();
    let mut child = walker.GetLastChildElementBuildCache(element, cache);
    for _ in 0..16 {
        let current = child.ok()?;
        if read_cached_control_type(&current) == "ListItem" {
            break;
        }
        child = walker.GetPreviousSiblingElementBuildCache(&current, cache);
        tail.push(current);
    }
    children.extend(tail.into_iter().rev());
    Some(children)
}

/// Root-walk UIA fallback for apps whose top-level window is a
/// `Windows.UI.Core.CoreWindow` (Calculator, Settings, older UWPs).
///
/// `ElementFromHandle(CoreWindow_hwnd)` returns an empty wrapper for these —
/// the actual XAML tree is registered at the desktop root as a sibling
/// element with the same `ProcessId`. We enumerate the root's children
/// (filtered by `ProcessId == target_pid`) and walk descendants from there,
/// reusing the caller's `cache_req` so the same properties + patterns get
/// pre-fetched as the primary path.
#[allow(clippy::too_many_arguments)]
unsafe fn walk_root_by_pid(
    automation: &IUIAutomation,
    cache_req: &IUIAutomationCacheRequest,
    target_pid: u32,
    nodes: &mut Vec<UiaNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
    walk_state: &mut WalkState<'_>,
    search_offscreen: bool,
) {
    let root = match automation.GetRootElement() {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(target: "uia", "GetRootElement failed: {e}");
            walk_state.mark_incomplete();
            return;
        }
    };
    let true_cond = match automation.CreateTrueCondition() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(target: "uia", "CreateTrueCondition failed: {e}");
            walk_state.mark_incomplete();
            return;
        }
    };
    let kids = match root.FindAll(TreeScope_Children, &true_cond) {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(target: "uia", "root.FindAll(Children) failed: {e}");
            walk_state.mark_incomplete();
            return;
        }
    };
    let count = match kids.Length() {
        Ok(count) => count,
        Err(e) => {
            tracing::debug!(target: "uia", "root child count failed: {e}");
            walk_state.mark_incomplete();
            return;
        }
    };
    for i in 0..count {
        let elem = match kids.GetElement(i) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(target: "uia", "root child {i} fetch failed: {e}");
                walk_state.mark_incomplete();
                continue;
            }
        };
        // Read ProcessId without a cache — root.FindAll didn't use one.
        // VARIANT for VT_I4 (UIA's ProcessId type) puts the int at
        // Anonymous.Anonymous.Anonymous.lVal; mirrors the read_cached_bool
        // pattern below for VT_BOOL.
        let pid: u32 = match elem.GetCurrentPropertyValue(UIA_ProcessIdPropertyId) {
            Ok(v) => {
                let raw = v.as_raw();
                if raw.Anonymous.Anonymous.vt != 3
                /* VT_I4 */
                {
                    walk_state.mark_incomplete();
                    continue;
                }
                raw.Anonymous.Anonymous.Anonymous.lVal as u32
            }
            Err(e) => {
                tracing::debug!(target: "uia", "root child {i} ProcessId read failed: {e}");
                walk_state.mark_incomplete();
                continue;
            }
        };
        if pid != target_pid {
            continue;
        }
        // Match — pull a cached subtree from this element using the same
        // cache_req shape as the primary path so walk_cached_bounded sees the same
        // properties + patterns.
        let cached = match elem.BuildUpdatedCache(cache_req) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(target: "uia", "BuildUpdatedCache on pid={target_pid} match failed: {e}");
                walk_state.mark_incomplete();
                continue;
            }
        };
        walk_cached_bounded_with_priority(
            &cached,
            0,
            None,
            false,
            nodes,
            lines,
            counter,
            walk_state,
            search_offscreen,
        );
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn walk_cached_bounded(
    element: &IUIAutomationElement,
    depth: usize,
    parent_index: Option<usize>,
    in_web_content: bool,
    nodes: &mut Vec<UiaNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
    walk_state: &mut WalkState<'_>,
) {
    walk_cached_bounded_with_priority(
        element,
        depth,
        parent_index,
        in_web_content,
        nodes,
        lines,
        counter,
        walk_state,
        false,
    );
}

struct DeferredUiaRow {
    element: IUIAutomationElement,
    in_web_content: bool,
    chunk: DeferredChunk<UiaNode>,
}

/// Offscreen deferred rows are walked only when a query searches them;
/// otherwise they stay a summary row, the element list misses them, and the
/// walk is not complete.
fn note_unwalked_offscreen_rows(
    walk_state: &mut WalkState<'_>,
    visible: impl IntoIterator<Item = bool>,
    search_offscreen: bool,
) {
    if !search_offscreen && visible.into_iter().any(|visible| !visible) {
        walk_state.mark_incomplete();
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn walk_cached_bounded_with_priority(
    element: &IUIAutomationElement,
    depth: usize,
    parent_index: Option<usize>,
    in_web_content: bool,
    nodes: &mut Vec<UiaNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
    walk_state: &mut WalkState<'_>,
    search_offscreen: bool,
) {
    let mut deferred: Vec<DeferredUiaRow> = Vec::new();
    walk_cached_element(
        element,
        depth,
        parent_index,
        in_web_content,
        nodes,
        lines,
        counter,
        walk_state,
        true,
        &mut deferred,
    );
    if !deferred.is_empty() {
        note_unwalked_offscreen_rows(
            walk_state,
            deferred.iter().map(|row| row.chunk.visible),
            search_offscreen,
        );
        for visible in [true, false] {
            if !visible && !search_offscreen {
                continue;
            }
            for row in deferred.iter_mut().filter(|r| r.chunk.visible == visible) {
                let mut chunk_nodes = Vec::new();
                let mut chunk_lines = Vec::new();
                walk_cached_element(
                    &row.element,
                    row.chunk.depth,
                    row.chunk.parent_index,
                    row.in_web_content,
                    &mut chunk_nodes,
                    &mut chunk_lines,
                    counter,
                    walk_state,
                    false,
                    &mut Vec::new(),
                );
                row.chunk.walked = Some((chunk_lines, chunk_nodes));
            }
        }
        let chunks: Vec<DeferredChunk<UiaNode>> = deferred.into_iter().map(|r| r.chunk).collect();
        let old_lines = std::mem::take(lines);
        let old_nodes = std::mem::take(nodes);
        let (mut spliced_lines, mut spliced_nodes, map) =
            splice_deferred_chunks(old_lines, old_nodes, chunks);
        renumber_indexed_tree(&mut spliced_lines, &mut spliced_nodes, &map);
        *lines = spliced_lines;
        *nodes = spliced_nodes;
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn walk_cached_element(
    element: &IUIAutomationElement,
    depth: usize,
    parent_index: Option<usize>,
    in_web_content: bool,
    nodes: &mut Vec<UiaNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
    walk_state: &mut WalkState<'_>,
    defer_dense_rows: bool,
    deferred: &mut Vec<DeferredUiaRow>,
) {
    // Node and time budget: a refused node is counted as discovered but not
    // visited, and the walk unwinds with the partial tree it has.
    if !walk_state.enter(depth) {
        return;
    }

    let (control_type, emitted_parent) = emit_cached_node(
        element,
        depth,
        parent_index,
        in_web_content,
        nodes,
        lines,
        counter,
    );
    let next_web = in_web_content || control_type.eq_ignore_ascii_case("Document");

    let children = match cached_children(element) {
        Ok(Some(children)) => children,
        Ok(None) => return,
        Err(_) => {
            walk_state.mark_incomplete();
            return;
        }
    };
    let len = match children.Length() {
        Ok(len) => len,
        Err(_) => {
            walk_state.mark_incomplete();
            return;
        }
    };
    let mut child_elems: Vec<IUIAutomationElement> = Vec::with_capacity(len as usize);
    for i in 0..len {
        match children.GetElement(i) {
            Ok(child) => child_elems.push(child),
            Err(_) => walk_state.mark_incomplete(),
        }
    }
    let tab_plan = if !next_web && child_elems.len() >= TAB_STRIP_COMPACTION_THRESHOLD {
        let child_types: Vec<String> = child_elems.iter().map(read_cached_control_type).collect();
        let child_selected: Vec<Option<bool>> =
            child_elems.iter().map(read_cached_selected).collect();
        let child_values: Vec<Option<String>> =
            child_elems.iter().map(read_cached_bstr_value).collect();
        let metas: Vec<TabChildMeta<'_>> = (0..child_elems.len())
            .map(|i| TabChildMeta {
                role: &child_types[i],
                role_description: None,
                title: None,
                selected: child_selected[i],
                value: child_values[i].as_deref(),
                value_state: child_values[i].as_deref(),
            })
            .collect();
        let parent_name = read_cached_bstr_name(element);
        compact_tab_strip(&control_type, parent_name.as_deref(), None, None, &metas)
    } else {
        None
    };

    let dense_plan = if defer_dense_rows && !next_web && is_uia_dense_collection(&control_type) {
        let (_, _, parent_rect) = read_cached_bounding_rect_full(element);
        let child_types: Vec<String> = child_elems.iter().map(read_cached_control_type).collect();
        let candidates: Vec<UiaChildCandidate<'_>> = child_elems
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let is_offscreen = read_cached_bool(c, UIA_IsOffscreenPropertyId);
                let (_, _, rect) = read_cached_bounding_rect_full(c);
                UiaChildCandidate {
                    control_type: &child_types[i],
                    is_offscreen,
                    rect,
                }
            })
            .collect();
        Some(plan_uia_collection_children(parent_rect, &candidates))
    } else {
        None
    };

    for (index, child) in child_elems.into_iter().enumerate() {
        if let Some(ref tp) = tab_plan {
            if index == tp.first_omitted_index {
                lines.push((depth + 1, format_omitted_tabs_line(tp.omitted_count)));
                nodes.push(UiaNode {
                    element_index: None,
                    control_type: "UiaTabStripSummary".into(),
                    name: None,
                    value: None,
                    automation_id: None,
                    help_text: None,
                    actions: vec![],
                    enabled: None,
                    selected: None,
                    checked: None,
                    expanded: None,
                    element_ptr: 0,
                    center_x: 0,
                    center_y: 0,
                    rect: None,
                    msaa_role: None,
                    depth: depth + 1,
                    parent_element_index: emitted_parent,
                    in_web_content: next_web,
                });
            }
            if !tp.keep[index] {
                continue;
            }
        }
        let action = dense_plan
            .as_ref()
            .map_or(UiaChildPlan::WalkNow, |plan| plan[index]);
        match action {
            UiaChildPlan::Defer { visible } => deferred.push(DeferredUiaRow {
                element: child,
                in_web_content: next_web,
                chunk: DeferredChunk {
                    anchor: nodes.len(),
                    seq: deferred.len(),
                    depth: depth + 1,
                    parent_index: emitted_parent,
                    visible,
                    walked: None,
                },
            }),
            UiaChildPlan::WalkNow => walk_cached_element(
                &child,
                depth + 1,
                emitted_parent,
                next_web,
                nodes,
                lines,
                counter,
                walk_state,
                defer_dense_rows,
                deferred,
            ),
        }
    }
}

unsafe fn cached_children(
    element: &IUIAutomationElement,
) -> windows::core::Result<Option<IUIAutomationElementArray>> {
    let mut raw = std::ptr::null_mut();
    let result =
        (Interface::vtable(element).GetCachedChildren)(Interface::as_raw(element), &mut raw);
    result.ok()?;
    if raw.is_null() {
        Ok(None)
    } else {
        Ok(Some(IUIAutomationElementArray::from_raw(raw)))
    }
}

/// Cache request carrying every property and pattern the node projection
/// reads. Callers choose the tree scope and filter.
unsafe fn node_cache_request(
    automation: &IUIAutomation,
) -> windows::core::Result<(IUIAutomationCacheRequest, bool)> {
    let cache_req = automation.CreateCacheRequest()?;
    let mut complete = true;
    for prop in &[
        UIA_ControlTypePropertyId,
        UIA_NamePropertyId,
        UIA_ValueValuePropertyId,
        UIA_AutomationIdPropertyId,
        UIA_HelpTextPropertyId,
        UIA_FullDescriptionPropertyId,
        UIA_LegacyIAccessibleDescriptionPropertyId,
        UIA_IsEnabledPropertyId,
        UIA_IsOffscreenPropertyId,
        UIA_IsPasswordPropertyId,
        UIA_BoundingRectanglePropertyId,
        UIA_ToggleToggleStatePropertyId,
        UIA_SelectionItemIsSelectedPropertyId,
        UIA_ExpandCollapseExpandCollapseStatePropertyId,
    ] {
        complete &= cache_req.AddProperty(*prop).is_ok();
    }
    for pat in &[
        UIA_InvokePatternId,
        UIA_TogglePatternId,
        UIA_SelectionItemPatternId,
        UIA_ExpandCollapsePatternId,
        UIA_ValuePatternId,
        UIA_RangeValuePatternId,
        UIA_TextPatternId,
        UIA_ScrollPatternId,
    ] {
        complete &= cache_req.AddPattern(*pat).is_ok();
    }
    Ok((cache_req, complete))
}

/// Walk only a window's native (non-renderer) accessibility tree.
///
/// [`walk_tree`] fetches the whole subtree in one `BuildUpdatedCache` call,
/// which includes the active tab's web document. When a Chromium renderer
/// stops answering UIA, that one call times out and the walk returns no
/// nodes at all, even though the browser's own tab strip, toolbar and
/// omnibox are still live (#4121: Edge 153 after a CDP session, about 10 s
/// per walk). This walker fetches one level per call and never descends
/// below a `Document`, so browser chrome stays observable. The `Document`
/// node itself is still reported, so callers can prove which page is
/// committed without trusting any renderer-owned descendant.
///
/// Returns `Err` when any native level cannot be read. A partial native tree
/// would misstate tab topology, so callers must treat it as unavailable.
pub fn walk_native_tree(hwnd: u64) -> Result<Vec<UiaNode>, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let automation = client::new_uia_client().map_err(|e| format!("UIA init failed: {e}"))?;
        let (cache_req, _) = node_cache_request(&automation)
            .map_err(|e| format!("CreateCacheRequest failed: {e}"))?;
        let _ = cache_req.SetTreeScope(TreeScope(TreeScope_Element.0 | TreeScope_Children.0));
        if let Ok(ctrl_cond) = automation.ControlViewCondition() {
            let _ = cache_req.SetTreeFilter(&ctrl_cond);
        }
        let hwnd_win = windows::Win32::Foundation::HWND(hwnd as *mut _);
        match client::probe_root(hwnd_win) {
            crate::uia_root::RootVerdict::Ok => {},
            crate::uia_root::RootVerdict::WindowGone => return Err("native UIA root could not be read: window no longer exists".to_owned()),
            crate::uia_root::RootVerdict::NoAnswer => return Err(format!("window 0x{hwnd:x} did not answer a no-op message within 1 s (the app may be hung); no accessibility tree was read")),
        }
        let root = automation
            .ElementFromHandle(hwnd_win)
            .and_then(|element| element.BuildUpdatedCache(&cache_req))
            .map_err(|e| format!("native UIA root could not be read: {e}"))?;
        let mut nodes = Vec::new();
        let mut lines = Vec::new();
        let mut counter = 0usize;
        let mut budget = WalkBudget::nodes_only(DEFAULT_MAX_TOTAL_ELEMENTS);
        let walked = walk_native_level(
            &root,
            &cache_req,
            0,
            None,
            &mut nodes,
            &mut lines,
            &mut counter,
            &mut budget,
        );
        match walked {
            Ok(()) => Ok(nodes),
            Err(error) => {
                release_walk_nodes(nodes);
                Err(error)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn walk_native_level(
    element: &IUIAutomationElement,
    cache_req: &IUIAutomationCacheRequest,
    depth: usize,
    parent_index: Option<usize>,
    nodes: &mut Vec<UiaNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
    budget: &mut WalkBudget,
) -> Result<(), String> {
    if depth > DEFAULT_MAX_DEPTH {
        return Ok(());
    }
    if !budget.admit() {
        return Err("the native UIA tree exceeded the element budget".to_owned());
    }
    let (control_type, emitted_parent) =
        emit_cached_node(element, depth, parent_index, false, nodes, lines, counter);
    if control_type.eq_ignore_ascii_case("Document") {
        return Ok(());
    }
    let Ok(children) = element.GetCachedChildren() else {
        return Ok(());
    };
    for i in 0..children.Length().unwrap_or(0) {
        let child = children
            .GetElement(i)
            .and_then(|child| child.BuildUpdatedCache(cache_req))
            .map_err(|e| format!("a native UIA level could not be read: {e}"))?;
        walk_native_level(
            &child,
            cache_req,
            depth + 1,
            emitted_parent,
            nodes,
            lines,
            counter,
            budget,
        )?;
    }
    Ok(())
}

/// Project one cached element into `nodes`/`lines`. Returns its control type
/// and the parent index its descendants should inherit.
fn emit_cached_node(
    element: &IUIAutomationElement,
    depth: usize,
    parent_index: Option<usize>,
    in_web_content: bool,
    nodes: &mut Vec<UiaNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
) -> (String, Option<usize>) {
    let control_type = read_cached_control_type(element);
    let name = read_cached_bstr_name(element);
    let value = read_cached_bstr_value(element);
    let automation_id = read_cached_bstr(element, UIA_AutomationIdPropertyId);
    let help_text = read_cached_bstr(element, UIA_HelpTextPropertyId);
    let is_offscreen = read_cached_bool(element, UIA_IsOffscreenPropertyId);
    // WinForms `AccessibleDescription` reaches UIA only as the MSAA
    // description (LegacyIAccessible.Description); UIA providers use
    // FullDescription. Either can be the only hint that a field holds an OTP.
    let description = [
        read_cached_bstr(element, UIA_FullDescriptionPropertyId),
        read_cached_bstr(element, UIA_LegacyIAccessibleDescriptionPropertyId),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let description = (!description.is_empty()).then(|| description.join(" "));
    let ProjectedNodeText {
        name,
        value,
        help: help_text,
        secure: _secure,
    } = project_node_text(RawNodeText {
        control_type: &control_type,
        name,
        value,
        help: help_text,
        description,
        automation_id: automation_id.clone(),
        is_password: read_cached_bool(element, UIA_IsPasswordPropertyId),
        has_value_pattern: unsafe { element.GetCachedPattern(UIA_ValuePatternId) }.is_ok(),
    });
    let enabled = read_cached_bool(element, UIA_IsEnabledPropertyId);
    // Missing UIA state must remain unknown on the structured observation
    // surface. Action discovery keeps its historical best-effort assumption.
    let is_enabled = enabled.unwrap_or(true);
    let selected = read_cached_selected(element);
    let checked = read_cached_checked(element);
    let expanded = read_cached_expanded(element);
    let actions = detect_cached_actions(element, &control_type, is_enabled);
    let is_actionable = !actions.is_empty() && is_enabled;
    if is_offscreen == Some(true) && !is_uia_offscreen_exempt(&control_type) && !is_actionable {
        return (control_type, parent_index);
    }
    let has_content = name
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
        || value
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);

    let mut emitted_parent: Option<usize> = parent_index;
    if is_actionable || has_content {
        let node = if is_actionable {
            let retained: IUIAutomationElement = element.clone();
            let ptr = retained.as_raw() as usize;
            std::mem::forget(retained);
            let idx = *counter;
            *counter += 1;
            let (center_x, center_y, rect) = read_cached_bounding_rect_full(element);
            emitted_parent = Some(idx);
            UiaNode {
                element_index: Some(idx),
                control_type: control_type.clone(),
                name: name.clone(),
                value: value.clone(),
                automation_id: automation_id.clone(),
                help_text: help_text.clone(),
                actions: actions.clone(),
                enabled,
                selected,
                checked,
                expanded,
                element_ptr: ptr,
                center_x,
                center_y,
                rect,
                msaa_role: None,
                depth,
                parent_element_index: parent_index,
                in_web_content,
            }
        } else {
            UiaNode {
                element_index: None,
                control_type: control_type.clone(),
                name: name.clone(),
                value: value.clone(),
                automation_id: automation_id.clone(),
                help_text: help_text.clone(),
                actions: vec![],
                enabled,
                selected,
                checked,
                expanded,
                element_ptr: 0,
                center_x: 0,
                center_y: 0,
                rect: None,
                msaa_role: None,
                depth,
                parent_element_index: parent_index,
                in_web_content,
            }
        };

        lines.push((depth, format_node_line(&node)));
        nodes.push(node);
    }
    (control_type, emitted_parent)
}

fn read_cached_control_type(element: &IUIAutomationElement) -> String {
    unsafe {
        element
            .CachedControlType()
            .ok()
            .map(|ct| control_type_name(ct.0))
            .unwrap_or_else(|| "Unknown".into())
    }
}

fn read_cached_bstr_name(element: &IUIAutomationElement) -> Option<String> {
    unsafe {
        let bstr = element.CachedName().ok()?;
        let s = bstr.to_string();
        if s.trim().is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

fn read_cached_bstr_value(element: &IUIAutomationElement) -> Option<String> {
    read_cached_bstr(element, UIA_ValueValuePropertyId)
}

fn read_cached_bstr(
    element: &IUIAutomationElement,
    property_id: windows::Win32::UI::Accessibility::UIA_PROPERTY_ID,
) -> Option<String> {
    unsafe {
        let variant = element.GetCachedPropertyValue(property_id).ok()?;
        if variant.as_raw().Anonymous.Anonymous.vt == 8 {
            let bstr = BSTR::from_raw(variant.as_raw().Anonymous.Anonymous.Anonymous.bstrVal);
            let s = bstr.to_string();
            std::mem::forget(bstr);
            if s.trim().is_empty() {
                None
            } else {
                Some(s)
            }
        } else {
            None
        }
    }
}

fn read_cached_bool(
    element: &IUIAutomationElement,
    property_id: windows::Win32::UI::Accessibility::UIA_PROPERTY_ID,
) -> Option<bool> {
    unsafe {
        let variant = element.GetCachedPropertyValue(property_id).ok()?;
        if variant.as_raw().Anonymous.Anonymous.vt == 11 {
            Some(variant.as_raw().Anonymous.Anonymous.Anonymous.boolVal != 0)
        } else {
            None
        }
    }
}

/// Read bounding rect as (center_x, center_y, Some((l,t,r,b))). Returns
/// rect=None when the element has no meaningful BoundingRectangle (offscreen
/// containers, structure-only elements).
fn read_cached_bounding_rect_full(
    element: &IUIAutomationElement,
) -> (i32, i32, Option<(i32, i32, i32, i32)>) {
    unsafe {
        match element.CachedBoundingRectangle() {
            Ok(r) if r.right > r.left && r.bottom > r.top => (
                (r.left + r.right) / 2,
                (r.top + r.bottom) / 2,
                Some((r.left, r.top, r.right, r.bottom)),
            ),
            _ => (0, 0, None),
        }
    }
}

fn detect_cached_actions(
    element: &IUIAutomationElement,
    control_type: &str,
    is_enabled: bool,
) -> Vec<String> {
    if !is_enabled {
        return vec![];
    }
    let mut actions = Vec::new();
    unsafe {
        if element.GetCachedPattern(UIA_InvokePatternId).is_ok() {
            actions.push("invoke".into());
        }
        if element.GetCachedPattern(UIA_TogglePatternId).is_ok() {
            actions.push("toggle".into());
        }
        if element.GetCachedPattern(UIA_SelectionItemPatternId).is_ok() {
            actions.push("select".into());
        }
        if element
            .GetCachedPattern(UIA_ExpandCollapsePatternId)
            .is_ok()
        {
            actions.push("expand".into());
        }
        if element.GetCachedPattern(UIA_ValuePatternId).is_ok() {
            actions.push("set_value".into());
        }
        // RangeValuePattern is exposed by Sliders, ProgressBars, and other
        // numeric-range controls. Without this entry the slider parent
        // gets actions=[] → marked non-actionable → no `[N]` index in the
        // flat tree, making the slider unaddressable by AutomationId.
        if element.GetCachedPattern(UIA_RangeValuePatternId).is_ok() {
            actions.push("set_value".into());
        }
        if element.GetCachedPattern(UIA_TextPatternId).is_ok() {
            actions.push("text".into());
        }
        if element.GetCachedPattern(UIA_ScrollPatternId).is_ok() {
            actions.push("scroll".into());
        }
    }
    if actions.is_empty() && control_type == "MenuItem" {
        // Some WPF MenuItem peers show up in ControlView with a usable name
        // and bounding rectangle, but without cached Invoke/ExpandCollapse
        // patterns. Index them anyway so click can retry live patterns and then
        // fall back to the coordinate injector if UIA still reports no pattern.
        actions.push("invoke".into());
    }
    actions
}

fn read_cached_checked(element: &IUIAutomationElement) -> Option<CheckedState> {
    unsafe {
        let pattern = element.GetCachedPattern(UIA_TogglePatternId).ok()?;
        let toggle = pattern.cast::<IUIAutomationTogglePattern>().ok()?;
        match toggle.CachedToggleState() {
            Ok(state) if state == ToggleState_On => Some(CheckedState::On),
            Ok(state) if state == ToggleState_Off => Some(CheckedState::Off),
            Ok(state) if state == ToggleState_Indeterminate => Some(CheckedState::Mixed),
            _ => None,
        }
    }
}

fn read_cached_expanded(element: &IUIAutomationElement) -> Option<bool> {
    unsafe {
        let pattern = element.GetCachedPattern(UIA_ExpandCollapsePatternId).ok()?;
        let expand = pattern.cast::<IUIAutomationExpandCollapsePattern>().ok()?;
        match expand.CachedExpandCollapseState() {
            Ok(state) if state == ExpandCollapseState_Collapsed => Some(false),
            Ok(state)
                if state == ExpandCollapseState_Expanded
                    || state == ExpandCollapseState_PartiallyExpanded =>
            {
                Some(true)
            }
            // LeafNode: nothing to expand, so no state.
            _ => None,
        }
    }
}

fn read_cached_selected(element: &IUIAutomationElement) -> Option<bool> {
    unsafe {
        if let Ok(pattern) = element.GetCachedPattern(UIA_TogglePatternId) {
            if let Ok(toggle) = pattern.cast::<IUIAutomationTogglePattern>() {
                return match toggle.CachedToggleState() {
                    Ok(state) if state == ToggleState_On => Some(true),
                    Ok(state) if state == ToggleState_Off => Some(false),
                    _ => None,
                };
            }
        }
        if let Ok(pattern) = element.GetCachedPattern(UIA_SelectionItemPatternId) {
            if let Ok(selection) = pattern.cast::<IUIAutomationSelectionItemPattern>() {
                return selection
                    .CachedIsSelected()
                    .ok()
                    .map(|selected| selected.as_bool());
            }
        }
    }
    None
}

fn control_type_name(id: i32) -> String {
    match id {
        50000 => "Button",
        50001 => "Calendar",
        50002 => "CheckBox",
        50003 => "ComboBox",
        50004 => "Edit",
        50005 => "Hyperlink",
        50006 => "Image",
        50007 => "ListItem",
        50008 => "List",
        50009 => "Menu",
        50010 => "MenuBar",
        50011 => "MenuItem",
        50012 => "ProgressBar",
        50013 => "RadioButton",
        50014 => "ScrollBar",
        50015 => "Slider",
        50016 => "Spinner",
        50017 => "StatusBar",
        50018 => "Tab",
        50019 => "TabItem",
        50020 => "Text",
        50021 => "ToolBar",
        50022 => "ToolTip",
        50023 => "Tree",
        50024 => "TreeItem",
        50025 => "Custom",
        50026 => "Group",
        50027 => "Thumb",
        50028 => "DataGrid",
        50029 => "DataItem",
        50030 => "Document",
        50031 => "SplitButton",
        50032 => "Window",
        50033 => "Pane",
        50034 => "Header",
        50035 => "HeaderItem",
        50036 => "Table",
        50037 => "TitleBar",
        50038 => "Separator",
        50039 => "SemanticZoom",
        50040 => "AppBar",
        _ => "Unknown",
    }
    .into()
}

pub(crate) fn format_node_line(node: &UiaNode) -> String {
    let mut s = String::new();
    if let Some(idx) = node.element_index {
        s.push_str(&format!("- [{}] {}", idx, node.control_type));
        if let Some(n) = &node.name {
            s.push_str(&format!(" \"{}\"", n));
        }
        let mut attrs = Vec::new();
        if let Some(v) = &node.value {
            attrs.push(format!("value=\"{}\"", v));
        }
        if let Some(id) = &node.automation_id {
            attrs.push(format!("id={}", id));
        }
        if let Some(h) = &node.help_text {
            attrs.push(format!("help=\"{}\"", h));
        }
        // States are part of the row so a state-only change shows up in incremental reads,
        // which compare rows.
        if let Some(checked) = node.checked {
            attrs.push(format!("toggle={}", checked.as_str()));
        }
        if let Some(expanded) = node.expanded {
            attrs.push(format!("expanded={}", expanded));
        }
        if !node.actions.is_empty() {
            attrs.push(format!("actions=[{}]", node.actions.join(",")));
        }
        if !attrs.is_empty() {
            s.push_str(&format!(" [{}]", attrs.join(" ")));
        }
    } else {
        s.push_str(&format!("- {}", node.control_type));
        if let Some(n) = &node.name {
            s.push_str(&format!(" \"{}\"", n));
        }
        if let Some(v) = &node.value {
            s.push_str(&format!(" = \"{}\"", v));
        }
    }
    s
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

fn filter_tree(markdown: &str, query: &str) -> String {
    let needle = query.to_lowercase();
    let lines: Vec<&str> = markdown.lines().collect();
    let mut ancestors: Vec<&str> = Vec::new();
    let mut last_emitted: Vec<Option<&str>> = Vec::new();
    let mut output: Vec<&str> = Vec::new();

    for line in &lines {
        if is_compaction_summary_line(line) {
            continue;
        }
        let depth = line.chars().take_while(|c| *c == ' ').count() / 2;
        while ancestors.len() <= depth {
            ancestors.push("");
            last_emitted.push(None);
        }
        for d in (depth + 1)..ancestors.len() {
            last_emitted[d] = None;
        }
        ancestors[depth] = line;

        if line.to_lowercase().contains(&needle) {
            for d in 0..depth {
                if ancestors[d].is_empty() {
                    continue;
                }
                if last_emitted[d] == Some(ancestors[d]) {
                    continue;
                }
                last_emitted[d] = Some(ancestors[d]);
                output.push(ancestors[d]);
            }
            last_emitted[depth] = Some(line);
            output.push(line);
        }
    }

    if output.is_empty() {
        return String::new();
    }
    let mut r = output.join("\n");
    r.push('\n');
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_list_view_detection_does_not_select_listbox() {
        assert!(native_list_view_class("SysListView32"));
        assert!(native_list_view_class(
            "WindowsForms10.SysListView32.app.0.34f5582_r7_ad1"
        ));
        assert!(!native_list_view_class("ListBox"));
        assert!(!native_list_view_class(
            "WindowsForms10.LISTBOX.app.0.34f5582_r7_ad1"
        ));
    }

    fn star(child_count: usize) -> Vec<Vec<usize>> {
        let mut children = vec![Vec::new(); child_count + 1];
        children[0] = (1..=child_count).collect();
        children
    }

    fn chain(len: usize) -> Vec<Vec<usize>> {
        let mut children = vec![Vec::new(); len];
        for i in 0..len.saturating_sub(1) {
            children[i] = vec![i + 1];
        }
        children
    }

    fn walk_synthetic_tree(
        children: &[Vec<usize>],
        node: usize,
        depth: usize,
        state: &mut WalkState<'_>,
    ) {
        if !state.enter(depth) {
            return;
        }
        for child in &children[node] {
            walk_synthetic_tree(children, *child, depth + 1, state);
        }
    }

    #[test]
    fn walk_state_is_complete_only_when_no_bound_skips_a_node() {
        struct Case {
            name: &'static str,
            children: Vec<Vec<usize>>,
            max_elements: usize,
            max_depth: usize,
            expected_visited: usize,
            expected_complete: bool,
        }

        let cases = [
            Case {
                name: "default bounds admit a 55-node tree",
                children: star(54),
                max_elements: DEFAULT_MAX_TOTAL_ELEMENTS,
                max_depth: DEFAULT_MAX_DEPTH,
                expected_visited: 55,
                expected_complete: true,
            },
            Case {
                name: "visiting exactly max_elements is still complete",
                children: star(54),
                max_elements: 55,
                max_depth: DEFAULT_MAX_DEPTH,
                expected_visited: 55,
                expected_complete: true,
            },
            Case {
                name: "skipping a node past max_elements is incomplete",
                children: star(54),
                max_elements: 54,
                max_depth: DEFAULT_MAX_DEPTH,
                expected_visited: 54,
                expected_complete: false,
            },
            Case {
                name: "a leaf at exactly max_depth is still complete",
                children: chain(3),
                max_elements: 10,
                max_depth: 2,
                expected_visited: 3,
                expected_complete: true,
            },
            Case {
                name: "skipping a descendant past max_depth is incomplete",
                children: chain(3),
                max_elements: 10,
                max_depth: 1,
                expected_visited: 2,
                expected_complete: false,
            },
        ];

        for case in cases {
            let mut budget = WalkBudget::nodes_only(case.max_elements);
            let mut state = WalkState::new(&mut budget, case.max_depth);
            walk_synthetic_tree(&case.children, 0, 0, &mut state);
            assert_eq!(
                state.budget.outcome().nodes_visited,
                case.expected_visited,
                "{}: visited",
                case.name
            );
            assert_eq!(
                state.complete, case.expected_complete,
                "{}: complete",
                case.name
            );
        }
    }

    #[test]
    fn walk_state_keeps_the_callers_budget_across_walks() {
        let mut budget = WalkBudget::nodes_only(2);
        assert!(budget.admit());
        let mut state = WalkState::new(&mut budget, DEFAULT_MAX_DEPTH);

        walk_synthetic_tree(&star(1), 0, 0, &mut state);

        assert!(!state.complete);
        let outcome = state.budget.outcome();
        assert_eq!((outcome.nodes_visited, outcome.nodes_pending), (2, 1));
        assert_eq!(outcome.reason(), Some("node_budget"));
    }

    #[test]
    fn timed_out_walk_cannot_report_complete() {
        let mut budget = WalkBudget::new(100, 10);
        budget.stop_for_timeout();
        let mut state = WalkState::new(&mut budget, DEFAULT_MAX_DEPTH);

        walk_synthetic_tree(&star(0), 0, 0, &mut state);

        assert!(!state.complete);
        assert_eq!(state.budget.outcome().reason(), Some("timeout"));
        assert_eq!(state.budget.outcome().nodes_visited, 0);
    }

    /// Without a query the generic walk leaves offscreen deferred rows as a
    /// summary row; the element list then misses them and is not complete.
    #[test]
    fn unwalked_offscreen_rows_make_the_walk_incomplete() {
        for (visible, search_offscreen, complete) in [
            (vec![true, false], false, false),
            (vec![false], false, false),
            (vec![true, true], false, true),
            (vec![true, false], true, true),
            (vec![], false, true),
        ] {
            let mut budget = WalkBudget::nodes_only(10);
            let mut state = WalkState::new(&mut budget, DEFAULT_MAX_DEPTH);
            note_unwalked_offscreen_rows(&mut state, visible.clone(), search_offscreen);
            assert_eq!(
                state.complete, complete,
                "{visible:?} search={search_offscreen}"
            );
        }
    }
}
