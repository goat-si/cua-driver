//! AX action dispatch — the preferred click/interaction path for indexed elements.

use crate::ax::bindings::*;
use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFTypeRef};

const MAX_SELECTION_ANCESTORS: usize = 8;

fn is_selectable_container_role(role: &str) -> bool {
    matches!(role, "AXRow" | "AXCell" | "AXListItem" | "AXImage")
}

/// Select the nearest list-like element at or above `element_ptr` and confirm
/// the write through `AXSelected` read-back.
///
/// Finder and other AppKit collection views commonly expose an item's label as
/// an actionable child (`AXTextField`) while the selectable object is its
/// parent `AXRow`. Neither object necessarily advertises `AXPress`, and Finder
/// can return `kAXErrorCannotComplete` for a press on the row. A pointer click
/// selects that row, so the AX equivalent is to set the row's `AXSelected`
/// attribute rather than treating the failed press as terminal.
///
/// Finder icon views expose each selectable file directly as an `AXImage` with
/// an `AXSelected` attribute, so that role is included alongside the standard
/// row-like containers. The fallback remains bounded and requires a successful
/// `AXSelected=true` read-back, so an arbitrary failed image/button press cannot
/// become a claimed success.
pub fn select_nearest_container(element_ptr: usize) -> Option<String> {
    let mut current = element_ptr as AXUIElementRef;
    let mut owns_current = false;

    for _ in 0..MAX_SELECTION_ANCESTORS {
        let role = unsafe { copy_string_attr(current, "AXRole") }.unwrap_or_default();
        if is_selectable_container_role(&role)
            && unsafe { copy_bool_attr(current, "AXSelected") }.is_some()
        {
            let err = unsafe { set_bool_attr_true(current, "AXSelected") };
            if err == kAXErrorSuccess
                && unsafe { copy_bool_attr(current, "AXSelected") } == Some(true)
            {
                if owns_current {
                    unsafe { CFRelease(current as CFTypeRef) };
                }
                return Some(role);
            }
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

/// Read the selection state of the nearest collection-like element without
/// mutating it. This is used to verify a coordinate fallback when AppKit
/// exposes `AXSelected` but refuses to set it directly (notably Finder icon
/// views).
pub fn nearest_container_selection_state(element_ptr: usize) -> Option<(String, bool)> {
    let mut current = element_ptr as AXUIElementRef;
    let mut owns_current = false;

    for _ in 0..MAX_SELECTION_ANCESTORS {
        let role = unsafe { copy_string_attr(current, "AXRole") }.unwrap_or_default();
        if is_selectable_container_role(&role) {
            if let Some(selected) = unsafe { copy_bool_attr(current, "AXSelected") } {
                if owns_current {
                    unsafe { CFRelease(current as CFTypeRef) };
                }
                return Some((role, selected));
            }
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

/// A retained selection context for proving the settled result of a modified
/// pointer click.
///
/// Reading only the target's `AXSelected` bit is insufficient for multi-select:
/// AppKit can expose a transient target transition while it is still resolving
/// the synthetic gesture, and a modifier-less outcome can replace the prior
/// selection with the target. Keep the target and every selected sibling alive
/// across delivery so callers can require both the intended target transition
/// and preservation of the pre-existing selection.
pub struct SelectionReadback {
    role: String,
    target: AXUIElementRef,
    peer_model_observed: bool,
    previously_selected_peers: Vec<AXUIElementRef>,
}

impl SelectionReadback {
    pub fn role(&self) -> &str {
        &self.role
    }

    pub fn observe(&self) -> Option<(bool, bool)> {
        let target_selected = unsafe { copy_bool_attr(self.target, "AXSelected") }?;
        let peers_preserved = self.peer_model_observed
            && self
                .previously_selected_peers
                .iter()
                .all(|&peer| unsafe { copy_bool_attr(peer, "AXSelected") } == Some(true));
        Some((target_selected, peers_preserved))
    }
}

impl Drop for SelectionReadback {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.target as CFTypeRef);
            for peer in self.previously_selected_peers.drain(..) {
                CFRelease(peer as CFTypeRef);
            }
        }
    }
}

/// Capture the nearest selectable container and its currently-selected peers.
/// Returns `None` when the platform does not expose a readable selection model;
/// callers must then leave the action unverifiable instead of inventing proof.
pub fn capture_nearest_container_selection(element_ptr: usize) -> Option<SelectionReadback> {
    let mut current = element_ptr as AXUIElementRef;
    let mut owns_current = false;

    for _ in 0..MAX_SELECTION_ANCESTORS {
        let role = unsafe { copy_string_attr(current, "AXRole") }.unwrap_or_default();
        if is_selectable_container_role(&role)
            && unsafe { copy_bool_attr(current, "AXSelected") }.is_some()
        {
            if !owns_current {
                unsafe { CFRetain(current as CFTypeRef) };
            }
            let target = current;
            let parent = unsafe { copy_element_attr(target, "AXParent") };
            let mut peers = Vec::new();
            let mut peer_model_observed = false;
            if let Some(parent) = parent {
                for child in unsafe { copy_children(parent) } {
                    let is_target =
                        unsafe { CFEqual(child as CFTypeRef, target as CFTypeRef) != 0 };
                    peer_model_observed |= is_target;
                    if !is_target && unsafe { copy_bool_attr(child, "AXSelected") } == Some(true) {
                        peers.push(child);
                    } else {
                        unsafe { CFRelease(child as CFTypeRef) };
                    }
                }
                unsafe { CFRelease(parent as CFTypeRef) };
            }
            return Some(SelectionReadback {
                role,
                target,
                peer_model_observed,
                previously_selected_peers: peers,
            });
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

fn ensure_ax_enabled(enabled: Option<bool>, action: &str) -> anyhow::Result<()> {
    if enabled == Some(false) {
        anyhow::bail!(
            "refusing {action}: the target reports AXEnabled=false. \
             Retry this action with delivery_mode:\"foreground\" or call bring_to_front first"
        );
    }
    Ok(())
}

/// Refuse AX actions that macOS reports as disabled.
///
/// This must be checked immediately before dispatch rather than trusting the
/// cached snapshot value: foreground delivery can make a menu item live after
/// it was resolved, while backgrounding can disable it in the other direction.
pub fn ensure_ax_action_enabled(element_ptr: usize, action: &str) -> anyhow::Result<()> {
    let enabled = unsafe { copy_bool_attr(element_ptr as AXUIElementRef, "AXEnabled") };
    ensure_ax_enabled(enabled, action)
}

/// Perform an AX action on a cached element.
pub fn perform_ax_action(element_ptr: usize, action: &str) -> anyhow::Result<()> {
    let ax_action = map_action(action)?;
    ensure_ax_action_enabled(element_ptr, ax_action)?;
    let err = unsafe { perform_action(element_ptr as AXUIElementRef, ax_action) };

    if err == kAXErrorSuccess {
        Ok(())
    } else {
        anyhow::bail!("AXUIElementPerformAction({action}) failed with error {err}")
    }
}

/// The AX action named by a tool's `action` argument. Accepts the short
/// names (`press`, `show_menu`, ...) and the tree's spelling (`showalternateui`,
/// `AXShowAlternateUI`). An unknown name is an error: falling back to AXPress
/// would press a control the caller meant only to reveal.
pub fn map_action(action: &str) -> anyhow::Result<&'static str> {
    let key = action_key(action);
    Ok(match key.as_str() {
        "press" | "click" => "AXPress",
        "showmenu" | "rightclick" => "AXShowMenu",
        "pick" => "AXPick",
        "confirm" => "AXConfirm",
        "cancel" => "AXCancel",
        "open" => "AXOpen",
        "showalternateui" => "AXShowAlternateUI",
        "showdefaultui" => "AXShowDefaultUI",
        _ => anyhow::bail!("unknown action \"{action}\": use an action the target element lists"),
    })
}

fn action_key(action: &str) -> String {
    let key: String = action
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .collect::<String>()
        .to_lowercase();
    key.strip_prefix("ax").unwrap_or(&key).to_owned()
}

/// Whether `requested` names an action the target currently exposes. Tree rows
/// omit `AX` and lowercase names; matching that spelling makes custom and
/// increment/decrement actions callable without guessing their capitalization.
pub fn advertised_action<'a>(requested: &str, advertised: &'a [String]) -> Option<&'a str> {
    let key = action_key(requested);
    let shown = requested.to_lowercase();
    advertised
        .iter()
        .find(|candidate| action_key(candidate) == key || displayed_action(candidate) == shown)
        .map(String::as_str)
}

/// An action name as a tree row shows it: no `AX` prefix, lower case, and
/// escaped so a custom name cannot split the row. Agents copy this spelling.
pub fn displayed_action(action: &str) -> String {
    action
        .strip_prefix("AX")
        .unwrap_or(action)
        .to_lowercase()
        .chars()
        .flat_map(char::escape_default)
        .collect()
}

pub fn background_action_may_front(action: &str) -> bool {
    action_key(action) == "raise"
}

#[derive(Debug)]
pub struct ElementActionRefusal {
    pub code: &'static str,
    message: String,
}

impl std::fmt::Display for ElementActionRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ElementActionRefusal {}

fn element_action_refusal(code: &'static str, message: String) -> anyhow::Error {
    anyhow::Error::new(ElementActionRefusal { code, message })
}

pub fn element_action_refusal_code(error: &anyhow::Error) -> Option<&'static str> {
    error
        .downcast_ref::<ElementActionRefusal>()
        .map(|error| error.code)
}

/// Resolve an AX action immediately before dispatch. The eight aliases remain
/// available for AppKit's hollow-press and selection fallbacks; every other
/// action must be live-advertised by the exact target. Background never raises.
pub fn resolve_element_action<'a>(
    requested: &str,
    advertised: &'a [String],
    foreground: bool,
) -> anyhow::Result<&'a str> {
    if !foreground && background_action_may_front(requested) {
        return Err(element_action_refusal(
            "background_unavailable",
            format!(
                "refusing {requested}: it fronts the target window. Retry with \
             delivery_mode:\"foreground\" only if the user allows it"
            ),
        ));
    }
    if let Some(action) = advertised_action(requested, advertised) {
        return Ok(action);
    }
    match map_action(requested)
        .map_err(|error| element_action_refusal("invalid_arguments", error.to_string()))?
    {
        "AXPress" => Ok("AXPress"),
        "AXShowMenu" => Ok("AXShowMenu"),
        "AXPick" => Ok("AXPick"),
        "AXConfirm" => Ok("AXConfirm"),
        "AXCancel" => Ok("AXCancel"),
        "AXOpen" => Ok("AXOpen"),
        "AXShowAlternateUI" => Ok("AXShowAlternateUI"),
        "AXShowDefaultUI" => Ok("AXShowDefaultUI"),
        _ => unreachable!("map_action returns only static aliases"),
    }
}

