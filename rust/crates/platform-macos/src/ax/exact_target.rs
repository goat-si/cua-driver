//! Fresh exact-target acquisition for background input decisions.
//!
//! Gathers the facts [`cua_driver_core::background_input`] needs about one
//! requested `(pid, CGWindowID)` immediately before a background mutation:
//! WindowServer ownership, fresh `AXWindows` membership (mapped through
//! `_AXUIElementGetWindow`), minimized/hidden state, competing same-pid AX
//! top-level keyboard destinations, and addressed-element ancestry. All reads are
//! bounded and fail closed — an unreadable fact never unlocks a route.

use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFTypeRef};
use cua_driver_core::background_input::{
    BackgroundTargetFacts, ElementAncestry, WindowServerOwnership,
};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use super::bindings::{
    ax_get_window_id, copy_ax_windows_including, copy_bool_attr, copy_children_checked,
    copy_element_attr, copy_string_attr, focused_element_of_pid, AXUIElementCreateApplication,
    AXUIElementRef, AXUIElementSetMessagingTimeout,
};
use super::snapshot::RetainedElement;
use crate::windows::{all_windows_without_spaces, resolve_window_owner, WindowOwner};

/// Bounded `AXParent` ascent used when an element does not expose `AXWindow`.
const MAX_ANCESTRY_DEPTH: usize = 40;

/// Bounded sheet chain followed when mapping a sheet to its parent window.
const MAX_SHEET_NESTING: usize = 8;

#[derive(Debug)]
pub(crate) struct ContextMenuElement(pub(crate) usize);
impl Drop for ContextMenuElement {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) }
    }
}
impl Clone for ContextMenuElement {
    fn clone(&self) -> Self {
        unsafe { CFRetain(self.0 as CFTypeRef) };
        Self(self.0)
    }
}

struct ContextMenuRecord {
    session: String,
    generation: u64,
    pid: i32,
    window_id: u32,
    target: Option<ContextMenuElement>,
    menu: ContextMenuElement,
}

#[derive(Default)]
struct ContextMenuRegistry {
    next_id: u64,
    records: HashMap<u64, ContextMenuRecord>,
    generations: HashMap<String, u64>,
}

static CONTEXT_MENUS: LazyLock<Mutex<ContextMenuRegistry>> =
    LazyLock::new(|| Mutex::new(ContextMenuRegistry::default()));

fn generation(registry: &ContextMenuRegistry, session: &str) -> u64 {
    registry.generations.get(session).copied().unwrap_or(0)
}

pub(crate) fn context_menu_generation(session: &str) -> u64 {
    let registry = CONTEXT_MENUS.lock().unwrap();
    generation(&registry, session)
}

pub(crate) fn register_context_menu(
    session: &str,
    expected_generation: u64,
    pid: i32,
    window_id: u32,
    target: Option<ContextMenuElement>,
    menu: usize,
) -> Option<u64> {
    let mut registry = CONTEXT_MENUS.lock().unwrap();
    if generation(&registry, session) != expected_generation {
        return None;
    }
    registry.next_id += 1;
    let id = registry.next_id;
    registry.records.insert(
        id,
        ContextMenuRecord {
            session: session.to_owned(),
            generation: expected_generation,
            pid,
            window_id,
            target,
            menu: ContextMenuElement(menu),
        },
    );
    Some(id)
}

pub(crate) fn context_menu_for_close(
    session: &str,
    pid: i32,
    window_id: u32,
    opener_id: u64,
) -> Option<(ContextMenuElement, Option<ContextMenuElement>)> {
    let registry = CONTEXT_MENUS.lock().unwrap();
    let record = registry.records.get(&opener_id)?;
    (record.session == session
        && record.generation == generation(&registry, session)
        && record.pid == pid
        && record.window_id == window_id)
        .then(|| (record.menu.clone(), record.target.clone()))
}

pub(crate) fn finish_context_menu_close(opener_id: u64) {
    CONTEXT_MENUS.lock().unwrap().records.remove(&opener_id);
}

pub(crate) fn end_context_menu_session(session: &str) {
    let mut registry = CONTEXT_MENUS.lock().unwrap();
    *registry.generations.entry(session.to_owned()).or_default() += 1;
    registry
        .records
        .retain(|_, record| record.session != session);
}

pub(crate) fn revive_context_menu_session(session: &str) {
    let mut registry = CONTEXT_MENUS.lock().unwrap();
    *registry.generations.entry(session.to_owned()).or_default() += 1;
}

unsafe fn menu_has_opener_record(pid: i32, window_id: u32, menu: AXUIElementRef) -> bool {
    let registry = CONTEXT_MENUS.lock().unwrap();
    registry.records.values().any(|record| {
        record.pid == pid
            && record.window_id == window_id
            && CFEqual(record.menu.0 as CFTypeRef, menu as CFTypeRef) != 0
    })
}

