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
                "coordinate_space": super::window_points::coordinate_space_schema(),
                "window_basis": super::window_points::window_basis_schema(),
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

impl ScrollTool {
    async fn invoke_in(&self, args: Value, space: super::window_points::PointSpace) -> ToolResult {
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
        let x_arg = args
            .opt_f64("x")
            .or_else(|| args.opt_i64("x").map(|v| v as f64));
        let y_arg = args
            .opt_f64("y")
            .or_else(|| args.opt_i64("y").map(|v| v as f64));
        // Argument shape before any live read: window-local x,y with no
        // element and no window cannot be anchored (the targeted wheel path
        // below refuses the same case; an element target wins over x,y).
        if pre_focus_guard.is_none() && window_id.is_none() && x_arg.is_some() && y_arg.is_some() {
            return super::invalid_arguments(
                "window_id is required when scrolling by window-local x,y pixels.".to_string(),
            );
        }
        // Electron/Chromium drop background wheel events and keystrokes; a
        // background scroll there goes through accessibility instead (below).
        let electron_background =
            !delivery_mode.is_foreground() && crate::browser::ElectronJs::is_electron(pid);
        // The person is working in this app: nothing is sent.
        if !delivery_mode.is_foreground() {
            if let Err(refusal) = super::gate_user_takeover(pid, window_id) {
                return refusal;
            }
        }
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
        // The window-points frame the target was translated with (pixel x, y).
        let mut validated_frame: Option<super::px_frame::WindowPxFrame> = None;
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
            let Some(wid) = window_id else {
                // Unreachable: the None case refused above. Kept explicit so a
                // future edit cannot reintroduce the screen-absolute fallback.
                return super::invalid_arguments(
                    "window_id is required when scrolling by window-local x,y pixels.".to_string(),
                );
            };
            match super::window_points::frame_or_resolve(validated_frame.clone(), wid).await {
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
                (space, validated_frame.clone()),
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
                    use crate::ax::scroll_area::{Budget, ScrollArea, SystemClock, WheelArea};
                    // Bounded: a busy app must not hold the scroll before the
                    // wheel is even sent. Past the budget the wheel goes out
                    // unverified, as it does without a scroll area.
                    let budget = Budget::new(&SystemClock, PROBE_BUDGET);
                    let area = ScrollArea::find_within(
                        pid,
                        probe_element.map(|ptr| ptr as AXUIElementRef),
                        (screen_x, screen_y),
                        wid,
                        &budget,
                    )?;
                    let position = WheelArea::position(&area, &probe_direction, &budget)?;
                    Some((area, position))
                })
                .await
                .ok()
                .flatten()
            };
            let amount_ticks = amount;
            if let Err(refusal) =
                super::window_points::recheck_now(space, wid, validated_frame.as_ref())
            {
                return refusal;
            }
            let fg = delivery_mode.is_foreground() && wid.is_some();
            let dispatch_guard =
                match super::window_points::DispatchGuard::new(space, wid, validated_frame.clone())
                {
                    Ok(guard) => guard,
                    Err(refusal) => return refusal,
                };
            let result = focus_guard::with_focus_suppressed(
                Some(pid),
                prior_front,
                "scroll.CGScrollWheel",
                || async move {
                    latency::spawn_action_blocking(move || -> anyhow::Result<()> {
                        let do_it = move || -> anyhow::Result<()> {
                            super::window_points::check_guard(&dispatch_guard)?;
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
            if let Ok(Err(error)) = &result {
                if let Some(refusal) = super::window_points::moved_refusal(error, space) {
                    return refusal;
                }
            }
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
                let verdict = latency::spawn_action_blocking(move || {
                    wheel_verdict(
                        &area,
                        &verdict_direction,
                        before,
                        wheel_distance,
                        amount,
                        verdict_window,
                        &crate::ax::scroll_area::SystemClock,
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
#[allow(clippy::too_many_arguments)]
async fn electron_reveal_scroll(
    (space, validated_frame): (
        super::window_points::PointSpace,
        Option<super::px_frame::WindowPxFrame>,
    ),
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
    if let Err(refusal) =
        super::window_points::recheck_now(space, window_id, validated_frame.as_ref())
    {
        return refusal;
    }
    let dispatch_guard =
        match super::window_points::DispatchGuard::new(space, window_id, validated_frame.clone()) {
            Ok(guard) => guard,
            Err(refusal) => return refusal,
        };
    let window_rect = window.map(|b| [b.x, b.y, b.width, b.height]);
    let direction_owned = direction.to_owned();
    let outcome = latency::spawn_action_blocking(move || unsafe {
        // Checked after the hit test and the walk, right before the reveal.
        let moved = std::cell::Cell::new(None);
        // Latched: once refused, refused for every later candidate.
        let proceed = || {
            if moved.get().is_some() {
                return false;
            }
            match super::window_points::check_guard(&dispatch_guard) {
                Ok(()) => true,
                Err(error) => {
                    moved.set(Some(error));
                    false
                }
            }
        };
        let limits = crate::ax::reveal_scroll::RevealLimits {
            proceed: Some(&proceed),
            ..crate::ax::reveal_scroll::electron_limits()
        };
        let run = crate::ax::reveal_scroll::reveal_scroll_with(
            pid,
            point,
            window_rect,
            &direction_owned,
            distance,
            limits,
        );
        (run, moved.get())
    })
    .await;
    let (run, moved) = match outcome {
        Ok(pair) => pair,
        Err(_) => return unavailable(),
    };
    match classify_electron_reveal(run, moved) {
        ElectronReveal::Scrolled(outcome) => ToolResult::text(format!(
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
        ElectronReveal::MovedBeforeAny(moved) => {
            let error = anyhow::Error::from(moved);
            super::window_points::moved_refusal(&error, space).unwrap_or_else(unavailable)
        }
        ElectronReveal::Attempted { moved_after } => ToolResult::text(format!(
            "An accessibility reveal was sent to scroll {direction}, but no movement could be \
             read back{}; it may have scrolled. Read the window before scrolling again.",
            if moved_after {
                " before the window moved"
            } else {
                ""
            }
        ))
        .with_structured(serde_json::json!({
            "path": "ax", "verified": false, "effect": "unverifiable"
        })),
        ElectronReveal::NothingSent => unavailable(),
    }
}

/// How an Electron background reveal ended, for its result.
#[derive(Debug, PartialEq)]
enum ElectronReveal {
    /// A candidate's reveal moved its content.
    Scrolled(crate::ax::reveal_scroll::RevealOutcome),
    /// The window moved before any reveal was sent: nothing sent.
    MovedBeforeAny(super::window_points::WindowMoved),
    /// A reveal was sent (or answered with an error) and nothing moved that
    /// could be read back; it may still have scrolled.
    Attempted { moved_after: bool },
    /// No candidate sent anything.
    NothingSent,
}

fn classify_electron_reveal(
    run: crate::ax::reveal_scroll::CandidateRun<crate::ax::reveal_scroll::RevealOutcome>,
    moved: Option<super::window_points::WindowMoved>,
) -> ElectronReveal {
    match (run.outcome, run.attempted, moved) {
        (Some(outcome), _, _) => ElectronReveal::Scrolled(outcome),
        (None, true, moved) => ElectronReveal::Attempted {
            moved_after: moved.is_some(),
        },
        (None, false, Some(moved)) => ElectronReveal::MovedBeforeAny(moved),
        (None, false, None) => ElectronReveal::NothingSent,
    }
}

/// What a background wheel did to the scroll area read before it.
#[derive(Debug, Clone, PartialEq)]
enum WheelVerdict {
    /// The bar moved: the wheel landed.
    Moved { before: f64, after: f64 },
    /// The bar was already at the end the wheel scrolls toward.
    AtEnd,
    /// The bar moved after the wheel and an accessibility fallback; which of
    /// them (or the app itself) moved it is not known.
    MovedAfterFallback {
        before: f64,
        after: f64,
        attempted: String,
        distance: DistanceCheck,
    },
    /// Neither the wheel nor accessibility moved the bar.
    Dropped { tried_accessibility: bool },
    /// An accessibility scroll was sent (or may have been: the app answered
    /// with an error or not in time) and its effect could not be read back.
    Unverified { attempted: String },
    /// The area could not be read again (before any fallback was sent).
    Unknown,
}

/// How far an accessibility fallback moved the bar compared with the
/// request, in the bar's 0..1 units.
#[derive(Debug, Clone, PartialEq)]
enum DistanceCheck {
    /// Content or viewport could not be measured, so it was not compared.
    NotMeasured,
    /// Within 1.5x of the request (or clamped at an end).
    AsRequested,
    /// Further than 1.5x the request: `later` is the bar on the extra read.
    Overshoot { later: f64, requested: f64 },
    /// The extra read found the bar back at or past where it started.
    Reversed { later: f64 },
}

/// A background-wheel fallback route, in the order the verdict tries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    ScrollBar,
    Reveal,
    Page,
}

impl Fallback {
    fn route(self) -> &'static str {
        match self {
            Fallback::ScrollBar => "scroll bar value",
            Fallback::Reveal => "AXScrollToVisible",
            Fallback::Page => "page action",
        }
    }
}

/// A settable bar is one exact write of the requested distance, so it goes
/// first; the reveal walk can cost seconds on a long list (R3: KakaoTalk).
/// Without a settable bar the reveal keeps its old place ahead of pages.
fn fallback_order(bar_settable: bool) -> &'static [Fallback] {
    if bar_settable {
        &[Fallback::ScrollBar, Fallback::Reveal, Fallback::Page]
    } else {
        &[Fallback::Reveal, Fallback::ScrollBar, Fallback::Page]
    }
}

/// Everything the verdict does after the wheel, bounded.
const VERDICT_BUDGET: std::time::Duration = std::time::Duration::from_millis(1200);
/// How long a late wheel is waited for before a fallback runs.
const WHEEL_POLL_FOR: std::time::Duration = std::time::Duration::from_millis(250);
const WHEEL_POLL_EVERY: std::time::Duration = std::time::Duration::from_millis(20);
/// No route starts with less than this left.
const MIN_ROUTE: std::time::Duration = std::time::Duration::from_millis(150);
/// The reveal walk's own cap inside the verdict budget.
const REVEAL_BUDGET: std::time::Duration = std::time::Duration::from_millis(600);
const FALLBACK_SETTLE: std::time::Duration = std::time::Duration::from_millis(80);
/// The probe before the wheel: finding the area and reading its bar.
const PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(400);

/// Reads the bar after a background wheel and, when it did not move, scrolls
/// the area through accessibility instead, within [`VERDICT_BUDGET`].
///
/// A mutation that was sent and read back unmoved lets the next route run
/// (an AppKit scroll view can advertise page actions and refuse them). A
/// mutation the app answered with an error or not in time may still apply,
/// so nothing else is sent: one more read decides between confirmed and
/// unverified.
fn wheel_verdict<A: crate::ax::scroll_area::WheelArea>(
    area: &A,
    direction: &str,
    before: f64,
    wheel_distance: f64,
    amount: usize,
    window: Option<[f64; 4]>,
    clock: &dyn crate::ax::scroll_area::Clock,
) -> WheelVerdict {
    use crate::ax::scroll_area::{at_end, moved_toward, pages_for, Budget, Mutation};
    let moved = |before: f64, after: f64| moved_toward(direction, before, after);
    let budget = Budget::new(clock, VERDICT_BUDGET);
    // The wheel's last notch was posted 30 ms ago; wait for the app to apply
    // it, reading the bar as it goes.
    let poll = budget.sooner(WHEEL_POLL_FOR);
    loop {
        poll.pause(WHEEL_POLL_EVERY);
        match area.position(direction, &budget) {
            None => return WheelVerdict::Unknown,
            Some(after) if moved(before, after) => {
                return WheelVerdict::Moved { before, after };
            }
            // The bar moved the other way: something else is scrolling this
            // area, so neither "at the end" nor a fallback is safe to claim.
            Some(after) if crate::ax::scroll_area::moved(before, after) => {
                return WheelVerdict::Unknown;
            }
            Some(_) => {}
        }
        if poll.expired() {
            break;
        }
    }
    if at_end(direction, before) {
        return WheelVerdict::AtEnd;
    }
    let mut attempted: Vec<String> = Vec::new();
    let mut ran_out = false;
    for &route in fallback_order(area.bar_settable(direction, &budget)) {
        if budget.left() < MIN_ROUTE {
            ran_out = true;
            break;
        }
        let mutation = match route {
            Fallback::ScrollBar => area.set_position(direction, wheel_distance, &budget),
            Fallback::Reveal => area.reveal(
                window,
                direction,
                wheel_distance,
                &budget.sooner(REVEAL_BUDGET),
            ),
            Fallback::Page => {
                let pages = pages_for(
                    wheel_distance,
                    area.extent(direction, &budget).unwrap_or(0.0),
                    amount,
                );
                area.page(direction, pages, &budget)
            }
        };
        let (count, failed) = match mutation {
            Mutation::NotSent => continue,
            Mutation::Sent { count } => (count, false),
            Mutation::Failed { count } => (count, true),
        };
        attempted.push(attempt_label(route, count, failed));
        budget.pause(FALLBACK_SETTLE);
        match area.position(direction, &budget) {
            // Movement after a fallback was sent: a wheel arriving after the
            // poll, the fallback or the app itself may have moved it, so it is
            // reported without a cause.
            Some(after) if moved(before, after) => {
                let distance =
                    distance_check(area, direction, before, after, wheel_distance, &budget);
                if let DistanceCheck::Reversed { later } = distance {
                    return WheelVerdict::Unverified {
                        attempted: format!(
                            "{}; the bar moved ({before:.3} → {after:.3}) and then back ({later:.3})",
                            attempted.join(", ")
                        ),
                    };
                }
                return WheelVerdict::MovedAfterFallback {
                    before,
                    after,
                    attempted: attempted.join(", "),
                    distance,
                };
            }
            // The bar moved the other way: not this scroll, and not a no-op.
            Some(after) if crate::ax::scroll_area::moved(before, after) => {
                return WheelVerdict::Unverified {
                    attempted: attempted.join(", "),
                };
            }
            Some(_) if !failed => {}
            _ => {
                return WheelVerdict::Unverified {
                    attempted: attempted.join(", "),
                }
            }
        }
    }
    // A last route that gave up because the budget ran out (NotSent) leaves
    // no next iteration to notice it.
    let ran_out = ran_out || budget.left() < MIN_ROUTE;
    match (attempted.is_empty(), ran_out) {
        (true, true) => WheelVerdict::Unknown,
        // Something was sent and the budget ran out before the ladder ended:
        // it may still apply, so this is not a verified no-op.
        (false, true) => WheelVerdict::Unverified {
            attempted: attempted.join(", "),
        },
        (no_attempt, false) => WheelVerdict::Dropped {
            tried_accessibility: !no_attempt,
        },
    }
}

fn attempt_label(route: Fallback, count: usize, failed: bool) -> String {
    let what = match (route, count) {
        (Fallback::Page, count) if count != 1 => format!("{} ({count} pages)", route.route()),
        _ => route.route().to_owned(),
    };
    if failed {
        format!("{what}, which the app answered with an error or not in time")
    } else {
        what
    }
}

/// Compares the fallback's move with the request in bar units, with one more
/// read for movement that kept coming after the first one.
fn distance_check<A: crate::ax::scroll_area::WheelArea>(
    area: &A,
    direction: &str,
    before: f64,
    after: f64,
    wheel_distance: f64,
    budget: &crate::ax::scroll_area::Budget,
) -> DistanceCheck {
    use crate::ax::scroll_area::{moved_toward, signed_progress};
    budget.pause(FALLBACK_SETTLE);
    let later = area.position(direction, budget).unwrap_or(after);
    if !moved_toward(direction, before, later) {
        return DistanceCheck::Reversed { later };
    }
    let Some(requested) = area.bar_units(direction, wheel_distance, budget) else {
        return DistanceCheck::NotMeasured;
    };
    let clamped = |value: f64| value <= 1e-4 || value >= 1.0 - 1e-4;
    if !clamped(later) && signed_progress(direction, before, later) > 1.5 * requested {
        DistanceCheck::Overshoot { later, requested }
    } else {
        DistanceCheck::AsRequested
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
            WheelVerdict::MovedAfterFallback { before, after, attempted, distance } => {
                let note = match distance {
                    DistanceCheck::NotMeasured => {
                        " The content length could not be measured, so the distance was not checked."
                            .to_owned()
                    }
                    DistanceCheck::AsRequested | DistanceCheck::Reversed { .. } => String::new(),
                    DistanceCheck::Overshoot { later, .. } => format!(
                        " The content moved further than requested (bar {before:.3} → {later:.3})."
                    ),
                };
                let mut structured = serde_json::json!({
                    "path": "ax", "verified": true, "effect": "confirmed",
                    "evidence": readback(before, after),
                    "attempted": attempted,
                    "cause_known": false,
                    "distance_measured": !matches!(distance, DistanceCheck::NotMeasured),
                });
                if let DistanceCheck::Overshoot { later, requested } = distance {
                    structured["overshoot"] = serde_json::json!({
                        "before": before, "after": later, "requested": requested
                    });
                }
                ToolResult::text(format!(
                    "✅ {sent}; it did not visibly move the scroll area within 250 ms, so \
                     accessibility was tried ({attempted}). The bar then moved \
                     ({before:.3} → {after:.3}); whether the wheel, that attempt or the app moved \
                     it is not known. Nothing was fronted.{note}{suffix}"
                ))
                .with_structured(structured)
            }
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
            WheelVerdict::Unverified { attempted } => ToolResult::text(format!(
                "{sent}; the scroll area did not visibly move, so it was then scrolled through \
                 accessibility ({attempted}), but the result could not be read back in time. \
                 Read the window before scrolling again.{suffix}"
            ))
            .with_structured(serde_json::json!({
                "path": "ax", "verified": false, "effect": "unverifiable",
                "attempted": attempted
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
    fn an_electron_reveal_that_was_sent_is_never_reported_as_refused() {
        use super::{classify_electron_reveal, ElectronReveal};
        use crate::ax::reveal_scroll::{first_outcome, CandidateRun, RevealOutcome};
        use crate::ax::scroll_area::Mutation;
        use crate::tools::window_points::WindowMoved;
        // Sent or answered with an error, nothing read back, window still.
        for mutation in [Mutation::Sent { count: 1 }, Mutation::Failed { count: 1 }] {
            let run = first_outcome(
                vec![1],
                |_| (mutation, None::<RevealOutcome>),
                || false,
                |_| {},
            );
            assert_eq!(
                classify_electron_reveal(run, None),
                ElectronReveal::Attempted { moved_after: false },
                "{mutation:?}"
            );
        }
        let moved = WindowMoved { window_id: 7 };
        let attempted = CandidateRun {
            outcome: None,
            attempted: true,
            stopped: true,
        };
        assert_eq!(
            classify_electron_reveal(attempted, Some(moved)),
            ElectronReveal::Attempted { moved_after: true }
        );
        let untried = CandidateRun {
            outcome: None,
            attempted: false,
            stopped: true,
        };
        assert_eq!(
            classify_electron_reveal(untried, Some(moved)),
            ElectronReveal::MovedBeforeAny(moved)
        );
        let nothing = CandidateRun::<RevealOutcome> {
            outcome: None,
            attempted: false,
            stopped: false,
        };
        assert_eq!(
            classify_electron_reveal(nothing, None),
            ElectronReveal::NothingSent
        );
    }

    mod wheel_verdict_tests {
        use super::super::{fallback_order, wheel_verdict, DistanceCheck, Fallback, WheelVerdict};
        use crate::ax::scroll_area::{Budget, Clock, Mutation, WheelArea};
        use std::cell::{Cell, RefCell};
        use std::time::{Duration, Instant};

        struct FakeClock(Cell<Instant>);
        impl FakeClock {
            fn new() -> Self {
                Self(Cell::new(Instant::now()))
            }
            fn advance(&self, by: Duration) {
                self.0.set(self.0.get() + by);
            }
        }
        impl Clock for FakeClock {
            fn now(&self) -> Instant {
                self.0.get()
            }
            fn sleep(&self, duration: Duration) {
                self.advance(duration);
            }
        }

        /// What a fake mutation does: its answer, its cost, and the bar
        /// value it leaves after a delay (a late apply when the delay is long).
        #[derive(Clone, Copy)]
        struct Effect {
            answer: Mutation,
            cost: Duration,
            bar: Option<(Duration, f64)>,
        }
        const NOT_SENT: Effect = Effect {
            answer: Mutation::NotSent,
            cost: Duration::ZERO,
            bar: None,
        };
        fn sent(bar: f64) -> Effect {
            Effect {
                answer: Mutation::Sent { count: 1 },
                cost: Duration::from_millis(5),
                bar: Some((Duration::ZERO, bar)),
            }
        }

        struct FakeArea<'c> {
            clock: &'c FakeClock,
            start: Instant,
            /// (offset from start, bar value), applied in order.
            timeline: RefCell<Vec<(Duration, f64)>>,
            log: RefCell<Vec<&'static str>>,
            settable: bool,
            read_cost: Duration,
            read_blocked: bool,
            set: Effect,
            reveal: Effect,
            page: Effect,
            units: Option<f64>,
        }

        impl<'c> FakeArea<'c> {
            fn new(clock: &'c FakeClock, bar: f64) -> Self {
                Self {
                    clock,
                    start: clock.now(),
                    timeline: RefCell::new(vec![(Duration::ZERO, bar)]),
                    log: RefCell::new(Vec::new()),
                    settable: true,
                    read_cost: Duration::from_millis(2),
                    read_blocked: false,
                    set: NOT_SENT,
                    reveal: NOT_SENT,
                    page: NOT_SENT,
                    units: Some(0.01),
                }
            }
            fn at(&self, after: Duration, bar: f64) {
                self.timeline.borrow_mut().push((after, bar));
            }
            fn bar(&self) -> f64 {
                let now = self.clock.now() - self.start;
                self.timeline
                    .borrow()
                    .iter()
                    .filter(|(at, _)| *at <= now)
                    .max_by_key(|(at, _)| *at)
                    .map(|(_, bar)| *bar)
                    .unwrap()
            }
            fn mutate(&self, name: &'static str, effect: Effect, budget: &Budget) -> Mutation {
                if budget.expired() {
                    return Mutation::NotSent;
                }
                self.log.borrow_mut().push(name);
                self.clock.advance(effect.cost);
                if let Some((delay, bar)) = effect.bar {
                    let now = self.clock.now() - self.start;
                    self.at(now + delay, bar);
                }
                effect.answer
            }
            fn mutations(&self) -> Vec<&'static str> {
                self.log.borrow().clone()
            }
        }

        impl WheelArea for FakeArea<'_> {
            fn position(&self, _: &str, budget: &Budget) -> Option<f64> {
                if budget.expired() || self.read_blocked {
                    return None;
                }
                self.clock.advance(self.read_cost);
                Some(self.bar())
            }
            fn bar_settable(&self, _: &str, _: &Budget) -> bool {
                self.settable
            }
            fn set_position(&self, _: &str, _: f64, budget: &Budget) -> Mutation {
                self.mutate("set", self.set, budget)
            }
            fn reveal(&self, _: Option<[f64; 4]>, _: &str, _: f64, budget: &Budget) -> Mutation {
                self.mutate("reveal", self.reveal, budget)
            }
            fn page(&self, _: &str, _: usize, budget: &Budget) -> Mutation {
                self.mutate("page", self.page, budget)
            }
            fn extent(&self, _: &str, _: &Budget) -> Option<f64> {
                Some(500.0)
            }
            fn bar_units(&self, _: &str, _: f64, _: &Budget) -> Option<f64> {
                self.units
            }
        }

        fn verdict(area: &FakeArea, clock: &FakeClock) -> WheelVerdict {
            wheel_verdict(area, "up", 0.5, 40.0, 1, None, clock)
        }

        #[test]
        fn fallback_order_puts_a_settable_bar_first() {
            assert_eq!(
                fallback_order(true),
                &[Fallback::ScrollBar, Fallback::Reveal, Fallback::Page]
            );
            assert_eq!(
                fallback_order(false),
                &[Fallback::Reveal, Fallback::ScrollBar, Fallback::Page]
            );
        }

        #[test]
        fn a_settable_bar_is_set_before_any_reveal() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.set = sent(0.49);
            area.reveal = sent(0.3);
            let got = verdict(&area, &clock);
            assert!(
                matches!(
                    got,
                    WheelVerdict::MovedAfterFallback { ref attempted, .. } if attempted == "scroll bar value"
                ),
                "{got:?}"
            );
            assert_eq!(area.mutations(), vec!["set"]);
        }

        #[test]
        fn an_unsettable_bar_tries_reveal_then_page() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.settable = false;
            area.page = sent(0.49);
            let got = verdict(&area, &clock);
            assert!(
                matches!(
                    got,
                    WheelVerdict::MovedAfterFallback { ref attempted, .. } if attempted == "page action"
                ),
                "{got:?}"
            );
            assert_eq!(area.mutations(), vec!["reveal", "set", "page"]);
        }

        #[test]
        fn a_late_wheel_within_the_poll_is_moved_not_a_fallback() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.at(Duration::from_millis(60), 0.49);
            area.set = sent(0.4);
            let got = verdict(&area, &clock);
            assert_eq!(
                got,
                WheelVerdict::Moved {
                    before: 0.5,
                    after: 0.49
                }
            );
            assert!(area.mutations().is_empty());
        }

        #[test]
        fn a_blocked_bar_read_returns_unknown_without_fallback() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.read_blocked = true;
            area.set = sent(0.49);
            assert_eq!(verdict(&area, &clock), WheelVerdict::Unknown);
            assert!(area.mutations().is_empty());
        }

        #[test]
        fn the_budget_stops_routes_without_a_second_scroll() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            // One read of a busy app eats the budget down to under a route.
            area.read_cost = Duration::from_millis(1100);
            area.set = sent(0.49);
            assert_eq!(verdict(&area, &clock), WheelVerdict::Unknown);
            assert!(area.mutations().is_empty());
        }

        #[test]
        fn a_timed_out_mutation_stops_further_fallbacks() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            // The write times out after 300 ms with budget left, and the app
            // applies it 200 ms later: past the one read, so unverified.
            area.set = Effect {
                answer: Mutation::Failed { count: 1 },
                cost: Duration::from_millis(300),
                bar: Some((Duration::from_millis(200), 0.49)),
            };
            area.reveal = sent(0.3);
            area.page = sent(0.3);
            let got = verdict(&area, &clock);
            assert!(
                matches!(&got, WheelVerdict::Unverified { attempted } if attempted.contains("scroll bar value")),
                "{got:?}"
            );
            assert_eq!(area.mutations(), vec!["set"]);

            // Moved within the settle: reported without crediting the failed write;
            // still nothing else sent.
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.set = Effect {
                answer: Mutation::Failed { count: 1 },
                cost: Duration::from_millis(300),
                bar: Some((Duration::from_millis(20), 0.49)),
            };
            area.reveal = sent(0.3);
            let got = verdict(&area, &clock);
            assert!(
                matches!(&got, WheelVerdict::MovedAfterFallback { attempted, .. } if attempted.contains("scroll bar value")),
                "the move is reported without crediting the failed write: {got:?}"
            );
            assert_eq!(area.mutations(), vec!["set"]);
        }

        #[test]
        fn a_failed_reveal_stops_the_ladder() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.settable = false;
            area.reveal = Effect {
                answer: Mutation::Failed { count: 1 },
                cost: Duration::from_millis(5),
                bar: None,
            };
            area.page = sent(0.49);
            let got = verdict(&area, &clock);
            assert!(
                matches!(&got, WheelVerdict::Unverified { attempted } if attempted.contains("AXScrollToVisible")),
                "{got:?}"
            );
            assert_eq!(area.mutations(), vec!["reveal"]);
        }

        #[test]
        fn a_failed_page_action_stops_the_ladder() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.settable = false;
            area.page = Effect {
                answer: Mutation::Failed { count: 1 },
                cost: Duration::from_millis(5),
                bar: None,
            };
            let got = verdict(&area, &clock);
            assert!(
                matches!(&got, WheelVerdict::Unverified { attempted } if attempted.starts_with("page action")),
                "{got:?}"
            );
            assert_eq!(area.mutations(), vec!["reveal", "set", "page"]);
        }

        #[test]
        fn running_out_after_a_sent_fallback_is_unverified_not_a_noop() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            // The write is accepted and reads back unmoved, leaving under a
            // route's worth of budget: it may still apply.
            area.set = Effect {
                answer: Mutation::Sent { count: 1 },
                cost: Duration::from_millis(800),
                bar: None,
            };
            area.reveal = sent(0.3);
            let got = verdict(&area, &clock);
            assert!(
                matches!(&got, WheelVerdict::Unverified { attempted } if attempted == "scroll bar value"),
                "{got:?}"
            );
            assert_eq!(area.mutations(), vec!["set"]);
        }

        #[test]
        fn running_out_inside_the_last_route_after_a_sent_fallback_is_unverified() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            // The bar write is accepted and reads back unmoved; the reveal has
            // nothing to send; the page route spends the rest and sends nothing.
            area.set = Effect {
                answer: Mutation::Sent { count: 1 },
                cost: Duration::from_millis(5),
                bar: None,
            };
            area.page = Effect {
                answer: Mutation::NotSent,
                cost: Duration::from_millis(2000),
                bar: None,
            };
            let got = verdict(&area, &clock);
            assert!(
                matches!(&got, WheelVerdict::Unverified { attempted } if attempted == "scroll bar value"),
                "{got:?}"
            );
            assert_eq!(area.mutations(), vec!["set", "reveal", "page"]);
        }

        #[test]
        fn a_late_wheel_after_a_successful_fallback_is_not_credited_to_it() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            // The write is accepted but changes nothing; the wheel lands
            // 300 ms after it was posted, past the poll.
            area.set = Effect {
                answer: Mutation::Sent { count: 1 },
                cost: Duration::from_millis(5),
                bar: None,
            };
            area.at(Duration::from_millis(300), 0.49);
            let got = verdict(&area, &clock);
            assert!(
                matches!(&got, WheelVerdict::MovedAfterFallback { attempted, .. } if attempted == "scroll bar value"),
                "{got:?}"
            );
            let result = got
                .result("up", "line", 1, (0.0, 0.0), "")
                .expect("a result");
            let structured = result.structured_content.expect("structured");
            assert_eq!(structured["cause_known"], false);
            let text = serde_json::to_string(&result.content).expect("content");
            assert!(
                text.contains("whether the wheel, that attempt or the app moved"),
                "{text}"
            );
        }

        #[test]
        fn movement_the_other_way_is_not_this_scroll() {
            // Before any fallback: the app moves the bar down while the wheel
            // scrolls up. That is neither the wheel landing nor a reason to
            // scroll again: the wheel result stays unverified, nothing more sent.
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.at(Duration::from_millis(30), 0.6);
            area.set = sent(0.45);
            assert_eq!(verdict(&area, &clock), WheelVerdict::Unknown);
            assert!(area.mutations().is_empty());

            // After a fallback: the bar ends up further down than it started.
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.set = sent(0.6);
            area.reveal = sent(0.3);
            let got = verdict(&area, &clock);
            assert!(
                matches!(&got, WheelVerdict::Unverified { attempted } if attempted == "scroll bar value"),
                "{got:?}"
            );
            assert_eq!(
                area.mutations(),
                vec!["set"],
                "no second scroll after unexplained movement"
            );
        }

        #[test]
        fn a_reversal_after_the_fallback_is_unverified() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.set = sent(0.45);
            area.units = Some(0.05);
            // The extra read finds the bar pushed back past the start.
            area.at(Duration::from_millis(400), 0.7);
            let got = verdict(&area, &clock);
            assert!(
                matches!(&got, WheelVerdict::Unverified { attempted } if attempted.contains("then back (0.700)")),
                "a reversal after the fallback is not confirmed progress: {got:?}"
            );
        }

        #[test]
        fn movement_the_other_way_at_the_end_is_not_a_noop() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 1.0);
            // Scrolling down from the end while the app moves the bar up.
            area.at(Duration::from_millis(40), 0.8);
            area.set = sent(0.9);
            let got = wheel_verdict(&area, "down", 1.0, 40.0, 1, None, &clock);
            assert_eq!(got, WheelVerdict::Unknown);
            assert!(area.mutations().is_empty());
        }

        #[test]
        fn expiry_after_a_successful_bar_write_reports_the_write_unverified() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.set = Effect {
                answer: Mutation::Sent { count: 1 },
                cost: Duration::from_millis(1000),
                bar: Some((Duration::from_millis(500), 0.49)),
            };
            let got = verdict(&area, &clock);
            assert!(
                matches!(&got, WheelVerdict::Unverified { attempted } if attempted == "scroll bar value"),
                "{got:?}"
            );
            assert_eq!(area.mutations(), vec!["set"]);
        }

        #[test]
        fn expiry_between_page_actions_reports_pages_sent() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.settable = false;
            area.page = Effect {
                answer: Mutation::Sent { count: 2 },
                cost: Duration::from_millis(1000),
                bar: Some((Duration::from_millis(500), 0.3)),
            };
            let got = verdict(&area, &clock);
            assert!(
                matches!(&got, WheelVerdict::Unverified { attempted } if attempted == "page action (2 pages)"),
                "{got:?}"
            );
        }

        #[test]
        fn overshoot_is_reported_without_a_cause() {
            let distance = |bar: f64, units: Option<f64>| {
                let clock = FakeClock::new();
                let mut area = FakeArea::new(&clock, 0.5);
                area.set = sent(bar);
                area.units = units;
                match verdict(&area, &clock) {
                    WheelVerdict::MovedAfterFallback { distance, .. } => distance,
                    other => panic!("{other:?}"),
                }
            };
            // Requested 0.05 of the bar: 0.1 is 2x, 0.06 is 1.2x.
            assert_eq!(
                distance(0.4, Some(0.05)),
                DistanceCheck::Overshoot {
                    later: 0.4,
                    requested: 0.05
                }
            );
            assert_eq!(distance(0.44, Some(0.05)), DistanceCheck::AsRequested);
            // Clamped at the top: not an overshoot.
            assert_eq!(distance(0.0, Some(0.05)), DistanceCheck::AsRequested);
            assert_eq!(distance(0.4, None), DistanceCheck::NotMeasured);
        }

        #[test]
        fn movement_before_the_fallback_is_a_move_not_an_overshoot() {
            let clock = FakeClock::new();
            let mut area = FakeArea::new(&clock, 0.5);
            area.at(Duration::from_millis(10), 0.2);
            area.set = sent(0.1);
            assert_eq!(
                verdict(&area, &clock),
                WheelVerdict::Moved {
                    before: 0.5,
                    after: 0.2
                }
            );
            assert!(area.mutations().is_empty());
        }
    }

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
