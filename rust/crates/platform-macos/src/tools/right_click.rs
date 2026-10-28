use crate::goat::action_latency as latency;

use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use crate::ax::bindings::{
    copy_action_names, copy_string_attr, kAXErrorSuccess, perform_action, AXUIElementRef,
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
            let result = latency::spawn_blocking(move || {
                let _key_focus = crate::key_focus_guard::protect(pid);
                let _action = latency::span(latency::Stage::ActionBlocking);
                ax_show_menu(element_guard.as_ptr(), idx, pid, wid)
            })
            .await;

            return match result {
                Ok(Ok(ElementRightClick::Ax(msg))) => ax_right_click_result(&msg),
                Ok(Ok(ElementRightClick::Pixel(msg))) => ToolResult::text(msg),
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
        if let Err(refusal) = super::window_points::recheck_now(space, window_id) {
            return refusal;
        }
        let route = match super::pixel_route::resolve(pid, fg, window_id, "mouse_right_click").await
        {
            Ok(route) => route,
            Err(refusal) => return refusal,
        };
        let result = latency::spawn_blocking(move || -> anyhow::Result<()> {
            let _key_focus = (!fg)
                .then(|| crate::key_focus_guard::protect(pid))
                .flatten();
            let _action = latency::span(latency::Stage::ActionBlocking);
            let do_it = move || -> anyhow::Result<()> {
                let m: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                if route == super::pixel_route::PixelClickRoute::ForegroundHid {
                    // Warp the hardware pointer and post at the HID tap; see
                    // `pixel_route` for the cross-platform foreground contract.
                    return crate::input::mouse::click_at_xy_desktop_with_modifiers(
                        screen_x, screen_y, 1, "right", &m,
                    );
                }
                if let Some(wid) = window_id {
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
                }
            };
            // Foreground rung: front the exact window → HID right-click →
            // restore the prior frontmost. No input is sent unless the exact
            // window is proven focused.
            match (route, window_id) {
                (super::pixel_route::PixelClickRoute::ForegroundHid, Some(wid)) => {
                    crate::input::skylight::with_foreground_hid_activation(
                        pid as libc::pid_t,
                        wid,
                        do_it,
                    )
                }
                _ => do_it(),
            }
        })
        .await;
        match result {
            Ok(Ok(())) => ToolResult::text(format!(
                "Right-clicked{mod_suffix} at ({screen_x:.1}, {screen_y:.1}) ({}).",
                super::pixel_route::delivery_note(route)
            ))
            .with_structured(serde_json::json!({
                "path": super::pixel_route::path_label(route), "verified": false, "effect": "unverifiable"
            })),
            Ok(Err(e)) if route == super::pixel_route::PixelClickRoute::ForegroundHid => {
                super::pixel_route::foreground_unavailable(
                    "Right-click",
                    window_id.unwrap_or_default(),
                    &e.to_string(),
                )
            }
            Ok(Err(e)) => ToolResult::error(format!("Right-click failed: {e}")),
            Err(e)     => ToolResult::error(format!("Task error: {e}")),
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

/// A right-click delivered as AXShowMenu. Nothing reads back the menu.
fn ax_right_click_result(message: &str) -> ToolResult {
    ToolResult::text(message)
        .with_structured(serde_json::json!({ "path": "ax", "effect": "unverifiable" }))
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
    pixel: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<ElementRightClick> {
    if advertises_show_menu {
        let err = show_menu();
        if err == kAXErrorSuccess {
            return Ok(ElementRightClick::Ax(format!(
                "Shown menu for {label} (AXShowMenu)."
            )));
        }
        if !super::click::perform_error_proves_nothing_was_sent(err) {
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
    right_click_element(
        &format!("[{idx}] {role} \"{title}\""),
        advertised.iter().any(|a| a == "AXShowMenu"),
        || unsafe { perform_action(element, "AXShowMenu") },
        || pixel_right_click(element, idx, &role, &title, pid, wid),
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
            let outcome = right_click_element(
                "[4]",
                advertised,
                || err,
                || {
                    pixels.set(pixels.get() + 1);
                    Ok("pixel".into())
                },
            )
            .unwrap();
            assert_eq!(outcome, ElementRightClick::Pixel("pixel".into()));
            assert_eq!(pixels.get(), 1);
        }
    }

    struct ShowMenuProbe;

    #[async_trait]
    impl Tool for ShowMenuProbe {
        fn def(&self) -> &ToolDef {
            def()
        }

        async fn invoke(&self, _args: Value) -> ToolResult {
            ax_right_click_result("Shown menu for [4] AXRow \"a\" (AXShowMenu).")
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
}