/// Resolve the CGWindowID of the top-level AX window that owns `element`.
///
/// Prefers the element's `AXWindow` attribute and falls back to a bounded
/// `AXParent` walk. `None` means ancestry could not be proven — callers must
/// treat that as "not the requested window", never as a wildcard.
///
/// A sheet is its own WindowServer window (and a remote Open/Save panel's
/// content lives in yet another, service-owned one), but it is modal to the
/// window it is attached to and is observed inside that window's snapshot.
/// An element under an `AXSheet` therefore resolves to the sheet's parent
/// window when AX proves that parent (issue #4392).
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
pub unsafe fn element_window_id(element: AXUIElementRef) -> Option<u32> {
    if let Some(window) = copy_element_attr(element, "AXWindow") {
        let window_id = owning_window_id(window);
        CFRelease(window as CFTypeRef);
        if window_id.is_some() {
            return window_id;
        }
    }
    // Fallback: ascend AXParent until a window role, then map it.
    let mut current: AXUIElementRef = element;
    let mut owned = false;
    let mut resolved = None;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXWindow") | Some("AXSheet") => {
                resolved = owning_window_id(current);
                break;
            }
            Some("AXApplication") | None => break,
            _ => {}
        }
        let parent = copy_element_attr(current, "AXParent");
        if owned {
            CFRelease(current as CFTypeRef);
        }
        {
            let parent = parent?;
            current = parent;
            owned = true;
        }
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    resolved
}

/// Decide an addressed element's ancestry from what was read about it.
fn element_ancestry(window: Option<u32>, in_app_menu_bar: bool, requested: u32) -> ElementAncestry {
    match window {
        Some(id) if id == requested => ElementAncestry::ProvenDescendant,
        Some(_) => ElementAncestry::OutsideTargetWindow,
        None if in_app_menu_bar => ElementAncestry::ProvenDescendant,
        None => ElementAncestry::Unproven,
    }
}

/// Whether `element` sits under the application's own `AXMenuBar`: a bounded
/// `AXParent` ascent that must end at the same menu bar element the
/// application reports.
///
/// # Safety
///
/// `app` and `element` must be valid `AXUIElementRef`s.
unsafe fn element_in_app_menu_bar(app: AXUIElementRef, element: AXUIElementRef) -> bool {
    let Some(menu_bar) = copy_element_attr(app, "AXMenuBar") else {
        return false;
    };
    let mut current = element;
    let mut owned = false;
    let mut found = false;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if core_foundation::base::CFEqual(current as CFTypeRef, menu_bar as CFTypeRef) != 0 {
            found = true;
            break;
        }
        if matches!(
            copy_string_attr(current, "AXRole").as_deref(),
            Some("AXWindow") | Some("AXSheet") | Some("AXApplication") | None
        ) {
            break;
        }
        let parent = copy_element_attr(current, "AXParent");
        if owned {
            CFRelease(current as CFTypeRef);
        }
        match parent {
            Some(parent) => {
                current = parent;
                owned = true;
            }
            None => {
                owned = false;
                break;
            }
        }
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    CFRelease(menu_bar as CFTypeRef);
    found
}

unsafe fn element_in_recorded_top_level_menu(
    _app: AXUIElementRef,
    element: AXUIElementRef,
    pid: i32,
    window_id: u32,
) -> bool {
    let mut current = element;
    let mut owned = false;
    let mut found = false;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if copy_string_attr(current, "AXRole").as_deref() == Some("AXMenu") {
            if let Some(parent) = copy_element_attr(current, "AXParent") {
                found = menu_has_opener_record(pid, window_id, current);
                CFRelease(parent as CFTypeRef);
            }
            break;
        }
        if matches!(
            copy_string_attr(current, "AXRole").as_deref(),
            Some("AXWindow") | Some("AXSheet") | Some("AXApplication") | None
        ) {
            break;
        }
        let parent = copy_element_attr(current, "AXParent");
        if owned {
            CFRelease(current as CFTypeRef);
        }
        match parent {
            Some(parent) => {
                current = parent;
                owned = true;
            }
            None => {
                owned = false;
                break;
            }
        }
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    found
}

/// Map a window-role AX element to the CGWindowID of the top-level window it
/// belongs to: an `AXWindow` maps to itself, an `AXSheet` to the window it is
/// attached to (see [`element_window_id`]).
///
/// # Safety
///
/// `window` must be a valid `AXUIElementRef` for the duration of the call.
unsafe fn owning_window_id(window: AXUIElementRef) -> Option<u32> {
    resolve_owning_window(
        RetainedElement::retain(window as usize),
        MAX_SHEET_NESTING,
        |element| copy_string_attr(element.as_ptr() as AXUIElementRef, "AXRole"),
        |element| {
            copy_element_attr(element.as_ptr() as AXUIElementRef, "AXParent").map(|parent| {
                // copy_element_attr returned +1; hand that reference over.
                let retained = RetainedElement::retain(parent as usize);
                CFRelease(parent as CFTypeRef);
                retained
            })
        },
        |element| ax_get_window_id(element.as_ptr() as AXUIElementRef),
    )
}