/// Set AXFocused=true on an element (for pre-focusing before key press).
pub fn focus_element(element_ptr: usize) -> anyhow::Result<()> {
    let err = unsafe { set_bool_attr_true(element_ptr as AXUIElementRef, "AXFocused") };
    if err == kAXErrorSuccess {
        Ok(())
    } else {
        // Focus errors are often benign (element doesn't support focus).
        tracing::warn!("AXSetAttribute(AXFocused) returned {err}");
        Ok(())
    }
}

/// Report whether `element_ptr` is the application's currently focused element.
///
/// This is a read-only confirmation for the foreground typing rung: an
/// `AXFocused` write can be accepted by the element and then immediately
/// clobbered when AppKit installs the window's remembered first responder, so
/// "the write returned success" is not evidence that focus stuck. Identity is
/// compared with `CFEqual` because the app hands back a fresh `AXUIElementRef`
/// for the same underlying element.
///
/// A `false` return is deliberately conservative: an app whose
/// `AXFocusedUIElement` is unreadable reports not-focused, which at worst costs
/// one extra re-apply.
pub fn is_element_focused(pid: i32, element_ptr: usize) -> bool {
    unsafe {
        let Some(focused) = crate::ax::bindings::focused_element_of_pid(pid) else {
            return false;
        };
        let same = CFEqual(focused as CFTypeRef, element_ptr as CFTypeRef) != 0;
        CFRelease(focused as CFTypeRef);
        same
    }
}

