use crate::goat::action_latency as latency;

use async_trait::async_trait;
use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_contract::{ScrollBy, ScrollDirection, ScrollInput};
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_children, copy_element_attr, copy_string_attr, element_screen_center, kAXErrorSuccess,
    perform_action, AXUIElementRef,
};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;

use super::ToolState;

/// Per-notch pixel step for the wheel path. `page` rolls a screenful-ish chunk,
/// `line` a few text lines — tuned to feel like a real wheel notch. Runtime
/// tuning deferred (host is screen-recording); centralized here for one-line edits.
const WHEEL_STEP_LINE_PX: i32 = 120;
const WHEEL_STEP_PAGE_PX: i32 = 600;

/// Resolved pixel-wheel target in screen space, plus optional window-local
/// stamp + window id for backgrounded delivery.
struct WheelTarget {
    screen_x: f64,
    screen_y: f64,
    win_local: Option<(f64, f64)>,
    wid: Option<u32>,
    /// Element screen rect `[x, y, w, h]` for the cursor glide; `None` for
    /// pixel targets.
    rect: Option<[f64; 4]>,
}

pub struct ScrollTool {
    state: Arc<ToolState>,
}

impl ScrollTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Wheel notches or keystroke repetitions accepted in one call. The input
/// schema advertises this range and both delivery paths enforce it.
const AMOUNT_MIN: u64 = 1;
const AMOUNT_MAX: u64 = 50;

fn clamp_amount(requested: u64) -> usize {
    requested.clamp(AMOUNT_MIN, AMOUNT_MAX) as usize
}

const ELECTRON_BACKGROUND_SCROLL_UNAVAILABLE: &str = "Background scroll is unavailable for \
     this Electron/Chromium window on macOS: it drops background wheel events and keystrokes, \
     and background scroll movement could not be established through accessibility. \
     Retry with delivery_mode:\"foreground\" (and the same window_id, element_token \
     or x,y); the driver briefly fronts that window, scrolls, and restores the prior frontmost \
     app.";

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "scroll".into(),
        description: "Scroll the target pid.\n\
            - Targeted wheel: pass `element_token` (preferred) or window-local `x, y`. Sends a real wheel event at that point, so it scrolls whatever is under it. The only way to scroll nested `overflow:auto` regions in web views.\n\
            - Keystroke: no target, just pid + direction. PageDown/PageUp (by='page') or arrows (by='line') on the focused scroller.\n\
            \n\
            Electron/Chromium windows drop background wheel events: there a background scroll reveals the content just past the edge of the scroll container under the target through accessibility (about `amount` notches); `delivery_mode:\"foreground\"` is the fallback.\n\
            `amount` is wheel notches (targeted) or key repetitions (keystroke).".into(),
        input_schema: serde_json::json!({
            "type": "object",
            // `pid` conditionally required (validated in code), not pinned in the
            // schema — keeps the contract consistent across platforms.
            "required": ["direction"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it." },
                "pid": { "type": "integer", "description": "Target process ID. Required unless scope is \"desktop\" or element_token is supplied." },
                "direction": {
                    "type": "string",
                    "enum": ["up", "down", "left", "right"],
                    "description": "Scroll direction."
                },
                "by": {
                    "type": "string",
                    "enum": ["line", "page"],
                    "description": "Scroll granularity. Default: line."
                },
                "amount": {
                    "type": "integer",
                    "minimum": AMOUNT_MIN,
                    "maximum": AMOUNT_MAX,
                    "description": "Wheel notches or key repetitions, clamped to the maximum. Default 3."
                },
                "window_id": { "type": "integer", "description": "Window ID. Required with x/y; optional with element_token." },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "x": { "type": "number", "description": "Window-local screenshot X (get_window_state PNG). With y, wheel-scrolls at that point; needs window_id." },
                "y": { "type": "number", "description": "Window-local screenshot Y." },
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "\"desktop\" with x,y and no pid/window_id uses get_desktop_state coordinates." },
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema_with("Default \"background\": no fronting or focus steal. \"foreground\": briefly front the window, act, restore the prior app; last resort when background did not land.")
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: true,
    })
}