/// Pure sheet-to-parent resolution behind [`owning_window_id`].
///
/// A sheet whose parent is not a window (or a sheet attached to another
/// sheet) is followed up to `max_nesting` levels. When no parent window is
/// proven, the sheet keeps its own window id: that never names a different
/// window than before, so it can only stay refused, never widen.
fn resolve_owning_window<E>(
    start: E,
    max_nesting: usize,
    role: impl Fn(&E) -> Option<String>,
    parent: impl Fn(&E) -> Option<E>,
    window_id: impl Fn(&E) -> Option<u32>,
) -> Option<u32> {
    let own_id = window_id(&start);
    let mut current = start;
    for _ in 0..=max_nesting {
        match role(&current).as_deref() {
            Some("AXSheet") => {}
            Some("AXWindow") => return window_id(&current),
            _ => return own_id,
        }
        current = match parent(&current) {
            Some(next) => next,
            None => return own_id,
        };
    }
    own_id
}

/// The process's focused AX element, but only when it provably belongs to the
/// requested window. Returns a retained element the caller must release.
///
/// This is the only focused-element reader background window-scoped keyboard
/// paths may use: a PID-global focused element can belong to a sibling window,
/// and sibling state must never address or confirm the requested target.
///
/// # Safety
///
/// Caller must `CFRelease` the returned element.
pub unsafe fn focused_element_in_window(pid: i32, window_id: u32) -> Option<AXUIElementRef> {
    let element = focused_element_of_pid(pid)?;
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        CFRelease(element as CFTypeRef);
        return None;
    }
    let requested = sheet_parent_window_id(app, pid, window_id).unwrap_or(window_id);
    CFRelease(app as CFTypeRef);
    if requested != window_id && !element_in_requested_sheet(element, window_id) {
        CFRelease(element as CFTypeRef);
        return None;
    }
    if element_window_id(element) == Some(requested) {
        Some(element)
    } else {
        CFRelease(element as CFTypeRef);
        None
    }
}

unsafe fn sheet_parent_window_id(
    app: AXUIElementRef,
    pid: i32,
    sheet_window_id: u32,
) -> Option<u32> {
    let windows = copy_ax_windows_including(app, pid, sheet_window_id);
    // A window AX lists with the role AXWindow is a plain window and never a
    // sheet (a sheet is an AXSheet child of its window): there is no parent to
    // find, so skip reading the children of every window. This runs before
    // every key and in every background gate.
    let plain_window = windows.iter().any(|&window| {
        ax_get_window_id(window) == Some(sheet_window_id)
            && copy_string_attr(window, "AXRole").as_deref() == Some("AXWindow")
    });
    if plain_window {
        for window in windows {
            CFRelease(window as CFTypeRef);
        }
        return None;
    }
    let mut found = None;
    for window in windows {
        if found.is_none() {
            if let Some(children) = copy_children_checked(window) {
                for child in children {
                    if copy_string_attr(child, "AXRole").as_deref() == Some("AXSheet")
                        && ax_get_window_id(child) == Some(sheet_window_id)
                    {
                        found = ax_get_window_id(window);
                    }
                    CFRelease(child as CFTypeRef);
                }
            }
        }
        CFRelease(window as CFTypeRef);
    }
    found
}

/// The window `sheet_window_id` is attached to as an `AXSheet`, or `None`
/// when it is not a sheet of one of `pid`'s windows.
pub fn sheet_parent_of(pid: i32, sheet_window_id: u32) -> Option<u32> {
    // SAFETY: the application element is created and released here.
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let parent = sheet_parent_window_id(app, pid, sheet_window_id);
        CFRelease(app as CFTypeRef);
        parent
    }
}

unsafe fn element_in_requested_sheet(element: AXUIElementRef, sheet_window_id: u32) -> bool {
    let mut current = element;
    let mut owned = false;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if copy_string_attr(current, "AXRole").as_deref() == Some("AXSheet") {
            let found = ax_get_window_id(current) == Some(sheet_window_id);
            if owned {
                CFRelease(current as CFTypeRef);
            }
            return found;
        }
        let parent = copy_element_attr(current, "AXParent");
        if owned {
            CFRelease(current as CFTypeRef);
        }
        match parent {
            Some(parent) => {
                current = parent;
                owned = true;
            }
            None => return false,
        }
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    false
}

/// Classification of whether a window-scoped keyboard action conflicts with
/// the addressed element's window or a competing sibling window's focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowFocusConflict {
    /// Neither the addressed element nor the process's focused element/window
    /// belongs to a different window of the same PID.
    Clear,
    /// An explicitly addressed element resolved outside `target_window_id`
    /// (or its window ancestry could not be proven).
    AddressedElementOutsideWindow {
        target_window_id: u32,
        actual_window_id: Option<u32>,
    },
    /// The process's focused element or `AXFocusedWindow` belongs to a
    /// sibling window (`sibling_window_id != target_window_id`), so a
    /// process-scoped key event risks landing in that sibling window.
    SiblingWindowFocused {
        target_window_id: u32,
        sibling_window_id: u32,
    },
    /// The target is a sheet and the process's focused element could not be
    /// proven to sit inside it (unreadable, or in the sheet's parent outside
    /// the sheet): a key could reach the parent's content.
    SheetFocusUnproven { sheet_window_id: u32 },
}