/// Report whether `element_ptr` holds keyboard focus, by either readback.
///
/// The application's `AXFocusedUIElement` is the stronger signal, but for
/// web content (the Tauri fixture) in a background window it does not name
/// the DOM control even after the control took focus, so a targeted
/// background `press_key` was refused although the key would have landed.
/// Accept the control's own `AXFocused = true` too. A write that AppKit
/// clobbered reads back `false` either way.
pub fn element_has_keyboard_focus(pid: i32, element_ptr: usize) -> bool {
    is_element_focused(pid, element_ptr)
        || unsafe { copy_bool_attr(element_ptr as AXUIElementRef, "AXFocused") } == Some(true)
}

const MAX_WEB_ANCESTORS: usize = 40;

/// Pure role/ancestry check: true when a control is a focusable text input
/// inside a web-content subtree (`AXWebArea`) or a Safari window, where
/// `AXFocused = true` alone can be ignored by the renderer until a window-local
/// click places the caret.
pub fn is_web_or_safari_focus_fallback_candidate(
    role: Option<&str>,
    has_web_area_ancestor: bool,
    is_safari_bundle: bool,
) -> bool {
    let is_text_role = matches!(
        role,
        Some("AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox")
    );
    is_text_role && (has_web_area_ancestor || is_safari_bundle)
}

