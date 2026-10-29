use crate::goat::action_latency as latency;

use async_trait::async_trait;
use core_foundation::{
    base::{CFEqual, CFGetTypeID, CFRelease, CFTypeRef, TCFType},
    string::CFString,
};
use cua_driver_core::{
    action_record::{
        ActionEffect, ActionEvidence, ActionExecutionRecord, ActionTransport, ActualDelivery,
        EvidenceKind, RequestedDelivery,
    },
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::ax::bindings::{
    ax_get_window_id, copy_action_names, copy_children_checked, copy_element_attr,
    copy_string_attr, kAXErrorSuccess, perform_action, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementRef, AXUIElementSetMessagingTimeout,
};

use super::ToolState;

pub struct RightClickTool {
    state: Arc<ToolState>,
}

impl RightClickTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "right_click".into(),
        description:
            "Right-click in a target pid. Give exactly one of `element_token` (from the latest \
                get_window_state of that window; performs AXShowMenu, works on backgrounded or hidden windows, \
                moves no cursor or focus, and carries its own pid) or `x`, `y` (window screenshot pixels, sent \
                as a right mouse down/up pair to the pid, which is then required). `modifier` forces the pixel \
                path, because AX actions do not carry modifier keys."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": [],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid": { "type": "integer", "description": "Target process ID. Optional with element_token (the token carries the pid)." },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "window_id": {
                    "type": "integer",
                    "description": "CGWindowID. Omit when element_token is supplied (the token carries it)."
                },
                "x": {
                    "type": "number",
                    "description": "X in window-local screenshot pixels. Must be provided together with y."
                },
                "y": {
                    "type": "number",
                    "description": "Y in window-local screenshot pixels. Must be provided together with x."
                },
                "modifier": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Modifier keys held during the right-click: cmd/shift/option/ctrl. Pixel path only."
                },
                "coordinate_space": super::window_points::coordinate_space_schema(),
                "window_basis": super::window_points::window_basis_schema(),
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema()
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
impl Tool for RightClickTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let space = match super::window_points::point_space(&args) {
            Ok(space) => space,
            Err(refusal) => return refusal,
        };
        let point = args
            .get("x")
            .and_then(Value::as_f64)
            .zip(args.get("y").and_then(Value::as_f64));
        super::window_points::annotate(self.invoke_in(args, space).await, space, point)
    }
}

impl RightClickTool {
    async fn invoke_in(&self, args: Value, space: super::window_points::PointSpace) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        // delivery_mode: foreground briefly fronts the window before the pixel
        // right-click (the explicit last resort for surfaces that drop
        // background CGEvents), via the same skylight assist click uses. The AX
        // (AXShowMenu) path is background-by-design and untouched.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);

        let window_id_arg = args.opt_u64("window_id");
        let resolved = match latency::resolve(|| self.state.snapshots.resolve(pid, &args)) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, element_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        let x = args.opt_f64("x");
        let y = args.opt_f64("y");
        let has_xy = x.is_some() && y.is_some();
        let partial_xy = x.is_some() != y.is_some();
        let modifiers: Vec<String> = args.str_array("modifier");

        if partial_xy {
            return super::invalid_arguments("Provide both x and y together, not just one.");
        }
        if element_index.is_some() && has_xy {
            return super::invalid_arguments("Provide either element_token or (x, y), not both.");
        }
        if element_index.is_none() && !has_xy {
            return super::invalid_arguments(
                "Provide element_token or (x, y) to address the right-click target.",
            );
        }