/// Pure decision for [`check_window_focus_conflict`].
///
/// `target_is_sheet`: the target is an `AXSheet` with its own CGWindowID.
/// AX maps every element of a sheet to the window it is attached to, so the
/// caller reports an element proven inside the requested sheet as
/// `target_window_id`. For a sheet target that proof is required: an
/// unreadable focused element refuses instead of passing as "no conflict".
/// `AXFocusedWindow` must still name the sheet itself.
pub fn decide_window_focus_conflict(
    target_window_id: u32,
    addressed_element_window: Option<Option<u32>>,
    addressed_in_menu_bar: bool,
    focused_element_window: Option<u32>,
    focused_window_id: Option<u32>,
    target_is_sheet: bool,
) -> WindowFocusConflict {
    if let Some(actual) = addressed_element_window {
        match actual {
            Some(wid) if wid == target_window_id => {}
            None if addressed_in_menu_bar => {}
            other => {
                return WindowFocusConflict::AddressedElementOutsideWindow {
                    target_window_id,
                    actual_window_id: other,
                };
            }
        }
    }
    if let Some(sibling_window_id) = focused_element_window.filter(|&w| w != target_window_id) {
        if target_is_sheet {
            return WindowFocusConflict::SheetFocusUnproven {
                sheet_window_id: target_window_id,
            };
        }
        return WindowFocusConflict::SiblingWindowFocused {
            target_window_id,
            sibling_window_id,
        };
    }
    if target_is_sheet && focused_element_window.is_none() {
        return WindowFocusConflict::SheetFocusUnproven {
            sheet_window_id: target_window_id,
        };
    }
    // A sheet's key window must be read as the sheet itself.
    if target_is_sheet && focused_window_id.is_none() {
        return WindowFocusConflict::SheetFocusUnproven {
            sheet_window_id: target_window_id,
        };
    }
    if let Some(sibling_window_id) = focused_window_id.filter(|&w| w != target_window_id) {
        return WindowFocusConflict::SiblingWindowFocused {
            target_window_id,
            sibling_window_id,
        };
    }
    WindowFocusConflict::Clear
}

/// The window an element counts toward for a keyboard target: the requested
/// sheet itself when the element is proven inside it, else its owning window
/// (which for any sheet element is the sheet's parent).
unsafe fn keyboard_element_window(
    element: AXUIElementRef,
    target_window_id: u32,
    sheet_parent: Option<u32>,
) -> Option<u32> {
    let window = element_window_id(element);
    if sheet_parent.is_some()
        && window == sheet_parent
        && element_in_requested_sheet(element, target_window_id)
    {
        return Some(target_window_id);
    }
    window
}

/// Inspect live AX state immediately before a window-scoped keyboard dispatch
/// to verify that neither the addressed element nor the process's current
/// focus belongs to a sibling window of `pid`.
pub fn check_window_focus_conflict(
    pid: i32,
    target_window_id: u32,
    addressed_element_ptr: Option<usize>,
) -> WindowFocusConflict {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return match addressed_element_ptr {
                Some(_) => WindowFocusConflict::AddressedElementOutsideWindow {
                    target_window_id,
                    actual_window_id: None,
                },
                None => WindowFocusConflict::Clear,
            };
        }
        let sheet_parent = sheet_parent_window_id(app, pid, target_window_id);
        let (addressed_window, in_menu_bar) = match addressed_element_ptr {
            Some(ptr) => {
                let el = ptr as AXUIElementRef;
                let win = keyboard_element_window(el, target_window_id, sheet_parent);
                let menu = win.is_none() && element_in_app_menu_bar(app, el);
                (Some(win), menu)
            }
            None => (None, false),
        };
        let focused_element_window = focused_element_of_pid(pid).and_then(|focused| {
            let win = keyboard_element_window(focused, target_window_id, sheet_parent);
            CFRelease(focused as CFTypeRef);
            win
        });
        let focused_window_id = copy_element_attr(app, "AXFocusedWindow").and_then(|window| {
            let wid = ax_get_window_id(window);
            CFRelease(window as CFTypeRef);
            wid
        });
        CFRelease(app as CFTypeRef);
        decide_window_focus_conflict(
            target_window_id,
            addressed_window,
            in_menu_bar,
            focused_element_window,
            focused_window_id,
            sheet_parent.is_some(),
        )
    }
}

/// One fresh `AXWindows` row: the mapped CGWindowID plus its minimized state.
/// `minimized: None` means the attribute could not be read — unknown, not
/// "not minimized".
struct AxWindowRecord {
    window_id: u32,
    minimized: Option<bool>,
}