fn is_safari_bundle_id(bundle_id: &str) -> bool {
    matches!(
        bundle_id,
        "com.apple.Safari" | "com.apple.SafariTechnologyPreview"
    )
}

unsafe fn element_has_web_area_ancestor(element: AXUIElementRef) -> bool {
    let mut current = element;
    let mut owns_current = false;
    let mut found = false;
    for _ in 0..MAX_WEB_ANCESTORS {
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXWebArea") => {
                found = true;
                break;
            }
            Some("AXWindow" | "AXApplication") | None => break,
            _ => {}
        }
        let parent = copy_element_attr(current, "AXParent");
        if owns_current {
            CFRelease(current as CFTypeRef);
        }
        match parent {
            Some(parent) => {
                current = parent;
                owns_current = true;
            }
            None => {
                owns_current = false;
                break;
            }
        }
    }
    if owns_current {
        CFRelease(current as CFTypeRef);
    }
    found
}

/// Whether `element_ptr` is an editable control in a web area or Safari window
/// that is eligible for a single window-local pointer click when `AXFocused`
/// write alone does not stick.
pub fn element_supports_pointer_focus_fallback(
    pid: i32,
    element_ptr: usize,
    window_id: Option<u32>,
) -> bool {
    let Some(wid) = window_id else {
        return false;
    };
    let element = element_ptr as AXUIElementRef;
    if unsafe { crate::ax::exact_target::element_window_id(element) } != Some(wid) {
        return false;
    }
    let role = unsafe { copy_string_attr(element, "AXRole") };
    let in_web_area = unsafe { element_has_web_area_ancestor(element) };
    let is_safari = crate::apps::bundle_id_for_pid(pid)
        .as_deref()
        .is_some_and(is_safari_bundle_id);
    is_web_or_safari_focus_fallback_candidate(role.as_deref(), in_web_area, is_safari)
}

/// Confirm keyboard focus on `element_ptr`, requiring that both the element
/// and any PID-focused element belong to `window_id` when a window is specified.
pub fn element_has_keyboard_focus_in_window(
    pid: i32,
    window_id: Option<u32>,
    element_ptr: usize,
) -> bool {
    let Some(wid) = window_id else {
        return element_has_keyboard_focus(pid, element_ptr);
    };
    let element = element_ptr as AXUIElementRef;
    if unsafe { crate::ax::exact_target::element_window_id(element) } != Some(wid) {
        return false;
    }
    unsafe {
        if let Some(focused) = crate::ax::exact_target::focused_element_in_window(pid, wid) {
            let same = CFEqual(focused as CFTypeRef, element_ptr as CFTypeRef) != 0;
            CFRelease(focused as CFTypeRef);
            if same {
                return true;
            }
        }
        copy_bool_attr(element, "AXFocused") == Some(true)
    }
}

