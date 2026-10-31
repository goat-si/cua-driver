//! AX action dispatch — the preferred click/interaction path for indexed elements.

use crate::ax::bindings::*;
use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFTypeRef};

const MAX_SELECTION_ANCESTORS: usize = 8;

pub(crate) fn climb_limit(_start_role: Option<&str>) -> usize {
    MAX_SELECTION_ANCESTORS
}

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

    let mut start_role = Some(unsafe { copy_string_attr(current, "AXRole") });
    let limit = climb_limit(start_role.as_ref().and_then(|role| role.as_deref()));
    for _ in 0..limit {
        let role = start_role
            .take()
            .unwrap_or_else(|| unsafe { copy_string_attr(current, "AXRole") })
            .unwrap_or_default();
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

    let mut start_role = Some(unsafe { copy_string_attr(current, "AXRole") });
    let limit = climb_limit(start_role.as_ref().and_then(|role| role.as_deref()));
    for _ in 0..limit {
        let role = start_role
            .take()
            .unwrap_or_else(|| unsafe { copy_string_attr(current, "AXRole") })
            .unwrap_or_default();
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

    let mut start_role = Some(unsafe { copy_string_attr(current, "AXRole") });
    let limit = climb_limit(start_role.as_ref().and_then(|role| role.as_deref()));
    for _ in 0..limit {
        let role = start_role
            .take()
            .unwrap_or_else(|| unsafe { copy_string_attr(current, "AXRole") })
            .unwrap_or_default();
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

/// The error for a target that reads AXEnabled=false; the action is not sent.
/// In the background the check runs before key priming, any selection write
/// and the action, so the click has changed nothing and the error is a coded
/// refusal: an uncoded error reads as a write that may have landed. Apps
/// often disable controls of a window that is not key (Calendar's toolbar
/// view switcher), so the code names the foreground retry. In the foreground
/// the click may already have activated and raised the window, so the error
/// stays uncoded and claims nothing beyond the unsent action.
fn ensure_ax_enabled(enabled: Option<bool>, action: &str, foreground: bool) -> anyhow::Result<()> {
    if enabled == Some(false) {
        return Err(disabled_action_refusal(action, foreground));
    }
    Ok(())
}

/// The error [`ensure_ax_action_enabled`] returns for a disabled target.
pub(crate) fn disabled_action_refusal(action: &str, foreground: bool) -> anyhow::Error {
    if foreground {
        anyhow::anyhow!(
            "refusing {action}: the target reports AXEnabled=false, so {action} was not sent. \
             Read the window again and act on it once it reads enabled"
        )
    } else {
        element_action_refusal(
            "background_unavailable",
            format!(
                "refusing {action}: the target reports AXEnabled=false, so {action} was not sent. \
                 Retry this action with delivery_mode:\"foreground\" or call bring_to_front first"
            ),
        )
    }
}

/// Refuse AX actions that macOS reports as disabled.
///
/// This must be checked immediately before dispatch rather than trusting the
/// cached snapshot value: foreground delivery can make a menu item live after
/// it was resolved, while backgrounding can disable it in the other direction.
pub fn ensure_ax_action_enabled(
    element_ptr: usize,
    action: &str,
    foreground: bool,
) -> anyhow::Result<()> {
    let enabled = unsafe { copy_bool_attr(element_ptr as AXUIElementRef, "AXEnabled") };
    ensure_ax_enabled(enabled, action, foreground)
}

/// Perform an AX action on a cached element.
pub fn perform_ax_action(element_ptr: usize, action: &str) -> anyhow::Result<()> {
    let ax_action = map_action(action)?;
    ensure_ax_action_enabled(element_ptr, ax_action, false)?;
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

/// Extract the human-readable name from AppKit's NSAccessibilityCustomAction
/// string. The complete raw value is still required when performing the action.
pub fn custom_action_display_name(raw: &str) -> Option<&str> {
    let (name, rest) = raw.split_once('\n')?;
    let name = name.strip_prefix("Name:")?.trim();
    let (target, selector) = rest.split_once('\n')?;
    let target = target.strip_prefix("Target:")?;
    let selector = selector.strip_prefix("Selector:")?;
    if name.is_empty() || target.is_empty() || selector.is_empty() || selector.contains('\n') {
        return None;
    }
    Some(name)
}

/// Whether `requested` names an action the target currently exposes. Tree rows
/// omit `AX` and lowercase names; matching that spelling makes custom and
/// increment/decrement actions callable without guessing their capitalization.
enum AdvertisedActionMatch<'a> {
    Matched(&'a str),
    Ambiguous,
    NotFound,
}

fn match_advertised_action<'a>(
    requested: &str,
    advertised: &'a [String],
) -> AdvertisedActionMatch<'a> {
    let key = action_key(requested);
    let shown = requested.to_lowercase();
    if let Some(action) = advertised.iter().find(|candidate| {
        action_key(candidate) == key || legacy_displayed_action(candidate) == shown
    }) {
        return AdvertisedActionMatch::Matched(action.as_str());
    }

    let mut exact = advertised.iter().filter(|candidate| {
        custom_action_display_name(candidate).is_some_and(|name| name == requested)
    });
    if let Some(first) = exact.next() {
        return if exact.next().is_some() {
            AdvertisedActionMatch::Ambiguous
        } else {
            AdvertisedActionMatch::Matched(first.as_str())
        };
    }

    let mut displayed = advertised.iter().filter(|candidate| {
        custom_action_display_name(candidate).is_some_and(|name| displayed_action(name) == shown)
    });
    if let Some(first) = displayed.next() {
        return if displayed.next().is_some() {
            AdvertisedActionMatch::Ambiguous
        } else {
            AdvertisedActionMatch::Matched(first.as_str())
        };
    }

    let mut normalized = advertised.iter().filter(|candidate| {
        custom_action_display_name(candidate).is_some_and(|name| action_key(name) == key)
    });
    if let Some(first) = normalized.next() {
        return if normalized.next().is_some() {
            AdvertisedActionMatch::Ambiguous
        } else {
            AdvertisedActionMatch::Matched(first.as_str())
        };
    }
    AdvertisedActionMatch::NotFound
}

pub fn advertised_action<'a>(requested: &str, advertised: &'a [String]) -> Option<&'a str> {
    match match_advertised_action(requested, advertised) {
        AdvertisedActionMatch::Matched(action) => Some(action),
        AdvertisedActionMatch::Ambiguous | AdvertisedActionMatch::NotFound => None,
    }
}