        // ── AX element path ──────────────────────────────────────────────────
        if let (Some(idx), Some(wid), Some(element_guard)) =
            (element_index, window_id, element_guard)
        {
            let element_ptr = element_guard.as_ptr();

            let _mutation_lease = match super::gate_background_window_action(
                pid,
                wid,
                Some(element_ptr),
                cua_driver_core::background_input::BackgroundAction::AxSemantic,
            )
            .await
            {
                Ok(lease) => lease,
                Err(refusal_result) => return refusal_result,
            };

            // The context menu steals key focus from the user's app when the
            // target is in the background; the guard hands it back.
            let result = latency::spawn_blocking(move || -> anyhow::Result<_> {
                let _key_focus = crate::key_focus_guard::protect(pid);
                let _action = latency::span(latency::Stage::ActionBlocking);
                let element_ptr = element_guard.as_ptr();
                let before = MenuSnapshot::read(pid, Some(element_ptr), Some(wid));
                let action = move |dispatch| {
                    let element = element_ptr as AXUIElementRef;
                    unsafe {
                        AXUIElementSetMessagingTimeout(element, 0.25);
                    }
                    let result = ax_show_menu(element_ptr, idx, pid, wid, dispatch);
                    unsafe {
                        AXUIElementSetMessagingTimeout(element, 0.0);
                    }
                    result
                };
                let (result, menu) = observe_right_click_action(before, action, || {
                    MenuSnapshot::read(pid, Some(element_ptr), Some(wid))
                });
                let result =
                    result.map_err(|_| anyhow::anyhow!("right-click action thread panicked"))?;
                result.map(|delivery| (delivery, menu))
            })
            .await;

            return match result {
                Ok(Ok((ElementRightClick::Ax(msg), menu))) => {
                    right_click_result(&msg, "ax", menu, false)
                }
                Ok(Ok((ElementRightClick::Pixel(msg), menu))) => {
                    right_click_result(&msg, "cgevent", menu, false)
                }
                Ok(Err(e)) => ToolResult::error(format!("Right-click failed: {e}")),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // ── Pixel path ───────────────────────────────────────────────────────
        // Scale back from screenshot pixels (or window points) to native
        // window pixels.
        let ((cx, cy), validated_frame) = match super::window_points::to_window_pixels(
            &self.state,
            &args,
            pid,
            window_id,
            space,
            &[(x.unwrap(), y.unwrap())],
        )
        .await
        {
            Ok(resolved) => (resolved.pixels[0], resolved.frame),
            Err(refusal) => return refusal,
        };

        // Window-local → screen coordinate translation + win-local logical coords
        // for CGEventSetWindowLocation (shared with click.rs via px_frame, which
        // refuses a window with no live frame).
        let (screen_x, screen_y, win_local_x, win_local_y) = if let Some(wid) = window_id {
            match super::window_points::frame_or_resolve(validated_frame.clone(), wid).await {
                Ok(frame) => {
                    let translated = frame.to_screen(cx, cy);
                    if !delivery_mode.is_foreground()
                        && (translated.2 < 0.0
                            || translated.3 < 0.0
                            || translated.2 > frame.bounds.width
                            || translated.3 > frame.bounds.height)
                    {
                        return super::invalid_arguments(format!(
                            "right_click: window-local point ({:.1}, {:.1}) pt lies outside \
                             window {wid}'s {:.0}×{:.0} pt frame; background delivery refused",
                            translated.2, translated.3, frame.bounds.width, frame.bounds.height
                        ));
                    }
                    translated
                }
                Err(refusal) => return refusal,
            }
        } else {
            (cx, cy, cx, cy)
        };

        let _mutation_lease = if !delivery_mode.is_foreground() {
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

        // Pin overlay above the target window before animating.
        if let Some(wid) = window_id {
            crate::cursor::overlay::send_command(
                cursor_key.clone(),
                cursor_overlay::OverlayCommand::PinAbove(wid as u64),
            );
        }
        // Animate cursor to the click point; wait for arrival before firing.
        crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), screen_x, screen_y).await;
        crate::cursor::overlay::send_command(
            cursor_key.clone(),
            cursor_overlay::OverlayCommand::ClickPulse {
                x: screen_x,
                y: screen_y,
            },
        );

        let mod_suffix = if modifiers.is_empty() {
            String::new()
        } else {
            format!(" with {}", modifiers.join("+"))
        };

        let fg = delivery_mode.is_foreground() && window_id.is_some();
        if let Err(refusal) =
            super::window_points::recheck_now(space, window_id, validated_frame.as_ref())
        {
            return refusal;
        }
        let route = match super::pixel_route::resolve(pid, fg, window_id, "mouse_right_click").await
        {
            Ok(route) => route,
            Err(refusal) => return refusal,
        };
        let dispatch_guard = match super::window_points::DispatchGuard::new(
            space,
            window_id,
            validated_frame.clone(),
        ) {
            Ok(guard) => guard,
            Err(refusal) => return refusal,
        };
        let result = latency::spawn_blocking(move || -> anyhow::Result<_> {
            let _key_focus = (!fg)
                .then(|| crate::key_focus_guard::protect(pid))
                .flatten();
            let _action = latency::span(latency::Stage::ActionBlocking);
            let do_it =
                move |dispatch: std::sync::mpsc::Sender<ActionEvent>| -> anyhow::Result<()> {
                    super::window_points::check_guard(&dispatch_guard)?;
                    let m: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    let result = if route == super::pixel_route::PixelClickRoute::ForegroundHid {
                        // Warp the hardware pointer and post at the HID tap; see
                        // `pixel_route` for the cross-platform foreground contract.
                        crate::input::mouse::click_at_xy_desktop_with_modifiers(
                            screen_x, screen_y, 1, "right", &m,
                        )
                    } else if let Some(wid) = window_id {
                        crate::input::mouse::right_click_at_xy_with_window_local(
                            pid,
                            screen_x,
                            screen_y,
                            win_local_x,
                            win_local_y,
                            wid,
                            &m,
                        )
                    } else {
                        crate::input::mouse::right_click_at_xy(pid, screen_x, screen_y, &m)
                    };
                    let _ = dispatch.send(ActionEvent::Sent(Instant::now()));
                    result
                };
            // Foreground rung: front the exact window → HID right-click →
            // restore the prior frontmost. No input is sent unless the exact
            // window is proven focused.
            let before = MenuSnapshot::read(pid, None, window_id);
            let action = move |dispatch| match (route, window_id) {
                (super::pixel_route::PixelClickRoute::ForegroundHid, Some(wid)) => {
                    crate::input::skylight::with_foreground_hid_activation(
                        pid as libc::pid_t,
                        wid,
                        || do_it(dispatch),
                    )
                }
                _ => do_it(dispatch),
            };
            let (result, menu) = observe_right_click_action(before, action, || {
                MenuSnapshot::read(pid, None, window_id)
            });
            let result =
                result.map_err(|_| anyhow::anyhow!("right-click action thread panicked"))?;
            result.map(|()| ((), menu))
        })
        .await;
        if let Ok(Err(error)) = &result {
            if let Some(refusal) = super::window_points::moved_refusal(error, space) {
                return refusal;
            }
        }
        match result {
            Ok(Ok(((), menu))) => right_click_result(
                &format!(
                    "Right-clicked{mod_suffix} at ({screen_x:.1}, {screen_y:.1}) ({}).",
                    super::pixel_route::delivery_note(route)
                ),
                super::pixel_route::path_label(route),
                menu,
                fg,
            ),
            Ok(Err(e)) if route == super::pixel_route::PixelClickRoute::ForegroundHid => {
                super::pixel_route::foreground_unavailable(
                    "Right-click",
                    window_id.unwrap_or_default(),
                    &e.to_string(),
                )
            }
            Ok(Err(e)) => ToolResult::error(format!("Right-click failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── Blocking AX path ─────────────────────────────────────────────────────────

/// How an element right-click was delivered.
#[derive(Debug, PartialEq, Eq)]
enum ElementRightClick {
    /// AXShowMenu was sent; the message says whether the app reported an error.
    Ax(String),
    /// A pixel right-click at the element's center.
    Pixel(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuEvidence {
    None,
    Accessibility,
    Window,
}

#[derive(Debug)]
struct RetainedAxElement(usize);

impl Drop for RetainedAxElement {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) }
    }
}

#[derive(Debug)]
struct MenuSnapshot {
    window_ids: Option<HashSet<u32>>,
    ax_menus: Option<Vec<RetainedAxElement>>,
}

impl MenuSnapshot {
    fn read(pid: i32, target: Option<usize>, window_id: Option<u32>) -> Self {
        Self {
            window_ids: menu_window_ids(pid),
            ax_menus: unsafe { ax_menus(pid, target, window_id) },
        }
    }
}

unsafe fn collect_direct_menus(root: AXUIElementRef, menus: &mut Vec<RetainedAxElement>) -> bool {
    let Some(children) = copy_children_checked(root) else {
        return false;
    };
    let mut menu_flags = Vec::with_capacity(children.len());
    for &child in &children {
        match is_ax_menu(child) {
            Some(is_menu) => menu_flags.push(is_menu),
            None => {
                for child in &children {
                    CFRelease(*child as CFTypeRef);
                }
                return false;
            }
        }
    }
    for (child, is_menu) in children.into_iter().zip(menu_flags) {
        if is_menu {
            let duplicate = menus
                .iter()
                .any(|existing| CFEqual(existing.0 as CFTypeRef, child as CFTypeRef) != 0);
            if duplicate {
                CFRelease(child as CFTypeRef);
            } else {
                menus.push(RetainedAxElement(child as usize));
            }
        } else {
            CFRelease(child as CFTypeRef);
        }
    }
    true
}

unsafe fn is_ax_menu(element: AXUIElementRef) -> Option<bool> {
    let attr = CFString::new("AXRole");
    let mut value: CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if error != kAXErrorSuccess || value.is_null() {
        if !value.is_null() {
            CFRelease(value);
        }
        return None;
    }
    if CFGetTypeID(value) != CFString::type_id() {
        CFRelease(value);
        return None;
    }
    let role = CFString::wrap_under_create_rule(value as _).to_string();
    Some(role == "AXMenu")
}

unsafe fn window_child(app: AXUIElementRef, window_id: u32) -> Option<RetainedAxElement> {
    let children = copy_children_checked(app)?;
    let mut found = None;
    for child in children {
        if found.is_none() && ax_get_window_id(child) == Some(window_id) {
            found = Some(RetainedAxElement(child as usize));
        } else {
            CFRelease(child as CFTypeRef);
        }
    }
    found
}

unsafe fn ax_menus(
    pid: i32,
    target: Option<usize>,
    window_id: Option<u32>,
) -> Option<Vec<RetainedAxElement>> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return None;
    }
    let app = RetainedAxElement(app as usize);
    let window = if let Some(target) = target.filter(|_| window_id.is_some()) {
        let window = copy_element_attr(target as AXUIElementRef, "AXWindow")?;
        Some(RetainedAxElement(window as usize))
    } else if let Some(window_id) = window_id {
        Some(window_child(app.0 as AXUIElementRef, window_id)?)
    } else {
        None
    };
    let mut menus = Vec::new();
    if !collect_direct_menus(app.0 as AXUIElementRef, &mut menus) {
        return None;
    }
    if let Some(target) = target {
        if !collect_direct_menus(target as AXUIElementRef, &mut menus) {
            return None;
        }
    }
    if let Some(window) = window {
        if !collect_direct_menus(window.0 as AXUIElementRef, &mut menus) {
            return None;
        }
    }
    Some(menus)
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    #[link_name = "CGWindowListCopyWindowInfo"]
    fn cg_window_list_copy_window_info(
        option: u32,
        relative_to_window: u32,
    ) -> core_foundation::array::CFArrayRef;
}

fn menu_window_ids(pid: i32) -> Option<HashSet<u32>> {
    use core_foundation::{
        array::CFArray,
        base::{CFGetTypeID, CFTypeRef, TCFType},
        dictionary::CFDictionary,
        number::CFNumber,
        string::CFString,
    };
    use std::ffi::c_void;

    // Exclude desktop elements but retain off-screen and other-Space windows so
    // re-exposing an existing menu cannot masquerade as a new window ID.
    let raw = unsafe { cg_window_list_copy_window_info(16, 0) };
    if raw.is_null() {
        return None;
    }
    let windows: CFArray<CFTypeRef> = unsafe { CFArray::wrap_under_create_rule(raw as _) };
    let mut ids = HashSet::new();
    for item in windows.iter() {
        let item = *item;
        let dictionary_type = CFDictionary::<*const c_void, *const c_void>::type_id();
        if unsafe { CFGetTypeID(item) } != dictionary_type {
            return None;
        }
        let dictionary: CFDictionary<*const c_void, *const c_void> =
            unsafe { CFDictionary::wrap_under_get_rule(item as _) };
        let number = |key: &str| -> Option<i64> {
            let key = CFString::new(key);
            let value = *dictionary.find(key.as_concrete_TypeRef() as *const c_void)?;
            unsafe {
                (CFGetTypeID(value) == CFNumber::type_id())
                    .then(|| CFNumber::wrap_under_get_rule(value as _).to_i64())
                    .flatten()
            }
        };
        let owner = number("kCGWindowOwnerPID")?;
        if owner != i64::from(pid) {
            continue;
        }
        let layer = number("kCGWindowLayer")?;
        let id = number("kCGWindowNumber")?;
        if layer == 101 {
            ids.insert(u32::try_from(id).ok()?);
        }
    }
    Some(ids)
}

unsafe fn menu_item_titles(menu: AXUIElementRef) -> Vec<String> {
    let Some(children) = copy_children_checked(menu) else {
        return Vec::new();
    };
    let mut titles = Vec::new();
    for child in children {
        if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenuItem") {
            if let Some(title) =
                copy_string_attr(child, "AXTitle").filter(|title| !title.trim().is_empty())
            {
                titles.push(title);
            }
        }
        CFRelease(child as CFTypeRef);
    }
    cap_menu_titles(titles)
}

fn cap_menu_titles(titles: impl IntoIterator<Item = String>) -> Vec<String> {
    titles
        .into_iter()
        .filter(|title| !title.trim().is_empty())
        .take(10)
        .collect()
}

#[cfg(test)]
fn has_new_by<T>(before: &[T], after: &[T], equal: impl Fn(&T, &T) -> bool) -> Option<usize> {
    after
        .iter()
        .position(|candidate| !before.iter().any(|old| equal(old, candidate)))
}

#[cfg(test)]
fn new_menu_observation(before: &MenuSnapshot, after: &MenuSnapshot) -> Option<MenuObservation> {
    if let (Some(before_menus), Some(after_menus)) = (&before.ax_menus, &after.ax_menus) {
        if let Some(index) = has_new_by(before_menus, after_menus, |old, menu| unsafe {
            CFEqual(old.0 as CFTypeRef, menu.0 as CFTypeRef) != 0
        }) {
            let menu = &after_menus[index];
            return Some(MenuObservation {
                opened: true,
                items: unsafe { menu_item_titles(menu.0 as AXUIElementRef) },
                evidence: MenuEvidence::Accessibility,
            });
        }
    }
    if let (Some(before_ids), Some(after_ids)) = (&before.window_ids, &after.window_ids) {
        if after_ids.difference(before_ids).next().is_some() {
            return Some(MenuObservation {
                opened: true,
                items: Vec::new(),
                evidence: MenuEvidence::Window,
            });
        }
    }
    None
}

fn new_menu_observation_between(
    before: &MenuSnapshot,
    sent: &MenuSnapshot,
    after: &MenuSnapshot,
) -> Option<MenuObservation> {
    if let (Some(before_menus), Some(sent_menus), Some(after_menus)) =
        (&before.ax_menus, &sent.ax_menus, &after.ax_menus)
    {
        for candidate in after_menus {
            let absent = |menus: &Vec<RetainedAxElement>| {
                !menus.iter().any(|old| unsafe {
                    CFEqual(old.0 as CFTypeRef, candidate.0 as CFTypeRef) != 0
                })
            };
            if absent(before_menus) && absent(sent_menus) {
                return Some(MenuObservation {
                    opened: true,
                    items: unsafe { menu_item_titles(candidate.0 as AXUIElementRef) },
                    evidence: MenuEvidence::Accessibility,
                });
            }
        }
    }
    if let (Some(before_ids), Some(sent_ids), Some(after_ids)) =
        (&before.window_ids, &sent.window_ids, &after.window_ids)
    {
        if after_ids
            .difference(before_ids)
            .any(|candidate| !sent_ids.contains(candidate))
        {
            return Some(MenuObservation {
                opened: true,
                items: Vec::new(),
                evidence: MenuEvidence::Window,
            });
        }
    }
    None
}

#[cfg(test)]
fn poll_until<T>(
    timeout: Duration,
    interval: Duration,
    mut now: impl FnMut() -> Instant,
    sleep: impl FnMut(Duration),
    read: impl FnMut() -> Option<T>,
) -> Option<T> {
    let deadline = now() + timeout;
    poll_until_deadline(deadline, interval, now, sleep, read)
}

fn poll_until_deadline<T>(
    deadline: Instant,
    interval: Duration,
    mut now: impl FnMut() -> Instant,
    mut sleep: impl FnMut(Duration),
    mut read: impl FnMut() -> Option<T>,
) -> Option<T> {
    loop {
        let observed = read();
        let current = now();
        if current > deadline {
            return None;
        }
        if let Some(observed) = observed {
            return Some(observed);
        }
        let remaining = deadline.saturating_duration_since(current);
        if remaining.is_zero() {
            return None;
        }
        sleep(interval.min(remaining));
    }
}

enum ActionEvent {
    Sent(Instant),
    Done,
}

fn observe_right_click_action<T: Send + 'static>(
    before: MenuSnapshot,
    action: impl FnOnce(std::sync::mpsc::Sender<ActionEvent>) -> T + Send + 'static,
    mut read: impl FnMut() -> MenuSnapshot,
) -> (std::thread::Result<T>, MenuObservation) {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let action_tx = event_tx.clone();
    let handle = std::thread::spawn(move || {
        let result = action(action_tx);
        let _ = event_tx.send(ActionEvent::Done);
        result
    });
    let sent_at = match event_rx.recv() {
        Ok(ActionEvent::Sent(sent_at)) => sent_at,
        Ok(ActionEvent::Done) | Err(_) => {
            let result = handle.join();
            return (result, MenuObservation::none());
        }
    };
    let sent_baseline = read();
    let deadline = sent_at + Duration::from_millis(150);
    let observation = poll_until_deadline(
        deadline,
        Duration::from_millis(15),
        Instant::now,
        std::thread::sleep,
        || {
            let after = read();
            new_menu_observation_between(&before, &sent_baseline, &after)
        },
    )
    .unwrap_or_else(MenuObservation::none);
    let result = handle.join();
    (result, observation)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MenuObservation {
    opened: bool,
    items: Vec<String>,
    evidence: MenuEvidence,
}

impl MenuObservation {
    fn none() -> Self {
        Self {
            opened: false,
            items: Vec::new(),
            evidence: MenuEvidence::None,
        }
    }
}

fn right_click_result(
    message: &str,
    path: &str,
    menu: MenuObservation,
    foreground: bool,
) -> ToolResult {
    let confirmed = menu.opened && menu.evidence != MenuEvidence::None;
    let mut text = message.to_owned();
    if confirmed {
        text.push_str(" Context menu opened");
        if !menu.items.is_empty() {
            text.push_str(&format!(": {}", menu.items.join(", ")));
        }
        text.push('.');
    }
    let structured = serde_json::json!({
        "path": path,
        "verified": confirmed,
        "effect": if confirmed { "confirmed" } else { "unverifiable" },
        "menu_opened": confirmed,
        "menu_items": confirmed.then(|| menu.items.clone()),
    });
    let (transport, requested, actual) = if path == "ax" {
        (
            ActionTransport::MacosAxAction,
            RequestedDelivery::Background,
            ActualDelivery::Background,
        )
    } else if foreground {
        (
            ActionTransport::MacosCgEventHid,
            RequestedDelivery::Foreground,
            ActualDelivery::Foreground,
        )
    } else {
        (
            ActionTransport::MacosCgEventPid,
            RequestedDelivery::Background,
            ActualDelivery::Background,
        )
    };
    let evidence_kind = match menu.evidence {
        MenuEvidence::Accessibility if confirmed => EvidenceKind::AccessibilityReadback,
        MenuEvidence::Window if confirmed => EvidenceKind::WindowChange,
        _ => EvidenceKind::NativeApiResult,
    };
    let record = ActionExecutionRecord::builder(
        if confirmed {
            ActionEffect::Confirmed
        } else {
            ActionEffect::Unverifiable
        },
        transport,
        requested,
    )
    .actual_delivery(actual)
    .evidence(ActionEvidence {
        kind: evidence_kind,
        detail: if confirmed {
            "A new context menu was observed after the right-click".into()
        } else {
            "Right-click delivery completed without a new menu readback".into()
        },
    })
    .build()
    .expect("right-click record has valid evidence and delivery");
    ToolResult::text(text)
        .with_structured(structured)
        .with_action_record(record)
}

/// AXShowMenu when the element advertises it, else a pixel right-click.
///
/// An AXShowMenu error that does not prove nothing was sent may have opened
/// the menu: report that, and never right-click on top of it. Only an error
/// that proves nothing was sent falls back to the pixel right-click.
fn right_click_element(
    label: &str,
    advertises_show_menu: bool,
    show_menu: impl FnOnce() -> crate::ax::bindings::AXError,
    ax_delivered: impl FnOnce(),
    pixel: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<ElementRightClick> {
    if advertises_show_menu {
        let err = show_menu();
        if err == kAXErrorSuccess {
            ax_delivered();
            return Ok(ElementRightClick::Ax(format!(
                "Shown menu for {label} (AXShowMenu)."
            )));
        }
        if !super::click::perform_error_proves_nothing_was_sent(err) {
            ax_delivered();
            return Ok(ElementRightClick::Ax(format!(
                "⚠️ Sent AXShowMenu to {label}, but the app returned AX error {err}, which does \
                 not mean it failed: the menu may be open. Read the window before repeating it."
            )));
        }
        tracing::debug!("AXShowMenu returned {err} for {label}; falling back to pixel right-click");
    }
    pixel().map(ElementRightClick::Pixel)
}

fn ax_show_menu(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    wid: u32,
    dispatch: std::sync::mpsc::Sender<ActionEvent>,
) -> anyhow::Result<ElementRightClick> {
    let element = element_ptr as AXUIElementRef;

    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    let title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();

    let advertised = unsafe { copy_action_names(element) };

    // Only attempt the pure-AX AXShowMenu when the element actually advertises
    // it. Plain controls (NSButton, custom NSView click targets, most web
    // nodes) DON'T — calling AXShowMenu on them returns kAXErrorActionUnsupported
    // (-25206), which used to surface as a hard "AXShowMenu failed" error and
    // forced the agent onto raw pixels. Instead, resolve the element's on-screen
    // center and synthesize a REAL pixel right-click there — the same actuation
    // a user performs, delivered to backgrounded windows via the window-local
    // primitive. This makes "right-click element N" land on any element, not
    // just ones with a native context-menu AX action.
    let ax_dispatch = dispatch.clone();
    right_click_element(
        &format!("[{idx}] {role} \"{title}\""),
        advertised.iter().any(|a| a == "AXShowMenu"),
        || unsafe { perform_action(element, "AXShowMenu") },
        || {
            let _ = ax_dispatch.send(ActionEvent::Sent(Instant::now()));
        },
        || {
            let result = pixel_right_click(element, idx, &role, &title, pid, wid);
            let _ = dispatch.send(ActionEvent::Sent(Instant::now()));
            result
        },
    )
}

fn pixel_right_click(
    element: AXUIElementRef,
    idx: usize,
    role: &str,
    title: &str,
    pid: i32,
    wid: u32,
) -> anyhow::Result<String> {
    // Pixel right-click at the element's screen-space center.
    let (cx, cy) =
        unsafe { crate::ax::bindings::element_screen_center(element) }.ok_or_else(|| {
            anyhow::anyhow!(
                "[{idx}] {role} \"{title}\" advertises no AXShowMenu and has no resolvable \
             on-screen center for a pixel right-click. Pass x, y directly."
            )
        })?;
    let (wx, wy) = crate::windows::window_bounds_by_id(wid)
        .map(|b| (cx - b.x, cy - b.y))
        .unwrap_or((cx, cy));
    crate::input::mouse::right_click_at_xy_with_window_local(pid, cx, cy, wx, wy, wid, &[])?;
    Ok(format!(
        "Right-clicked [{idx}] {role} \"{title}\" at element center ({cx:.0}, {cy:.0}) \
         (pixel right-click; element advertises no AXShowMenu)."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_axshowmenu_error_that_may_have_opened_the_menu_sends_no_pixel_right_click() {
        use crate::ax::bindings::{
            kAXErrorActionUnsupported, kAXErrorCannotComplete, kAXErrorIllegalArgument,
        };
        for err in [kAXErrorCannotComplete, kAXErrorActionUnsupported] {
            let outcome = right_click_element(
                "[4] AXRow \"a\"",
                true,
                || err,
                || {},
                || panic!("pixel right-click on top of a menu that may be open"),
            )
            .expect("reported, not failed");
            assert!(
                matches!(&outcome, ElementRightClick::Ax(message) if message.contains("may be open")),
                "{outcome:?}"
            );
        }
        // Proven unsent, or not advertised: one pixel right-click.
        for (advertised, err) in [(true, kAXErrorIllegalArgument), (false, kAXErrorSuccess)] {
            let pixels = std::cell::Cell::new(0);
            let ax_delivered = std::cell::Cell::new(0);
            let outcome = right_click_element(
                "[4]",
                advertised,
                || err,
                || ax_delivered.set(ax_delivered.get() + 1),
                || {
                    pixels.set(pixels.get() + 1);
                    Ok("pixel".into())
                },
            )
            .unwrap();
            assert_eq!(outcome, ElementRightClick::Pixel("pixel".into()));
            assert_eq!(pixels.get(), 1);
            assert_eq!(
                ax_delivered.get(),
                0,
                "a proven-unsent AX attempt has no sent signal"
            );
        }
    }

    struct ShowMenuProbe;

    #[async_trait]
    impl Tool for ShowMenuProbe {
        fn def(&self) -> &ToolDef {
            def()
        }

        async fn invoke(&self, _args: Value) -> ToolResult {
            right_click_result(
                "Shown menu for [4] AXRow \"a\" (AXShowMenu).",
                "ax",
                MenuObservation::none(),
                false,
            )
        }
    }

    #[tokio::test]
    async fn an_axshowmenu_right_click_is_published_as_accessibility() {
        let mut registry = cua_driver_core::tool::ToolRegistry::new();
        registry.register(Box::new(ShowMenuProbe));
        let result = registry
            .invoke(
                "right_click",
                serde_json::json!({"pid": 1, "window_id": 2, "element_token": "s1:4"}),
            )
            .await;
        let structured = result.structured_content.expect("published action result");
        assert_eq!(structured["route"], "accessibility", "{structured}");
        assert_eq!(structured["effect"], "unverifiable", "{structured}");
    }

    struct ConfirmedMenuProbe;

    #[async_trait]
    impl Tool for ConfirmedMenuProbe {
        fn def(&self) -> &ToolDef {
            def()
        }

        async fn invoke(&self, _args: Value) -> ToolResult {
            right_click_result(
                "menu delivered",
                "ax",
                MenuObservation {
                    opened: true,
                    items: vec!["Copy".into()],
                    evidence: MenuEvidence::Accessibility,
                },
                false,
            )
        }
    }

    #[tokio::test]
    async fn wave400c_registry_preserves_the_public_menu_readback() {
        let mut registry = cua_driver_core::tool::ToolRegistry::new();
        registry.register(Box::new(ConfirmedMenuProbe));
        let result = registry
            .invoke(
                "right_click",
                serde_json::json!({"pid": 1, "window_id": 2, "element_token": "s1:4"}),
            )
            .await;
        let structured = result.structured_content.expect("public action result");
        assert_eq!(structured["effect"], "confirmed", "{structured}");
        assert_eq!(structured["verified"], true, "{structured}");
        assert_eq!(structured["menu_opened"], true, "{structured}");
        assert_eq!(structured["menu_items"], serde_json::json!(["Copy"]));
        cua_driver_contract::validate_success_output("right_click", structured)
            .expect("typed public output");
    }

    #[test]
    fn wave400c_right_click_result_promotes_a_new_menu() {
        let result = right_click_result(
            "menu delivered",
            "ax",
            MenuObservation {
                opened: true,
                items: vec!["Copy".into(), "Paste".into()],
                evidence: MenuEvidence::Accessibility,
            },
            false,
        );
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["effect"], "confirmed");
        assert_eq!(structured["verified"], true);
        assert_eq!(structured["menu_opened"], true);
        assert_eq!(
            structured["menu_items"],
            serde_json::json!(["Copy", "Paste"])
        );
        assert_eq!(
            result.action_record.unwrap().evidence[0].kind,
            EvidenceKind::AccessibilityReadback
        );
    }

    #[test]
    fn wave400c_menu_novelty_rejects_reexposure_and_incomplete_baselines() {
        assert_eq!(has_new_by(&[1, 2], &[1, 2], |a, b| a == b), None);
        assert_eq!(has_new_by(&[1, 2], &[1, 2, 3], |a, b| a == b), Some(2));

        let before = MenuSnapshot {
            window_ids: Some(HashSet::from([7])),
            ax_menus: None,
        };
        let reexposed = MenuSnapshot {
            window_ids: Some(HashSet::from([7])),
            ax_menus: None,
        };
        assert!(new_menu_observation(&before, &reexposed).is_none());
        let new_window = MenuSnapshot {
            window_ids: Some(HashSet::from([7, 8])),
            ax_menus: None,
        };
        assert_eq!(
            new_menu_observation(&before, &new_window).unwrap().evidence,
            MenuEvidence::Window
        );
        let incomplete = MenuSnapshot {
            window_ids: None,
            ax_menus: None,
        };
        assert!(new_menu_observation(&incomplete, &new_window).is_none());

        let crossed_after = MenuSnapshot {
            window_ids: Some(HashSet::from([7, 8])),
            ax_menus: None,
        };
        let other_baseline = MenuSnapshot {
            window_ids: Some(HashSet::from([8])),
            ax_menus: None,
        };
        assert!(new_menu_observation_between(&before, &other_baseline, &crossed_after).is_none());
        let common_new = MenuSnapshot {
            window_ids: Some(HashSet::from([7, 8, 9])),
            ax_menus: None,
        };
        assert!(new_menu_observation_between(&before, &other_baseline, &common_new).is_some());
    }

    #[test]
    fn wave400c_menu_titles_are_capped_and_empty_titles_are_dropped() {
        let titles = (0..12)
            .map(|index| format!("item {index}"))
            .chain(["  ".into(), "ignored".into()]);
        assert_eq!(cap_menu_titles(titles).len(), 10);
    }

    #[test]
    fn wave400c_poll_uses_immediate_interval_and_deadline_reads() {
        use std::{cell::RefCell, rc::Rc};
        let start = Instant::now();
        let clock = Rc::new(RefCell::new(start));
        let sleeps = Rc::new(RefCell::new(Vec::new()));
        let now_clock = clock.clone();
        let sleep_clock = clock.clone();
        let sleep_log = sleeps.clone();
        let mut reads = 0;
        let result: Option<()> = poll_until(
            Duration::from_millis(150),
            Duration::from_millis(15),
            move || *now_clock.borrow(),
            move |duration| {
                *sleep_clock.borrow_mut() += duration;
                sleep_log.borrow_mut().push(duration);
            },
            || {
                reads += 1;
                None
            },
        );
        assert!(result.is_none());
        assert_eq!(reads, 11);
        assert_eq!(sleeps.borrow().as_slice(), &[Duration::from_millis(15); 10]);

        let slow_clock = Rc::new(RefCell::new(start));
        let now_clock = slow_clock.clone();
        let read_clock = slow_clock.clone();
        let observed = poll_until(
            Duration::from_millis(150),
            Duration::from_millis(15),
            move || *now_clock.borrow(),
            |_| {},
            move || {
                *read_clock.borrow_mut() += Duration::from_millis(151);
                Some(())
            },
        );
        assert!(
            observed.is_none(),
            "a read completing after the deadline cannot confirm"
        );
    }

    #[test]
    fn wave400c_observation_runs_while_the_action_is_blocked() {
        let (baseline_tx, baseline_rx) = std::sync::mpsc::channel();
        let before = MenuSnapshot {
            window_ids: Some(HashSet::new()),
            ax_menus: None,
        };
        let menu_ids = Arc::new(std::sync::Mutex::new(HashSet::from([1])));
        let action_ids = menu_ids.clone();
        let mut reads = 0;
        let (result, observation) = observe_right_click_action(
            before,
            move |sent| {
                let _ = sent.send(ActionEvent::Sent(Instant::now()));
                baseline_rx.recv().unwrap();
                action_ids.lock().unwrap().insert(2);
                42
            },
            || {
                reads += 1;
                let snapshot = MenuSnapshot {
                    window_ids: Some(menu_ids.lock().unwrap().clone()),
                    ax_menus: None,
                };
                if reads == 1 {
                    baseline_tx.send(()).unwrap();
                }
                snapshot
            },
        );
        assert_eq!(result.unwrap(), 42);
        assert!(observation.opened);
        assert_eq!(observation.evidence, MenuEvidence::Window);
    }

    #[test]
    fn wave400c_menu_seen_before_the_actual_dispatch_does_not_confirm() {
        let before = MenuSnapshot {
            window_ids: Some(HashSet::new()),
            ax_menus: None,
        };
        let menu_ids = Arc::new(std::sync::Mutex::new(HashSet::from([1])));
        let action_ids = menu_ids.clone();
        let (result, observation) = observe_right_click_action(
            before,
            move |sent| {
                action_ids.lock().unwrap().insert(2);
                let _ = sent.send(ActionEvent::Sent(Instant::now()));
            },
            || MenuSnapshot {
                window_ids: Some(menu_ids.lock().unwrap().clone()),
                ax_menus: None,
            },
        );
        result.unwrap();
        assert!(!observation.opened);
    }

    #[test]
    fn wave400c_proven_unsent_fallback_rearms_and_observes_the_pixel_dispatch() {
        use crate::ax::bindings::kAXErrorIllegalArgument;
        let (sent_tx, sent_rx) = std::sync::mpsc::channel();
        let ax_tx = sent_tx.clone();
        let outcome = right_click_element(
            "[4]",
            true,
            || kAXErrorIllegalArgument,
            move || {
                let _ = ax_tx.send("ax");
            },
            move || {
                let _ = sent_tx.send("pixel");
                Ok("pixel".into())
            },
        )
        .unwrap();
        assert_eq!(outcome, ElementRightClick::Pixel("pixel".into()));
        assert_eq!(sent_rx.recv().unwrap(), "pixel");
        assert!(sent_rx.try_recv().is_err());
    }

    #[test]
    fn wave400c_fast_completion_still_observes_the_sent_window() {
        let before = MenuSnapshot {
            window_ids: Some(HashSet::new()),
            ax_menus: None,
        };
        let mut reads = 0;
        let (result, observation) = observe_right_click_action(
            before,
            |sent| {
                let _ = sent.send(ActionEvent::Sent(Instant::now()));
            },
            || {
                reads += 1;
                MenuSnapshot {
                    window_ids: Some(if reads == 1 {
                        HashSet::new()
                    } else {
                        HashSet::from([1])
                    }),
                    ax_menus: None,
                }
            },
        );
        result.unwrap();
        assert!(observation.opened);
    }
}