/// Map the application's fresh `AXWindows` — plus the requested window when it
/// is on another Space — through `_AXUIElementGetWindow`. Windows whose id the
/// SPI cannot resolve are omitted: an unmappable window can never satisfy an
/// exact-target requirement.
unsafe fn ax_window_records(app: AXUIElementRef, pid: i32, window_id: u32) -> Vec<AxWindowRecord> {
    copy_ax_windows_including(app, pid, window_id)
        .into_iter()
        .filter_map(|window| {
            let record = ax_get_window_id(window).map(|window_id| AxWindowRecord {
                window_id,
                minimized: copy_bool_attr(window, "AXMinimized"),
            });
            CFRelease(window as CFTypeRef);
            record
        })
        .collect()
}

/// One same-session WindowServer row as the keyboard-ambiguity count sees it.
struct WindowServerRow {
    pid: i32,
    window_id: u32,
    /// On screen and of real size. An ordered-out window (LibreOffice's
    /// `VCL ImplGetDefaultWindow`, a closed Chrome omnibox popup) cannot be
    /// the key window, so process-scoped keys can never reach it.
    visible: bool,
}

/// Count independently AX-mapped, visible, non-minimized sibling top-level
/// windows that could receive process-scoped key events instead of the target.
///
/// WindowServer may expose several layer-0 compositor surfaces for one native
/// Electron, Tauri, or WebKit window. A raw same-pid CGWindow row is therefore
/// not enough to prove another process-scoped keyboard destination. Requiring a
/// fresh `AXWindows` mapping preserves the fail-closed two-window guard while
/// ignoring render surfaces that cannot independently become the AX key window.
///
/// AppKit sends a process's key events to its key window. When AX proves the
/// target is that window (`target_is_key`), no sibling can receive them, so a
/// sibling such as an open omnibox popup does not make the target ambiguous.
fn count_competing_keyboard_destinations(
    pid: i32,
    target_window_id: u32,
    window_server_rows: impl IntoIterator<Item = WindowServerRow>,
    ax_records: &[AxWindowRecord],
    target_is_key: bool,
) -> usize {
    if target_is_key {
        return 0;
    }
    window_server_rows
        .into_iter()
        .filter(|row| {
            row.pid == pid
                && row.window_id != target_window_id
                && row.visible
                && ax_records.iter().any(|record| {
                    record.window_id == row.window_id && record.minimized != Some(true)
                })
        })
        .count()
}

/// Gather fresh background-input facts for one `(pid, window_id)` target.
///
/// `element_ptr` is an optional retained `AXUIElementRef` (as `usize`) for an
/// explicitly addressed element; the caller must keep it retained for the
/// duration of this call. Blocking: performs up to two Space-free CGWindowList
/// enumerations and bounded AX reads. Call from a blocking context immediately
/// before deciding.
pub fn gather_background_facts(
    pid: i32,
    window_id: u32,
    element_ptr: Option<usize>,
) -> BackgroundTargetFacts {
    let window_server = match resolve_window_owner(pid, window_id) {
        WindowOwner::SamePid => WindowServerOwnership::SamePid,
        WindowOwner::Unknown => WindowServerOwnership::NotFound,
        WindowOwner::ForeignPid { owner_pid, .. } => {
            WindowServerOwnership::ForeignPid { owner_pid }
        }
    };

    // SAFETY: the application element is created and released here; window
    // elements are released inside ax_window_records; the caller guarantees
    // element_ptr stays retained.
    let (records, app_hidden, element, focused_window_id, sheet_parent) = unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            (
                Vec::new(),
                None,
                element_ptr.map(|_| ElementAncestry::Unproven),
                None,
                None,
            )
        } else {
            // Electron/Chromium apps may need per-process-lifetime enablement
            // before their AX windows and subtrees are materialized.
            super::enablement::ensure_chromium_ax_enabled(pid, app);
            let records = ax_window_records(app, pid, window_id);
            let app_hidden = copy_bool_attr(app, "AXHidden");
            let element = element_ptr.map(|ptr| {
                let element = ptr as AXUIElementRef;
                let window = element_window_id(element);
                // A menu-bar item has no window ancestor: the menu bar hangs
                // off the application. It still belongs to this app.
                let in_menu_bar = window.is_none() && element_in_app_menu_bar(app, element);
                let in_recorded_menu = window.is_none()
                    && element_in_recorded_top_level_menu(app, element, pid, window_id);
                let requested = match sheet_parent_window_id(app, pid, window_id) {
                    Some(parent) if element_in_requested_sheet(element, window_id) => parent,
                    _ => window_id,
                };
                // An item of an open context menu reports the menu's own
                // window; the menu itself hangs off a view of the requested
                // window (Finder's list view), proven through AXParent.
                let window = if window != Some(requested)
                    && super::context_menus::element_in_context_menu_of(element, requested)
                {
                    Some(requested)
                } else {
                    window
                };
                element_ancestry(window, in_menu_bar || in_recorded_menu, requested)
            });
            let focused_window_id = copy_element_attr(app, "AXFocusedWindow").and_then(|window| {
                let window_id = ax_get_window_id(window);
                CFRelease(window as CFTypeRef);
                window_id
            });
            let sheet_parent = sheet_parent_window_id(app, pid, window_id);
            CFRelease(app as CFTypeRef);
            (
                records,
                app_hidden,
                element,
                focused_window_id,
                sheet_parent,
            )
        }
    };

    let target = records.iter().find(|record| record.window_id == window_id);
    // A sheet is not in AXWindows; its minimized state is the minimized
    // state of the window it is attached to.
    let target_minimized = target_minimized_state(&records, window_id, sheet_parent);
    let competing_rows = all_windows_without_spaces()
        .iter()
        .filter(|window| Some(window.window_id) != sheet_parent)
        .map(|window| WindowServerRow {
            pid: window.pid,
            window_id: window.window_id,
            visible: window.is_on_screen
                && window.bounds.width >= 2.0
                && window.bounds.height >= 2.0,
        })
        .collect::<Vec<_>>();
    let competing_keyboard_destinations = count_competing_keyboard_destinations(
        pid,
        window_id,
        competing_rows,
        &records,
        target.is_some() && focused_window_id == Some(window_id),
    );

    BackgroundTargetFacts {
        window_server,
        ax_window_present: target.is_some() || sheet_parent.is_some(),
        target_minimized,
        app_hidden,
        competing_keyboard_destinations,
        element: element.unwrap_or(ElementAncestry::NotAddressed),
        screen_locked: crate::session::screen_locked(),
    }
}