#[async_trait]
impl Tool for ScrollTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<ScrollInput>("scroll", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let (x, y) = (input.x, input.y);
            let direction = input.direction.as_str();
            let by = input.by.unwrap_or(ScrollBy::Line).as_str();
            let amount = clamp_amount(input.amount.unwrap_or(3));
            let step = if input.by == Some(ScrollBy::Page) {
                WHEEL_STEP_PAGE_PX
            } else {
                WHEEL_STEP_LINE_PX
            };
            let (delta_y, delta_x) = match input.direction {
                ScrollDirection::Down => (-step, 0),
                ScrollDirection::Up => (step, 0),
                ScrollDirection::Right => (0, -step),
                ScrollDirection::Left => (0, step),
            };
            let (x, y) = super::desktop_screenshot_point(x, y);
            if let Some(refusal) =
                super::desktop_pointer_guard::check("scroll", &args, vec![(x, y)]).await
            {
                return refusal;
            }
            let result = latency::spawn_action_blocking(move || {
                crate::input::mouse::scroll_wheel_desktop(x, y, delta_y, delta_x, amount)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Scrolled desktop {direction} by {by} × {amount} at ({x:.1}, {y:.1})."
                ))
                .with_structured(serde_json::json!({
                    "scope": "desktop",
                    "path": "hid",
                    "effect": "unverifiable"
                })),
                Ok(Err(error)) => ToolResult::error(format!("desktop scroll failed: {error}")),
                Err(error) => ToolResult::error(format!("desktop scroll task failed: {error}")),
            };
        }
        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        // delivery_mode: foreground briefly fronts the window before the
        // pixel-wheel dispatch (the explicit last resort for surfaces that drop
        // background CGEvents). Only the pixel-wheel path honors it; the
        // keystroke path is background-by-design and untouched.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        // Electron/Chromium drop background wheel events and keystrokes; a
        // background scroll there goes through accessibility instead (below).
        let electron_background =
            !delivery_mode.is_foreground() && crate::browser::ElectronJs::is_electron(pid);
        let direction = match args.require_str("direction") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let by = args.str_or("by", "line");
        let amount = clamp_amount(args.u64_or("amount", 3));
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match latency::resolve(|| self.state.snapshots.resolve(pid, &args)) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (_, window_id, pre_focus_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        let pre_focus_ptr: Option<usize> = pre_focus_guard.as_ref().map(|g| g.as_ptr());

        let mut _mutation_lease: Option<super::BackgroundMutationLease> = None;

        // AppKit exposes vertical scroll-bar buttons beneath the text area's
        // AXScrollArea parent. Pressing those controls is a true
        // background-safe scroll: no activation, z-order change, or cursor move.
        if matches!(direction.as_str(), "up" | "down") {
            if let (Some(element_guard), Some(wid)) = (pre_focus_guard.clone(), window_id) {
                if !delivery_mode.is_foreground() {
                    if let Some(lease) = _mutation_lease.as_ref() {
                        if let Err(refusal_result) = lease
                            .gate_again(
                                wid,
                                pre_focus_ptr,
                                cua_driver_core::background_input::BackgroundAction::AxSemantic,
                            )
                            .await
                        {
                            return refusal_result;
                        }
                    } else {
                        match super::gate_background_window_action(
                            pid,
                            wid,
                            pre_focus_ptr,
                            cua_driver_core::background_input::BackgroundAction::AxSemantic,
                        )
                        .await
                        {
                            Ok(lease) => _mutation_lease = Some(lease),
                            Err(refusal_result) => return refusal_result,
                        }
                    }
                }
                let direction_for_ax = direction.clone();
                let by_for_ax = by.clone();
                let foreground = delivery_mode.is_foreground();
                let ax_result =
                    latency::spawn_action_blocking(move || -> anyhow::Result<(bool, bool)> {
                        if foreground {
                            let mut delivered = false;
                            let fronted = crate::input::skylight::with_foreground_assist(
                                pid as libc::pid_t,
                                wid,
                                || {
                                    delivered = unsafe {
                                        scroll_native_text_area(
                                            element_guard.as_ptr() as AXUIElementRef,
                                            &direction_for_ax,
                                            &by_for_ax,
                                            amount,
                                        )
                                    };
                                    std::thread::sleep(std::time::Duration::from_millis(100));
                                    Ok(())
                                },
                            )?;
                            Ok((delivered, fronted))
                        } else {
                            Ok((
                                unsafe {
                                    scroll_native_text_area(
                                        element_guard.as_ptr() as AXUIElementRef,
                                        &direction_for_ax,
                                        &by_for_ax,
                                        amount,
                                    )
                                },
                                false,
                            ))
                        }
                    })
                    .await;
                match ax_result {
                    Ok(Ok((true, fronted))) => {
                        return ToolResult::text(format!(
                        "✅ Scrolled native macOS control {direction} by {by} × {amount} through AX."
                    ))
                    .with_structured(serde_json::json!({
                        "path": if fronted { "ax_fg" } else { "ax" },
                        "verified": false,
                        "effect": "unverifiable"
                    }));
                    }
                    Ok(Ok((false, _))) => {}
                    Ok(Err(error)) => {
                        return ToolResult::error(format!("Native AX scroll failed: {error}"));
                    }
                    Err(error) => {
                        return ToolResult::error(format!("Native AX scroll task failed: {error}"));
                    }
                }
            }
        }

        // ── Targeted wheel path ─────────────────────────────────────────────
        // A target — element (preferred) OR window-local x,y — routes the scroll
        // through a synthesized mouse-wheel event at that screen point, so the
        // renderer's hit-test delivers it to whatever element is under the
        // cursor. This is the ONLY way to scroll a nested overflow:auto region
        // that never takes keyboard focus (the keystroke path below no-ops on
        // it). No user-facing flag: presence of a target IS the switch.
        let x_arg = args
            .opt_f64("x")
            .or_else(|| args.opt_i64("x").map(|v| v as f64));
        let y_arg = args
            .opt_f64("y")
            .or_else(|| args.opt_i64("y").map(|v| v as f64));

        // Per-notch step + direction→delta mapping (sign convention lives
        // here; the mouse primitive stays sign-agnostic). macOS: +y reveals
        // content ABOVE, -y reveals BELOW; +x reveals LEFT, -x reveals RIGHT.
        let step = if by == "page" {
            WHEEL_STEP_PAGE_PX
        } else {
            WHEEL_STEP_LINE_PX
        };
        let (delta_y, delta_x): (i32, i32) = match direction.as_str() {
            "down" => (-step, 0),
            "up" => (step, 0),
            "right" => (0, -step),
            "left" => (0, step),
            _ => (-step, 0),
        };

        // Resolve a screen-space wheel target, if a target was supplied.
        let wheel_target: Option<WheelTarget> = if let Some(element_ptr) = pre_focus_ptr {
            // Revealing an element is itself an AX mutation. Prove that the
            // cached element still belongs to the exact requested window before
            // AXScrollToVisible for every direction, then keep the lease for the
            // stricter pointer revalidation below.
            let semantic_gate = if !delivery_mode.is_foreground() {
                if let Some(wid) = window_id {
                    if let Some(lease) = _mutation_lease.as_ref() {
                        lease
                            .gate_again(
                                wid,
                                Some(element_ptr),
                                cua_driver_core::background_input::BackgroundAction::AxSemantic,
                            )
                            .await
                    } else {
                        match super::gate_background_window_action(
                            pid,
                            wid,
                            Some(element_ptr),
                            cua_driver_core::background_input::BackgroundAction::AxSemantic,
                        )
                        .await
                        {
                            Ok(lease) => {
                                _mutation_lease = Some(lease);
                                Ok(())
                            }
                            Err(refusal) => Err(refusal),
                        }
                    }
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            // Element path: wheel at the element's screen-space center. Both AX
            // coordinates and window bounds are logical top-left points, so no
            // Retina scaling is needed here.
            let wid = window_id;
            let target_guard = pre_focus_guard.clone();
            let target_task = latency::spawn_action_blocking(move || {
                let _target_guard = target_guard;
                // Web content can be present in AX while its frame is below
                // the outer page viewport. Ask the accessibility hierarchy to
                // reveal the target before taking the screen-space center;
                // otherwise the wheel is posted outside the rendered window
                // and nested overflow regions never receive it.
                semantic_gate?;
                unsafe {
                    crate::ax::bindings::perform_action(
                        element_ptr as AXUIElementRef,
                        "AXScrollToVisible",
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(40));
                let center = unsafe { element_screen_center(element_ptr as AXUIElementRef) };
                let rect = unsafe {
                    crate::ax::bindings::element_screen_rect(element_ptr as AXUIElementRef)
                };
                Ok(center.map(|(cx, cy)| {
                    let win_local = wid
                        .and_then(crate::windows::window_bounds_by_id)
                        .map(|b| (cx - b.x, cy - b.y));
                    WheelTarget {
                        screen_x: cx,
                        screen_y: cy,
                        win_local,
                        wid,
                        rect,
                    }
                }))
            });
            match target_task.await {
                Ok(Ok(target)) => target,
                Ok(Err(refusal)) => return refusal,
                Err(_) => None,
            }
        } else if let (Some(mut cx), Some(mut cy)) = (x_arg, y_arg) {
            // Targeted x,y are window-local screenshot pixels and REQUIRE a
            // window_id to anchor the window→screen conversion (schema contract).
            // Without one, refuse rather than scrolling at screen-absolute coords.
            if window_id.is_none() {
                return super::invalid_arguments(
                    "window_id is required when scrolling by window-local x,y pixels.".to_string(),
                );
            }
            // Pixel path: x,y are window-local screenshot pixels. Mirror the
            // click pixel path — undo any session downscale, then translate
            // through the shared window frame (which refuses a window with no
            // live frame rather than scrolling at screen-absolute coords).
            let ratio = match super::screenshot_scale(&self.state, &args, pid, window_id) {
                Ok(ratio) => ratio,
                Err(refusal) => return refusal,
            };
            cx *= ratio;
            cy *= ratio;
            let Some(wid) = window_id else {
                // Unreachable: the None case refused above. Kept explicit so a
                // future edit cannot reintroduce the screen-absolute fallback.
                return super::invalid_arguments(
                    "window_id is required when scrolling by window-local x,y pixels.".to_string(),
                );
            };
            match super::px_frame::resolve_or_refuse(wid).await {
                Ok(frame) => {
                    let (sx, sy, lx, ly) = frame.to_screen(cx, cy);
                    Some(WheelTarget {
                        screen_x: sx,
                        screen_y: sy,
                        win_local: Some((lx, ly)),
                        wid: Some(wid),
                        rect: None,
                    })
                }
                Err(refusal) => return refusal,
            }
        } else {
            None
        };

        if electron_background {
            return electron_reveal_scroll(
                pid,
                window_id,
                wheel_target
                    .as_ref()
                    .map(|target| (target.screen_x, target.screen_y)),
                &direction,
                (step.unsigned_abs() as usize * amount) as f64,
                pre_focus_ptr,
                &mut _mutation_lease,
            )
            .await;
        }

        if let Some(target) = wheel_target {
            if !delivery_mode.is_foreground() {
                if let Some(wid) = target.wid {
                    if let (Some((lx, ly)), Some(bounds)) =
                        (target.win_local, crate::windows::window_bounds_by_id(wid))
                    {
                        if lx < 0.0 || ly < 0.0 || lx > bounds.width || ly > bounds.height {
                            return super::invalid_arguments(format!(
                                "scroll: window-local point ({lx:.1}, {ly:.1}) pt lies outside \
                                 window {wid}'s {:.0}×{:.0} pt frame; background delivery \
                                 refused",
                                bounds.width, bounds.height
                            ));
                        }
                    }
                    // The semantic reveal gate does not authorize pointer
                    // delivery. Revalidate the stricter route immediately
                    // before proceeding to wheel dispatch.
                    if let Some(lease) = _mutation_lease.as_ref() {
                        if let Err(refusal_result) = lease
                            .gate_again(
                                wid,
                                pre_focus_ptr,
                                cua_driver_core::background_input::BackgroundAction::WindowPointer,
                            )
                            .await
                        {
                            return refusal_result;
                        }
                    } else {
                        match super::gate_background_window_action(
                            pid,
                            wid,
                            pre_focus_ptr,
                            cua_driver_core::background_input::BackgroundAction::WindowPointer,
                        )
                        .await
                        {
                            Ok(lease) => _mutation_lease = Some(lease),
                            Err(refusal_result) => return refusal_result,
                        }
                    }
                }
            }
            let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
            // Pin + glide the agent-cursor overlay to the target for visibility
            // (overlay only — does NOT move the hardware cursor). Mirrors click.
            if let Some(wid) = target.wid {
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
            }
            crate::cursor::overlay::animate_cursor_to_target(
                cursor_key.clone(),
                target.screen_x,
                target.screen_y,
                target.rect,
            )
            .await;
            self.state.cursor_registry.update_position(
                &cursor_key,
                target.screen_x,
                target.screen_y,
            );

            let prior_front = apps::frontmost_pid();
            let snapshot = WindowChangeDetector::snapshot(prior_front);

            let WheelTarget {
                screen_x,
                screen_y,
                win_local,
                wid,
                ..
            } = target;
            // A background wheel is posted to the pid and can be dropped without
            // trace (SwiftUI panes of System Settings ignore it). Read the scroll
            // area's bar first so the result can tell, and fall back to its page
            // actions when the wheel left it where it was.
            let probe_direction = direction.clone();
            let probe_element = pre_focus_ptr;
            let area_before = if delivery_mode.is_foreground() {
                None
            } else {
                latency::spawn_action_blocking(move || unsafe {
                    let area = crate::ax::scroll_area::ScrollArea::find(
                        pid,
                        probe_element.map(|ptr| ptr as AXUIElementRef),
                        (screen_x, screen_y),
                        wid,
                    )?;
                    let position = area.position(&probe_direction)?;
                    Some((area, position))
                })
                .await
                .ok()
                .flatten()
            };
            let amount_ticks = amount;
            let fg = delivery_mode.is_foreground() && wid.is_some();
            let result = focus_guard::with_focus_suppressed(
                Some(pid),
                prior_front,
                "scroll.CGScrollWheel",
                || async move {
                    latency::spawn_action_blocking(move || -> anyhow::Result<()> {
                        let do_it = move || -> anyhow::Result<()> {
                            crate::input::mouse::scroll_wheel_at_xy(
                                pid,
                                screen_x,
                                screen_y,
                                win_local,
                                wid,
                                delta_y,
                                delta_x,
                                amount_ticks,
                            )
                        };
                        // Foreground rung: brief front → wheel → restore prior frontmost.
                        match (fg, wid) {
                            (true, Some(w)) => {
                                crate::input::skylight::with_foreground_assist(
                                    pid as libc::pid_t,
                                    w,
                                    do_it,
                                )?;
                                Ok(())
                            }
                            _ => do_it(),
                        }
                    })
                    .await
                },
            )
            .await;

            let changes = super::finish_window_observation(snapshot).await;
            let mode_label = if fg {
                " (delivery_mode:foreground)"
            } else {
                ""
            };
            if let (Ok(Ok(())), Some((area, before))) = (&result, area_before) {
                let wheel_distance = step.unsigned_abs() as f64 * amount as f64;
                let verdict_direction = direction.clone();
                let verdict_window = wid
                    .and_then(crate::windows::window_bounds_by_id)
                    .map(|b| [b.x, b.y, b.width, b.height]);
                let verdict = latency::spawn_action_blocking(move || unsafe {
                    wheel_verdict(
                        &area,
                        &verdict_direction,
                        before,
                        wheel_distance,
                        amount,
                        verdict_window,
                    )
                })
                .await
                .unwrap_or(WheelVerdict::Unknown);
                if let Some(result) = verdict.result(
                    &direction,
                    &by,
                    amount,
                    (screen_x, screen_y),
                    &changes.result_suffix(),
                ) {
                    return result;
                }
            }
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "✅ Sent {direction} scroll by {by} × {amount} via pixel wheel at \
                     ({screen_x:.0}, {screen_y:.0}){mode_label} (background CGEvent; not \
                     driver-verified — confirm via screenshot).{}",
                    changes.result_suffix()
                ))
                .with_structured(serde_json::json!({
                    "path": if fg { "cgevent_fg" } else { "cgevent" }, "verified": false, "effect": "unverifiable"
                })),
                Ok(Err(e)) => ToolResult::error(format!("Wheel scroll failed: {e}")),
                Err(e)     => ToolResult::error(format!("Task error: {e}")),
            };
        }

        let key = match (by.as_str(), direction.as_str()) {
            ("page", "down") | (_, "down") if by == "page" => "pagedown",
            ("page", "up") | (_, "up") if by == "page" => "pageup",
            ("line", "down") | (_, "down") => "down",
            ("line", "up") | (_, "up") => "up",
            (_, "left") => "left",
            (_, "right") => "right",
            _ => "down",
        };
        let key = key.to_owned();

        if !delivery_mode.is_foreground() {
            if let Some(wid) = window_id {
                if let Some(lease) = _mutation_lease.as_ref() {
                    if let Err(refusal_result) = lease
                        .gate_again(
                            wid,
                            pre_focus_ptr,
                            cua_driver_core::background_input::BackgroundAction::GenericKey,
                        )
                        .await
                    {
                        return refusal_result;
                    }
                } else {
                    match super::gate_background_window_action(
                        pid,
                        wid,
                        pre_focus_ptr,
                        cua_driver_core::background_input::BackgroundAction::GenericKey,
                    )
                    .await
                    {
                        Ok(lease) => _mutation_lease = Some(lease),
                        Err(refusal_result) => return refusal_result,
                    }
                }
            }
        }

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // Scroll keystrokes (PageDown / arrow) into search-box autocomplete
        // can spawn floating helper windows; rare but real. Wrap for parity
        // with the other action tools.
        //
        // The AX focus_element() pre-write also runs inside the closure so
        // any reflex activations it triggers are caught by both the wildcard
        // snapshot suppressor and the targeted FocusGuard lease.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);

        // No target to aim a wheel at: read the focused element's scroll area
        // before and after the keys, so a scroll that moved it is confirmed.
        let focused_area = match (window_id, delivery_mode.is_foreground()) {
            (Some(wid), false) => {
                let probe_direction = direction.clone();
                latency::spawn_action_blocking(move || unsafe {
                    let focused = crate::ax::exact_target::focused_element_in_window(pid, wid)?;
                    let center = crate::ax::bindings::element_screen_center(focused);
                    let area = center.and_then(|point| {
                        crate::ax::scroll_area::ScrollArea::find(
                            pid,
                            Some(focused),
                            point,
                            Some(wid),
                        )
                    });
                    core_foundation::base::CFRelease(focused as CFTypeRef);
                    let area = area?;
                    let before = area.position(&probe_direction)?;
                    Some((area, before))
                })
                .await
                .ok()
                .flatten()
            }
            _ => None,
        };

        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "scroll.CGEvent",
            || async move {
                // Pre-focus the element under suppression so its
                // side-effects are captured by the snapshot + lease.
                if let Some(guard) = pre_focus_guard {
                    let _ = latency::spawn_action_blocking(move || {
                        crate::input::ax_actions::focus_element(guard.as_ptr())
                    })
                    .await;
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }

                latency::spawn_action_blocking(move || {
                    for _ in 0..amount {
                        crate::input::keyboard::press_key(pid, &key, &[])?;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Ok::<(), anyhow::Error>(())
                })
                .await
            },
        )
        .await;

        let changes = super::finish_window_observation(snapshot).await;

        if let (Ok(Ok(())), Some((area, before))) = (&result, focused_area) {
            let read_direction = direction.clone();
            let after = latency::spawn_action_blocking(move || {
                std::thread::sleep(std::time::Duration::from_millis(120));
                unsafe { area.position(&read_direction) }
            })
            .await
            .ok()
            .flatten();
            if let Some(outcome) = keystroke_read_back(
                &direction,
                &by,
                amount,
                before,
                after,
                &changes.result_suffix(),
            ) {
                return outcome;
            }
        }

        match result {
            Ok(Ok(())) => ToolResult::text(format!(
                "✅ Sent {direction} scroll by {by} × {amount} via keystroke \
                 (background; not driver-verified — confirm via screenshot).{}",
                changes.result_suffix()
            ))
            .with_structured(serde_json::json!({ "path": "key_events", "verified": false })),
            Ok(Err(e)) => ToolResult::error(format!("Scroll failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// Background scroll for Electron/Chromium windows, which drop background
/// What the focused scroll area's bar says about a keystroke scroll: confirmed
/// when it moved, suspected_noop when it already sat at the end the keys
/// scroll toward. None when it did not move otherwise, since the keys may
/// have scrolled something other than that area.
fn keystroke_read_back(
    direction: &str,
    by: &str,
    amount: usize,
    before: f64,
    after: Option<f64>,
    suffix: &str,
) -> Option<ToolResult> {
    let after = after?;
    if crate::ax::scroll_area::moved(before, after) {
        return Some(
            ToolResult::text(format!(
                "✅ Scrolled {direction} by {by} × {amount} via keystroke; the focused scroll \
                 area's bar moved from {before:.3} to {after:.3}.{suffix}"
            ))
            .with_structured(serde_json::json!({
                "path": "key_events",
                "verified": true,
                "effect": "confirmed",
                "scroll_position": { "before": before, "after": after },
                // The dispatch seam keeps "confirmed" only with published
                // read-back evidence, as for the wheel verdicts below.
                "evidence": [{
                    "kind": "accessibility_readback",
                    "detail": format!("scroll bar {before:.3} -> {after:.3}"),
                }],
            })),
        );
    }
    crate::ax::scroll_area::at_end(direction, before).then(|| {
        ToolResult::text(format!(
            "The focused scroll area was already at its end ({before:.3}); the {direction} keys \
             changed nothing there.{suffix}"
        ))
        .with_structured(serde_json::json!({
            "path": "key_events",
            "verified": true,
            "effect": "suspected_noop",
            "at_edge": true,
            "scroll_position": { "before": before, "after": after },
        }))
    })
}

/// Background scroll for Electron/Chromium windows, which drop background
/// wheel events: reveal the content just past the edge of the scroll
/// container under `point` (the window's center when there is no target)
/// through accessibility. Refuses as before when no container moves.
async fn electron_reveal_scroll(
    pid: i32,
    window_id: Option<u32>,
    point: Option<(f64, f64)>,
    direction: &str,
    distance: f64,
    pre_focus_ptr: Option<usize>,
    lease: &mut Option<super::BackgroundMutationLease>,
) -> ToolResult {
    let unavailable = || {
        cua_driver_core::delivery::background_unavailable_result(
            ELECTRON_BACKGROUND_SCROLL_UNAVAILABLE,
            "background_unavailable",
            "Electron/Chromium windows drop background wheel events and keystrokes on macOS",
            serde_json::json!({ "effect": "refused" }),
        )
    };
    let window = window_id.and_then(crate::windows::window_bounds_by_id);
    let Some(point) = point.or_else(|| {
        window
            .as_ref()
            .map(|b| (b.x + b.width / 2.0, b.y + b.height / 2.0))
    }) else {
        return unavailable();
    };
    if let Some(wid) = window_id {
        let gate = match lease.as_ref() {
            Some(lease) => {
                lease
                    .gate_again(
                        wid,
                        pre_focus_ptr,
                        cua_driver_core::background_input::BackgroundAction::AxSemantic,
                    )
                    .await
            }
            None => super::gate_background_window_action(
                pid,
                wid,
                pre_focus_ptr,
                cua_driver_core::background_input::BackgroundAction::AxSemantic,
            )
            .await
            .map(|granted| *lease = Some(granted)),
        };
        if let Err(refusal) = gate {
            return refusal;
        }
    }
    let window_rect = window.map(|b| [b.x, b.y, b.width, b.height]);
    let direction_owned = direction.to_owned();
    let outcome = latency::spawn_action_blocking(move || unsafe {
        crate::ax::reveal_scroll::reveal_scroll(pid, point, window_rect, &direction_owned, distance)
    })
    .await;
    match outcome {
        Ok(Some(outcome)) => ToolResult::text(format!(
            "✅ Scrolled {direction} about {:.0} pt in the background through accessibility \
             (revealed the content just past the {}'s edge; this Electron/Chromium window drops \
             background wheel events).",
            outcome.moved, outcome.container_role
        ))
        .with_structured(serde_json::json!({
            "path": "ax",
            "verified": false,
            "effect": "unverifiable",
        })),
        _ => unavailable(),
    }
}

/// What a background wheel did to the scroll area read before it.
enum WheelVerdict {
    /// The bar moved: the wheel landed.
    Moved { before: f64, after: f64 },
    /// The bar was already at the end the wheel scrolls toward.
    AtEnd,
    /// The wheel left the bar alone; accessibility moved it instead.
    Accessibility {
        route: &'static str,
        before: f64,
        after: f64,
    },
    /// Neither the wheel nor accessibility moved the bar.
    Dropped { tried_accessibility: bool },
    /// The area could not be read again.
    Unknown,
}

/// Reads the bar after a background wheel and, when it did not move, scrolls
/// the area through its page actions instead.
///
/// # Safety
///
/// Performs AX calls; call from a blocking context.
unsafe fn wheel_verdict(
    area: &crate::ax::scroll_area::ScrollArea,
    direction: &str,
    before: f64,
    wheel_distance: f64,
    amount: usize,
    window: Option<[f64; 4]>,
) -> WheelVerdict {
    use crate::ax::scroll_area::{at_end, moved, pages_for};
    let settle = || std::thread::sleep(std::time::Duration::from_millis(80));
    // The wheel's last notch was posted 30 ms ago; let the app apply it.
    settle();
    let Some(after) = area.position(direction) else {
        return WheelVerdict::Unknown;
    };
    if moved(before, after) {
        return WheelVerdict::Moved { before, after };
    }
    if at_end(direction, before) {
        return WheelVerdict::AtEnd;
    }
    // Revealing content past the edge and setting the bar keep the requested
    // distance; page actions are the coarse last resort (AppKit scroll views
    // can advertise them and still refuse them).
    let mut tried = false;
    if area.reveal(window, direction, wheel_distance) {
        tried = true;
        settle();
        if let Some(after) = area
            .position(direction)
            .filter(|after| moved(before, *after))
        {
            return WheelVerdict::Accessibility {
                route: "AXScrollToVisible",
                before,
                after,
            };
        }
    }
    if area.set_position(direction, wheel_distance) {
        tried = true;
        settle();
        if let Some(after) = area
            .position(direction)
            .filter(|after| moved(before, *after))
        {
            return WheelVerdict::Accessibility {
                route: "scroll bar value",
                before,
                after,
            };
        }
    }
    let pages = pages_for(
        wheel_distance,
        area.extent(direction).unwrap_or(0.0),
        amount,
    );
    if area.page(direction, pages) {
        tried = true;
        settle();
        if let Some(after) = area
            .position(direction)
            .filter(|after| moved(before, *after))
        {
            return WheelVerdict::Accessibility {
                route: "page action",
                before,
                after,
            };
        }
    }
    WheelVerdict::Dropped {
        tried_accessibility: tried,
    }
}

impl WheelVerdict {
    /// The tool result for this verdict; `None` keeps the unverified wheel
    /// result as before.
    fn result(
        &self,
        direction: &str,
        by: &str,
        amount: usize,
        (screen_x, screen_y): (f64, f64),
        suffix: &str,
    ) -> Option<ToolResult> {
        let sent = format!(
            "Sent {direction} scroll by {by} × {amount} via pixel wheel at \
             ({screen_x:.0}, {screen_y:.0}) (background CGEvent)"
        );
        let readback = |before: &f64, after: &f64| {
            serde_json::json!([{
                "kind": "accessibility_readback",
                "detail": format!("scroll bar {before:.3} -> {after:.3}")
            }])
        };
        Some(match self {
            WheelVerdict::Moved { before, after } => ToolResult::text(format!(
                "✅ {sent}; the scroll area's position changed.{suffix}"
            ))
            .with_structured(serde_json::json!({
                "path": "cgevent", "verified": true, "effect": "confirmed",
                "evidence": readback(before, after)
            })),
            WheelVerdict::AtEnd => ToolResult::text(format!(
                "{sent}; the scroll area was already at its {direction} end, so nothing moved.{suffix}"
            ))
            .with_structured(serde_json::json!({
                "path": "cgevent", "verified": true, "effect": "suspected_noop"
            })),
            WheelVerdict::Accessibility { route, before, after } => ToolResult::text(format!(
                "✅ The background wheel did not move this scroll area, so it was scrolled \
                 {direction} through accessibility ({route}) instead: no event posted, \
                 nothing fronted.{suffix}"
            ))
            .with_structured(serde_json::json!({
                "path": "ax", "verified": true, "effect": "confirmed",
                "evidence": readback(before, after)
            })),
            WheelVerdict::Dropped { tried_accessibility } => ToolResult::text(format!(
                "{sent}, but the scroll area did not move{}. This window ignores background \
                 scrolling; use delivery_mode:foreground if foreground input is allowed.{suffix}",
                if *tried_accessibility { ", and neither did accessibility scrolling" } else { "" }
            ))
            .with_structured(serde_json::json!({
                "path": if *tried_accessibility { "ax" } else { "cgevent" },
                "verified": true,
                "effect": "suspected_noop"
            })),
            WheelVerdict::Unknown => return None,
        })
    }
}

unsafe fn scroll_native_text_area(
    element: AXUIElementRef,
    direction: &str,
    by: &str,
    amount: usize,
) -> bool {
    if copy_string_attr(element, "AXRole").as_deref() != Some("AXTextArea") {
        return false;
    }
    let Some(scroll_area) = copy_element_attr(element, "AXParent") else {
        return false;
    };
    if copy_string_attr(scroll_area, "AXRole").as_deref() != Some("AXScrollArea") {
        CFRelease(scroll_area as CFTypeRef);
        return false;
    }
    let mut buttons = Vec::new();
    collect_ax_buttons(scroll_area, 0, &mut buttons);
    CFRelease(scroll_area as CFTypeRef);
    if buttons.is_empty() {
        return false;
    }

    let reverse = direction == "up";
    let base = if by == "page" && buttons.len() >= 4 {
        2
    } else {
        0
    };
    let index = base + usize::from(reverse);
    let mut delivered = false;
    if let Some(target) = buttons.get(index).copied() {
        for _ in 0..amount.max(1) {
            if perform_action(target, "AXPress") != kAXErrorSuccess {
                break;
            }
            delivered = true;
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
    for button in buttons {
        CFRelease(button as CFTypeRef);
    }
    delivered
}

unsafe fn collect_ax_buttons(
    element: AXUIElementRef,
    depth: usize,
    buttons: &mut Vec<AXUIElementRef>,
) {
    if depth >= 4 || buttons.len() >= 4 {
        return;
    }
    for child in copy_children(element) {
        if buttons.len() >= 4 {
            CFRelease(child as CFTypeRef);
        } else if copy_string_attr(child, "AXRole").as_deref() == Some("AXButton") {
            buttons.push(child);
        } else {
            collect_ax_buttons(child, depth + 1, buttons);
            CFRelease(child as CFTypeRef);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_is_clamped_to_the_advertised_range() {
        let amount = &def().input_schema["properties"]["amount"];
        assert_eq!(amount["minimum"], serde_json::json!(AMOUNT_MIN));
        assert_eq!(amount["maximum"], serde_json::json!(AMOUNT_MAX));
        assert_eq!(clamp_amount(1100), AMOUNT_MAX as usize);
        assert_eq!(clamp_amount(0), AMOUNT_MIN as usize);
        assert_eq!(clamp_amount(3), 3);
    }

    #[test]
    fn a_keystroke_scroll_is_judged_by_the_focused_scroll_bar() {
        let moved = keystroke_read_back("down", "page", 1, 0.0, Some(0.4), "").unwrap();
        let structured = moved.structured_content.unwrap();
        assert_eq!(structured["effect"], "confirmed");
        assert_eq!(structured["scroll_position"]["after"], 0.4);
        let at_end = keystroke_read_back("down", "line", 3, 1.0, Some(1.0), "").unwrap();
        assert_eq!(
            at_end.structured_content.unwrap()["effect"],
            "suspected_noop"
        );
        // Unmoved mid-range or unreadable afterwards proves nothing either way.
        assert!(keystroke_read_back("down", "line", 3, 0.5, Some(0.5), "").is_none());
        assert!(keystroke_read_back("up", "line", 3, 0.5, None, "").is_none());
    }

    /// Returns what a keystroke scroll reads back from a moved scroll bar, so
    /// the test sees that result after the registry publishes it.
    struct KeystrokeReadBackProbe;

    #[async_trait]
    impl Tool for KeystrokeReadBackProbe {
        fn def(&self) -> &ToolDef {
            def()
        }

        async fn invoke(&self, _args: Value) -> ToolResult {
            keystroke_read_back("down", "page", 1, 0.0, Some(0.4), "").unwrap()
        }
    }

    #[tokio::test]
    async fn a_confirmed_keystroke_scroll_stays_confirmed_through_dispatch() {
        let mut registry = cua_driver_core::tool::ToolRegistry::new();
        registry.register(Box::new(KeystrokeReadBackProbe));
        let result = registry
            .invoke(
                "scroll",
                serde_json::json!({"pid": 123, "window_id": 7, "direction": "down", "by": "page"}),
            )
            .await;
        let structured = result.structured_content.expect("published action result");
        assert_eq!(structured["effect"], "confirmed", "{structured}");
        // The public contract projects every read-back kind as value_readback.
        assert_eq!(
            structured["evidence"][0]["kind"], "value_readback",
            "{structured}"
        );
        assert_eq!(
            structured["evidence"][0]["detail"], "scroll bar 0.000 -> 0.400",
            "{structured}"
        );
    }

    #[tokio::test]
    async fn missing_window_id_is_refused_before_a_pixel_scroll() {
        let tool = ScrollTool::new(Arc::new(ToolState::new(false, false, None)));
        let result = tool
            .invoke(serde_json::json!({"pid": 123, "direction": "down", "x": 1, "y": 1}))
            .await;
        let structured = result.structured_content.expect("structured refusal");
        assert_eq!(structured["code"], "invalid_arguments");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
    }
}