/// An action name as a tree row shows it: no `AX` prefix, lower case, with
/// control characters, quotes and backslashes escaped so a custom name cannot
/// split or confuse the row. Other text, Korean included, is shown as is.
/// Agents copy this spelling.
pub fn displayed_action(action: &str) -> String {
    let action = custom_action_display_name(action).unwrap_or(action);
    display_action_name(action)
}

/// The pre-custom-name tree spelling, retained as an input alias so older
/// copied markdown rows still resolve to the original action.
fn legacy_displayed_action(action: &str) -> String {
    display_action_name(action)
}

fn display_action_name(action: &str) -> String {
    let mut shown = String::new();
    for c in action
        .strip_prefix("AX")
        .unwrap_or(action)
        .to_lowercase()
        .chars()
    {
        if c.is_control() || matches!(c, '"' | '\'' | '\\') {
            shown.extend(c.escape_default());
        } else {
            shown.push(c);
        }
    }
    shown
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
    match match_advertised_action(requested, advertised) {
        AdvertisedActionMatch::Matched(action) => return Ok(action),
        AdvertisedActionMatch::Ambiguous => {
            return Err(element_action_refusal(
                "ambiguous",
                format!(
                    "refusing {requested}: multiple custom actions share this name or normalized key"
                ),
            ));
        }
        AdvertisedActionMatch::NotFound => {}
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
        let korean = vec!["Name:보관\nTarget:list\nSelector:perform:".to_string()];
        let shown = displayed_action(&korean[0]);
        // Only what could split or confuse a row is escaped: Korean and other
        // non-ASCII text stays readable, so an agent can copy it back.
        assert_eq!(shown, "보관");
        assert_eq!(
            displayed_action("Say \"hi\"\\now\t✓"),
            "say \\\"hi\\\"\\\\now\\t✓"
        );
        let quoted = vec!["Say \"hi\"\\now\t✓".to_string()];
        assert_eq!(
            resolve_element_action(&displayed_action(&quoted[0]), &quoted, false).unwrap(),
            quoted[0]
        );
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
        // `focus` is a pixel-only rung; on an element it is refused before
        // anything is sent, as an argument error.
        let focus = resolve_element_action("focus", &["AXPress".into()], false).unwrap_err();
        assert_eq!(
            element_action_refusal_code(&focus),
            Some("invalid_arguments")
        );
    }

    #[test]
    fn wave_400b_custom_actions_display_and_match_by_their_clean_name() {
        let raw = "Name:Move Next\nTarget:0x0\nSelector:(null)";
        assert_eq!(custom_action_display_name(raw), Some("Move Next"));
        assert_eq!(displayed_action(raw), "move next");

        let advertised = vec![raw.to_owned()];
        assert_eq!(
            advertised_action("mOvE nExT", &advertised),
            Some(raw),
            "matching must return the raw value AXUIElementPerformAction requires"
        );
        for malformed in [
            "Name:Missing selector\nTarget:0x0",
            "Name:\nTarget:0x0\nSelector:(null)",
            "Move Next\nTarget:0x0\nSelector:(null)",
            "Name:Extra\nTarget:0x0\nSelector:(null)\nTrailing:value",
        ] {
            assert_eq!(custom_action_display_name(malformed), None, "{malformed:?}");
        }
    }

    #[test]
    fn wave_400b_builtin_actions_win_over_a_custom_name_collision() {
        let custom = "Name:Press\nTarget:0x0\nSelector:(null)".to_owned();
        let builtin = "AXPress".to_owned();
        for advertised in [
            vec![custom.clone(), builtin.clone()],
            vec![builtin.clone(), custom.clone()],
        ] {
            assert_eq!(advertised_action("press", &advertised), Some("AXPress"));
            assert_eq!(advertised_action("AXPress", &advertised), Some("AXPress"));
        }
    }

    #[test]
    fn wave_400b_custom_matching_rejects_ambiguity_and_accepts_legacy_alias() {
        let dash = "Name:Move-Next\nTarget:0x1\nSelector:first:";
        let underscore = "Name:Move_Next\nTarget:0x2\nSelector:second:";
        for advertised in [
            vec![dash.to_owned(), underscore.to_owned()],
            vec![underscore.to_owned(), dash.to_owned()],
        ] {
            assert_eq!(advertised_action("movenext", &advertised), None);
            assert_eq!(
                advertised_action("Move_Next", &advertised),
                Some(underscore),
                "an exact displayed spelling wins before normalized matching"
            );
        }

        let same_name = vec![
            "Name:Move Next\nTarget:0x1\nSelector:first:".to_owned(),
            "Name:Move Next\nTarget:0x2\nSelector:second:".to_owned(),
        ];
        assert_eq!(advertised_action("Move Next", &same_name), None);

        let raw = "Name:Move Next\nTarget:0x0\nSelector:(null)";
        let advertised = vec![raw.to_owned()];
        let legacy = "name:move next\\ntarget:0x0\\nselector:(null)";
        assert_eq!(advertised_action(legacy, &advertised), Some(raw));
        assert_eq!(advertised_action(raw, &advertised), Some(raw));
        assert_eq!(advertised_action("Move Next", &advertised), Some(raw));
    }

    #[test]
    fn wave_400b_resolve_refuses_ambiguous_custom_names_before_builtin_fallback() {
        for requested in ["Open", "Press", "Cancel"] {
            let advertised = vec![
                format!("Name:{requested}\nTarget:0x1\nSelector:first:"),
                format!("Name:{requested}\nTarget:0x2\nSelector:second:"),
            ];
            let error = resolve_element_action(requested, &advertised, false).unwrap_err();
            assert_eq!(element_action_refusal_code(&error), Some("ambiguous"));
            assert!(error.to_string().contains("multiple custom actions"));
        }

        let advertised = vec![
            "Name:Press\nTarget:0x1\nSelector:first:".to_owned(),
            "AXPress".to_owned(),
        ];
        assert_eq!(
            resolve_element_action("press", &advertised, false).unwrap(),
            "AXPress",
            "an advertised builtin still wins"
        );
        assert_eq!(
            resolve_element_action("press", &[], false).unwrap(),
            "AXPress",
            "the existing hollow-press fallback remains"
        );
    }

    use super::*;

    #[test]
    fn disabled_elements_are_refused_before_dispatch() {
        let background = ensure_ax_enabled(Some(false), "AXPick", false).unwrap_err();
        let message = background.to_string();
        assert!(message.contains("AXEnabled=false"));
        assert!(message.contains("AXPick was not sent"), "{message}");
        assert!(message.contains("delivery_mode:\"foreground\""));
        assert!(message.contains("bring_to_front"));
        assert_eq!(
            element_action_refusal_code(&background),
            Some("background_unavailable")
        );

        let foreground = ensure_ax_enabled(Some(false), "AXPress", true).unwrap_err();
        let message = foreground.to_string();
        assert!(message.contains("AXEnabled=false"));
        assert!(message.contains("AXPress was not sent"), "{message}");
        assert!(!message.contains("delivery_mode"), "{message}");
        assert_eq!(element_action_refusal_code(&foreground), None);
    }

    #[test]
    fn enabled_or_unreported_state_is_allowed() {
        for foreground in [false, true] {
            assert!(ensure_ax_enabled(Some(true), "AXPress", foreground).is_ok());
            assert!(ensure_ax_enabled(None, "AXPress", foreground).is_ok());
        }
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

#[cfg(test)]
mod climb_limit_tests {
    use super::climb_limit;

    #[test]
    fn baseline_label_children_use_eight_steps() {
        for role in [Some("AXStaticText"), Some("AXTextField")] {
            assert_eq!(climb_limit(role), 8);
        }
    }
}