/// `AXMinimized` of the requested window. A sheet has no `AXMinimized` and
/// is not listed in `AXWindows`: it is shown exactly when the window it is
/// attached to is, so it takes that window's state. Unknown stays unknown.
fn target_minimized_state(
    records: &[AxWindowRecord],
    window_id: u32,
    sheet_parent: Option<u32>,
) -> Option<bool> {
    let minimized_of = |id: u32| {
        records
            .iter()
            .find(|record| record.window_id == id)
            .and_then(|record| record.minimized)
    };
    match records.iter().any(|record| record.window_id == window_id) {
        true => minimized_of(window_id),
        false => sheet_parent.and_then(minimized_of),
    }
}

/// owning_window_id under the caller's deadline: the role, parent and
/// window-id reads stop when the shared budget expires.
unsafe fn owning_window_id_with_deadline(
    window: AXUIElementRef,
    deadline: std::time::Instant,
) -> Option<u32> {
    resolve_owning_window(
        RetainedElement::retain(window as usize),
        MAX_SHEET_NESTING,
        |element| {
            if std::time::Instant::now() >= deadline {
                None
            } else {
                let _ = AXUIElementSetMessagingTimeout(element.as_ptr() as AXUIElementRef, 0.5);
                copy_string_attr(element.as_ptr() as AXUIElementRef, "AXRole")
            }
        },
        |element| {
            if std::time::Instant::now() >= deadline {
                None
            } else {
                copy_element_attr(element.as_ptr() as AXUIElementRef, "AXParent").map(|parent| {
                    let retained = RetainedElement::retain(parent as usize);
                    CFRelease(parent as CFTypeRef);
                    retained
                })
            }
        },
        |element| {
            if std::time::Instant::now() >= deadline {
                None
            } else {
                let _ = AXUIElementSetMessagingTimeout(element.as_ptr() as AXUIElementRef, 0.5);
                ax_get_window_id(element.as_ptr() as AXUIElementRef)
            }
        },
    )
}

/// focused_element_in_window under the caller's deadline: each acquisition
/// and ownership read is checked so a stalled app cannot outrun the guard's
/// budget.
///
/// # Safety
///
/// Caller must release the returned element exactly once with CFRelease.
pub unsafe fn focused_element_in_window_with_deadline(
    pid: i32,
    window_id: u32,
    deadline: std::time::Instant,
) -> Option<AXUIElementRef> {
    if std::time::Instant::now() >= deadline {
        return None;
    }
    let element = focused_element_of_pid(pid)?;
    if std::time::Instant::now() >= deadline {
        CFRelease(element as CFTypeRef);
        return None;
    }
    if element_window_id_with_deadline(element, deadline) == Some(window_id) {
        Some(element)
    } else {
        CFRelease(element as CFTypeRef);
        None
    }
}