/// Attempt a single bounded window-local pointer click at `element_ptr`'s
/// screen center inside `window_id` to establish DOM focus when `AXFocused=true`
/// is ignored by Safari/WebKit/`AXWebArea` text controls.
pub fn click_element_center_for_focus(
    pid: i32,
    window_id: u32,
    element_ptr: usize,
    foreground: bool,
) -> anyhow::Result<()> {
    let element = element_ptr as AXUIElementRef;
    if unsafe { crate::ax::exact_target::element_window_id(element) } != Some(window_id) {
        anyhow::bail!(
            "refusing pointer focus fallback: element does not belong to window {window_id}"
        );
    }
    let (screen_x, screen_y) = unsafe { element_screen_center(element) }
        .ok_or_else(|| anyhow::anyhow!("element has no readable screen center"))?;
    let bounds = crate::windows::window_bounds_by_id(window_id)
        .ok_or_else(|| anyhow::anyhow!("window {window_id} has no readable bounds"))?;
    if bounds.width <= 0.0 || bounds.height <= 0.0 {
        anyhow::bail!("window {window_id} has empty bounds");
    }
    let wx = screen_x - bounds.x;
    let wy = screen_y - bounds.y;
    if wx < 0.0 || wx >= bounds.width || wy < 0.0 || wy >= bounds.height {
        anyhow::bail!(
            "element center ({screen_x:.1}, {screen_y:.1}) lies outside window {window_id}"
        );
    }
    crate::input::mouse::click_at_xy_with_window_local(
        pid,
        screen_x,
        screen_y,
        wx,
        wy,
        window_id,
        1,
        &[],
        crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn action_names_accept_the_tree_spelling_and_refuse_unknown_ones() {
        use super::map_action;
        assert_eq!(map_action("press").unwrap(), "AXPress");
        assert_eq!(
            map_action("show_alternate_ui").unwrap(),
            "AXShowAlternateUI"
        );
        assert_eq!(map_action("showalternateui").unwrap(), "AXShowAlternateUI");
        assert_eq!(map_action("AXShowDefaultUI").unwrap(), "AXShowDefaultUI");
        assert_eq!(map_action("right_click").unwrap(), "AXShowMenu");
        // An unknown name must not fall back to pressing the control.
        assert!(map_action("hover").is_err());
    }

    #[test]
    fn live_advertised_actions_accept_custom_and_increment_names() {
        let advertised = vec![
            "AXIncrement".into(),
            "Name:Reveal \"details\"\nTarget:inspector".into(),
        ];
        assert_eq!(
            resolve_element_action("decrement", &["AXDecrement".into()], false).unwrap(),
            "AXDecrement"
        );
        assert_eq!(
            resolve_element_action("AXIncrement", &advertised, false).unwrap(),
            "AXIncrement"
        );
        assert_eq!(
            resolve_element_action(
                "name:reveal \"details\"\ntarget:inspector",
                &advertised,
                false
            )
            .unwrap(),
            advertised[1]
        );
        assert!(resolve_element_action("AXDelete", &advertised, false).is_err());
        // The escaped spelling a tree row shows also names the action.
        let korean = vec!["Name:보관\nTarget:list".to_string()];
        let shown = displayed_action(&korean[0]);
        assert!(shown.contains("\\n"));
        assert_eq!(
            resolve_element_action(&shown, &korean, false).unwrap(),
            korean[0]
        );
        assert_eq!(
            resolve_element_action(&displayed_action(&advertised[1]), &advertised, false).unwrap(),
            advertised[1]
        );
        assert!(resolve_element_action("raise", &["AXRaise".into()], false).is_err());
        assert_eq!(
            resolve_element_action("AXRaise", &["AXRaise".into()], true).unwrap(),
            "AXRaise"
        );
        assert_eq!(
            resolve_element_action("press", &[], false).unwrap(),
            "AXPress"
        );
    }

    use super::*;

    #[test]
    fn disabled_elements_are_refused_before_dispatch() {
        let error = ensure_ax_enabled(Some(false), "AXPick").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("AXEnabled=false"));
        assert!(message.contains("delivery_mode:\"foreground\""));
        assert!(message.contains("bring_to_front"));
    }

    #[test]
    fn enabled_or_unreported_state_is_allowed() {
        assert!(ensure_ax_enabled(Some(true), "AXPress").is_ok());
        assert!(ensure_ax_enabled(None, "AXPress").is_ok());
    }

    #[test]
    fn selection_fallback_is_limited_to_collection_item_roles() {
        for role in ["AXRow", "AXCell", "AXListItem", "AXImage"] {
            assert!(is_selectable_container_role(role), "{role}");
        }
        for role in ["AXButton", "AXTextField", "AXWindow", "AXOutline"] {
            assert!(!is_selectable_container_role(role), "{role}");
        }
    }

    #[test]
    fn web_or_safari_focus_fallback_requires_text_role_and_web_or_safari_context() {
        for role in ["AXTextField", "AXTextArea", "AXSearchField", "AXComboBox"] {
            assert!(is_web_or_safari_focus_fallback_candidate(
                Some(role),
                true,
                false
            ));
            assert!(is_web_or_safari_focus_fallback_candidate(
                Some(role),
                false,
                true
            ));
            assert!(!is_web_or_safari_focus_fallback_candidate(
                Some(role),
                false,
                false
            ));
        }
        for non_text in [
            Some("AXButton"),
            Some("AXGroup"),
            Some("AXStaticText"),
            None,
        ] {
            assert!(!is_web_or_safari_focus_fallback_candidate(
                non_text, true, true
            ));
        }
    }
}