/// element_window_id under the caller's deadline: every read in the
/// ownership walk is checked so a stalled app cannot outrun the guard.
///
/// # Safety
///
/// Caller must release the returned element exactly once with CFRelease.
pub unsafe fn element_window_id_with_deadline(
    element: AXUIElementRef,
    deadline: std::time::Instant,
) -> Option<u32> {
    if std::time::Instant::now() >= deadline {
        return None;
    }
    let _ = AXUIElementSetMessagingTimeout(element, 0.5);
    if let Some(window) = copy_element_attr(element, "AXWindow") {
        let window_id = owning_window_id_with_deadline(window, deadline);
        CFRelease(window as CFTypeRef);
        if window_id.is_some() {
            return window_id;
        }
    }
    let mut current: AXUIElementRef = element;
    let mut owned = false;
    let mut resolved = None;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if std::time::Instant::now() >= deadline {
            if owned {
                CFRelease(current as CFTypeRef);
            }
            return None;
        }
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXWindow") | Some("AXSheet") => {
                resolved = owning_window_id_with_deadline(current, deadline);
                break;
            }
            Some("AXApplication") | None => break,
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            if owned {
                CFRelease(current as CFTypeRef);
            }
            return None;
        }
        let parent = copy_element_attr(current, "AXParent");
        if owned {
            CFRelease(current as CFTypeRef);
        }
        {
            let parent = parent?;
            let _ = AXUIElementSetMessagingTimeout(parent, 0.5);
            current = parent;
            owned = true;
        }
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::{
        count_competing_keyboard_destinations, resolve_owning_window, AxWindowRecord,
        WindowServerRow,
    };

    /// A fake AX node: (role, AXParent index, CGWindowID).
    type FakeNode = (&'static str, Option<usize>, Option<u32>);

    fn resolve(tree: &[FakeNode], start: usize) -> Option<u32> {
        resolve_owning_window(
            start,
            8,
            |&index| Some(tree[index].0.to_owned()),
            |&index| tree[index].1,
            |&index| tree[index].2,
        )
    }

    #[test]
    fn menu_bar_item_is_proven_only_through_the_app_menu_bar() {
        use super::element_ancestry;
        use cua_driver_core::background_input::ElementAncestry::*;
        assert_eq!(element_ancestry(None, true, 7), ProvenDescendant);
        assert_eq!(element_ancestry(None, false, 7), Unproven);
        assert_eq!(element_ancestry(Some(7), false, 7), ProvenDescendant);
        assert_eq!(element_ancestry(Some(8), true, 7), OutsideTargetWindow);
    }

    #[test]
    fn window_maps_to_itself() {
        assert_eq!(resolve(&[("AXWindow", None, Some(167))], 0), Some(167));
    }

    #[test]
    fn sheet_maps_to_the_window_it_is_attached_to() {
        // Open panel sheet 172 attached to window 167 (issue #4392).
        let tree = [
            ("AXWindow", None, Some(167)),
            ("AXSheet", Some(0), Some(172)),
        ];
        assert_eq!(resolve(&tree, 1), Some(167));
    }

    #[test]
    fn nested_sheets_map_to_the_root_window() {
        let tree = [
            ("AXWindow", None, Some(167)),
            ("AXSheet", Some(0), Some(172)),
            ("AXSheet", Some(1), Some(180)),
        ];
        assert_eq!(resolve(&tree, 2), Some(167));
    }

    #[test]
    fn sheet_without_a_proven_parent_window_keeps_its_own_id() {
        // A top-level consent sheet whose parent is the application, and a
        // sheet whose parent cannot be read, never resolve to another window.
        let tree = [
            ("AXApplication", None, None),
            ("AXSheet", Some(0), Some(172)),
        ];
        assert_eq!(resolve(&tree, 1), Some(172));
        assert_eq!(resolve(&[("AXSheet", None, Some(172))], 0), Some(172));
    }

    #[test]
    fn sheet_chain_is_bounded() {
        let tree = [
            ("AXSheet", Some(1), Some(172)),
            ("AXSheet", Some(0), Some(173)),
        ];
        assert_eq!(resolve(&tree, 0), Some(172));
    }

    fn ax_window(window_id: u32, minimized: Option<bool>) -> AxWindowRecord {
        AxWindowRecord {
            window_id,
            minimized,
        }
    }

    fn rows<const N: usize>(rows: [(i32, u32); N]) -> Vec<WindowServerRow> {
        rows.into_iter()
            .map(|(pid, window_id)| WindowServerRow {
                pid,
                window_id,
                visible: true,
            })
            .collect()
    }

    #[test]
    fn compositor_surfaces_do_not_create_keyboard_ambiguity() {
        let rows = rows([(42, 10), (42, 11), (42, 12), (42, 13), (42, 14), (42, 15)]);
        let records = [ax_window(10, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records, false),
            0
        );
    }

    #[test]
    fn independently_mapped_sibling_remains_ambiguous() {
        let rows = rows([(42, 10), (42, 11)]);
        let records = [ax_window(10, Some(false)), ax_window(11, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records, false),
            1
        );
    }

    #[test]
    fn minimized_mapped_sibling_is_not_a_keyboard_destination() {
        let rows = rows([(42, 10), (42, 11)]);
        let records = [ax_window(10, Some(false)), ax_window(11, Some(true))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records, false),
            0
        );
    }

    #[test]
    fn unmapped_window_server_sibling_is_not_a_keyboard_destination() {
        let rows = rows([(42, 10), (42, 99), (7, 11)]);
        let records = [ax_window(10, Some(false)), ax_window(11, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records, false),
            0
        );
    }

    #[test]
    fn off_screen_mapped_sibling_is_not_a_keyboard_destination() {
        // LibreOffice lists `VCL ImplGetDefaultWindow` in AXWindows, but it is
        // ordered out and can never be the key window (bench CDB-S02).
        let mut rows = rows([(42, 10), (42, 11)]);
        rows[1].visible = false;
        let records = [ax_window(10, Some(false)), ax_window(11, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records, false),
            0
        );
    }

    #[test]
    fn a_proven_key_target_has_no_competing_destination() {
        // Chrome with its omnibox popup open: the popup is visible and
        // AX-mapped, but the browser window stays key (bench CDB-G04).
        let rows = rows([(42, 10), (42, 11)]);
        let records = [ax_window(10, Some(false)), ax_window(11, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records, true),
            0
        );
    }

    #[test]
    fn window_focus_conflict_detects_sibling_focus_and_foreign_elements() {
        use super::{decide_window_focus_conflict, WindowFocusConflict};

        assert_eq!(
            decide_window_focus_conflict(700, Some(Some(700)), false, Some(700), Some(700), false),
            WindowFocusConflict::Clear
        );
        assert_eq!(
            decide_window_focus_conflict(700, Some(None), true, None, Some(700), false),
            WindowFocusConflict::Clear
        );
        assert_eq!(
            decide_window_focus_conflict(700, Some(Some(888)), false, Some(700), Some(700), false),
            WindowFocusConflict::AddressedElementOutsideWindow {
                target_window_id: 700,
                actual_window_id: Some(888),
            }
        );
        assert_eq!(
            decide_window_focus_conflict(700, None, false, Some(888), Some(700), false),
            WindowFocusConflict::SiblingWindowFocused {
                target_window_id: 700,
                sibling_window_id: 888,
            }
        );
        assert_eq!(
            decide_window_focus_conflict(700, None, false, None, Some(888), false),
            WindowFocusConflict::SiblingWindowFocused {
                target_window_id: 700,
                sibling_window_id: 888,
            }
        );
    }

    #[test]
    fn a_sheet_target_needs_its_focused_element_proven_inside_it() {
        use super::{decide_window_focus_conflict, WindowFocusConflict};
        // Sheet 900 is attached to window 700. The caller reports an element
        // proven inside the sheet as 900; any other element of the parent
        // keeps the parent's id.
        assert_eq!(
            decide_window_focus_conflict(900, None, false, Some(900), Some(900), true),
            WindowFocusConflict::Clear
        );
        let unproven = WindowFocusConflict::SheetFocusUnproven {
            sheet_window_id: 900,
        };
        // The sheet's element focused, the key window unreadable.
        assert_eq!(
            decide_window_focus_conflict(900, None, false, Some(900), None, true),
            unproven
        );
        // Unreadable focus: refused, where a plain window passes.
        assert_eq!(
            decide_window_focus_conflict(900, None, false, None, Some(900), true),
            unproven
        );
        assert_eq!(
            decide_window_focus_conflict(900, None, false, None, Some(900), false),
            WindowFocusConflict::Clear
        );
        // A parent control outside the sheet, or another sheet's window.
        assert_eq!(
            decide_window_focus_conflict(900, None, false, Some(700), Some(900), true),
            unproven
        );
        assert_eq!(
            decide_window_focus_conflict(900, None, false, Some(901), Some(900), true),
            unproven
        );
        // The sheet's element focused while the parent (or a sibling) is
        // the app's focused window: keys would follow that window.
        for focused_window in [700, 888] {
            assert_eq!(
                decide_window_focus_conflict(
                    900,
                    None,
                    false,
                    Some(900),
                    Some(focused_window),
                    true
                ),
                WindowFocusConflict::SiblingWindowFocused {
                    target_window_id: 900,
                    sibling_window_id: focused_window,
                }
            );
        }
    }

    #[test]
    fn a_sheet_takes_the_minimized_state_of_the_window_it_is_attached_to() {
        let records = [
            ax_window(700, Some(false)),
            ax_window(701, Some(true)),
            ax_window(702, None),
        ];
        // A listed window keeps its own state.
        assert_eq!(
            super::target_minimized_state(&records, 701, None),
            Some(true)
        );
        // A sheet (not in AXWindows) reads its parent; unknown stays unknown.
        assert_eq!(
            super::target_minimized_state(&records, 900, Some(700)),
            Some(false)
        );
        assert_eq!(
            super::target_minimized_state(&records, 900, Some(701)),
            Some(true)
        );
        assert_eq!(
            super::target_minimized_state(&records, 900, Some(702)),
            None
        );
        assert_eq!(super::target_minimized_state(&records, 900, None), None);
        assert_eq!(
            super::target_minimized_state(&records, 900, Some(999)),
            None
        );
    }
}
