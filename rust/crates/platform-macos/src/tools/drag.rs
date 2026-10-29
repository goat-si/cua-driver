//! drag tool — matches the Swift reference DragTool.swift.
//!
//! Press-drag-release gesture: mouseDown at (from_x, from_y), N interpolated
//! mouseDragged events along the path, mouseUp at (to_x, to_y).
//!
//! All coordinates are in window-local screenshot pixels (same space as
//! `get_window_state` returns). `from_zoom=true` translates from the most
//! recent zoom crop context, same as click/double_click.

use crate::goat::action_latency as latency;

use async_trait::async_trait;
use core_foundation::{
    base::{CFEqual, CFGetTypeID, CFRelease, CFTypeRef, TCFType},
    string::CFString,
};
use cua_driver_contract::{ClickButton, DragInput};
use cua_driver_core::{
    action_record::{
        ActionEffect, ActionEvidence, ActionExecutionRecord, ActionTransport, ActualDelivery,
        EvidenceKind, RequestedDelivery,
    },
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use serde_json::Value;
use std::sync::Arc;

use super::ToolState;
use crate::apps;
use crate::ax::bindings::{
    copy_string_attr, kAXErrorAttributeUnsupported, kAXErrorNoValue, kAXErrorSuccess,
    kAXValueCFRangeType, AXUIElementCopyAttributeValue, AXUIElementRef, AXValueGetTypeID,
    AXValueGetValue,
};
use crate::focus_guard;
use crate::input::mouse::DragButton;
use crate::window_change_detector::WindowChangeDetector;

pub struct DragTool {
    pub state: Arc<ToolState>,
}

impl DragTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

const DELIVERY_MODE_DESCRIPTION: &str = "Window-scoped drag on macOS: \"background\" (default) \
     posts the press, drag and release to the exact window (window_id required) without moving \
     the pointer, raising the window or taking the user's focus. \"foreground\" briefly fronts \
     the window, drives the physical pointer through the gesture, then restores the prior \
     frontmost app; use it last, when a background drag had no effect and no element_token \
     control or keyboard route does the same. Desktop scope ignores this field.";

/// What to try after a background drag that was refused or changed nothing:
/// a non-drag route by element_token, then the keyboard, and the foreground
/// last, because it moves the user's pointer and brings the window in front
/// of them (cua-driver#22).
const DRAG_LADDER: &str = "Look for a route that is not a drag: a Move control, menu item or \
     context-menu command by element_token, or the keyboard (a shortcut, cut and paste). Retry \
     with delivery_mode:\"foreground\" last, only if the user allows it: it moves the user's \
     pointer and brings the window to the front.";

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "drag".into(),
        description:
            "Press-drag-release from (from_x, from_y) to (to_x, to_y) in window-local screenshot pixels \
                (top-left origin, the get_window_state space); with `from_zoom:true`, in the last zoom image for \
                this pid. `duration_ms` (default 500) and `steps` (default 20 interpolated moves) set the speed; \
                `modifier` keys (cmd/shift/option/ctrl) are held throughout.\n\
                \n\
                A window-scoped drag (pid + window_id) runs in the background by default: it is posted to that window, the \
                real pointer does not move and the user's front window keeps focus. It works for controls that \
                follow drag events (sliders, canvases, pointer-event widgets on Chrome pages); in an Electron \
                window it is sent from the app's main process, so pointer-capture widgets and HTML5 drag and \
                drop work too. Elsewhere a drag that starts a macOS drag-and-drop session (list reordering, \
                moving a file, HTML5 draggable content) cannot drop in the background: the driver releases at \
                once and returns background_unavailable with reason drag_and_drop_session. For a browser tab \
                bound with get_browser_state, use browser_pointer action:\"drag\". The result is not \
                driver-verified. If nothing changed, use a route that is not a drag first: a Move control or \
                menu item by element_token, or the keyboard (a shortcut, cut and paste). Retry with \
                delivery_mode:\"foreground\" last, only if the user allows it: it briefly fronts the window and \
                moves the user's physical pointer."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["from_x", "from_y", "to_x", "to_y"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid": { "type": "integer", "description": "Target process ID." },
                "window_id": {
                    "type": "integer",
                    "description": "CGWindowID for the window the pixel coordinates were measured against. Optional only when pid owns exactly one eligible top-level window; otherwise the action refuses with ambiguous_window_target."
                },
                "from_x": { "type": "number", "description": "Drag-start X in window-local screenshot pixels. Top-left origin." },
                "from_y": { "type": "number", "description": "Drag-start Y in window-local screenshot pixels. Top-left origin." },
                "to_x": { "type": "number", "description": "Drag-end X in window-local screenshot pixels." },
                "to_y": { "type": "number", "description": "Drag-end Y in window-local screenshot pixels." },
                "duration_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 10000,
                    "description": "Wall-clock duration of the drag path between mouseDown and mouseUp. Default: 500."
                },
                "steps": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "description": "Number of intermediate mouseDragged events linearly interpolated along the path. Default: 20."
                },
                "modifier": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Modifier keys held across the entire gesture: cmd/shift/option/ctrl."
                },
                "button": {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "description": "Mouse button used for the drag. Default: left."
                },
                "from_zoom": {
                    "type": "boolean",
                    "description": "When true, coordinates are in the last zoom image for this pid; driver maps back to window coordinates."
                },
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "Use desktop with no pid/window_id for native get_desktop_state screenshot coordinates." },
                "coordinate_space": super::window_points::coordinate_space_schema(),
                "window_basis": super::window_points::window_basis_schema(),
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema_with(DELIVERY_MODE_DESCRIPTION)
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
impl Tool for DragTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let space = match super::window_points::point_space(&args) {
            Ok(space) => space,
            Err(refusal) => return refusal,
        };
        let point = args
            .get("to_x")
            .and_then(Value::as_f64)
            .zip(args.get("to_y").and_then(Value::as_f64));
        super::window_points::annotate(self.invoke_in(args, space).await, space, point)
    }
}

impl DragTool {
    async fn invoke_in(&self, args: Value, space: super::window_points::PointSpace) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<DragInput>("drag", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let (from_x, from_y, to_x, to_y) = (input.from_x, input.from_y, input.to_x, input.to_y);
            let (from_x, from_y) = super::desktop_screenshot_point(from_x, from_y);
            let (to_x, to_y) = super::desktop_screenshot_point(to_x, to_y);
            if let Some(refusal) = super::desktop_pointer_guard::check(
                "drag",
                &args,
                vec![(from_x, from_y), (to_x, to_y)],
            )
            .await
            {
                return refusal;
            }
            let duration_ms = input.duration_ms.unwrap_or(500).min(10_000);
            let steps = input.steps.unwrap_or(20).clamp(1, 200) as usize;
            let modifiers = input.modifier.unwrap_or_default();
            let button = match input.button.unwrap_or(ClickButton::Left) {
                ClickButton::Left => DragButton::Left,
                ClickButton::Right => DragButton::Right,
                ClickButton::Middle => DragButton::Middle,
            };
            let cursor_for_drag = cursor_key.clone();
            crate::cursor::overlay::send_command(
                cursor_key.clone(),
                cursor_overlay::OverlayCommand::SetPressed(true),
            );
            let result = latency::spawn_action_blocking(move || {
                let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                crate::input::mouse::drag_at_xy_foreground_observed(
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    duration_ms,
                    steps,
                    &modifier_refs,
                    button,
                    move |x, y| {
                        crate::cursor::overlay::send_command(
                            cursor_for_drag.clone(),
                            cursor_overlay::track_pointer_command(x, y),
                        );
                    },
                )
            })
            .await;
            crate::cursor::overlay::send_command(
                cursor_key.clone(),
                cursor_overlay::OverlayCommand::SetPressed(false),
            );
            if matches!(&result, Ok(Ok(()))) {
                self.state
                    .cursor_registry
                    .update_position(&cursor_key, to_x, to_y);
            }
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Dragged desktop from ({from_x:.1}, {from_y:.1}) to ({to_x:.1}, {to_y:.1})."
                ))
                .with_structured(serde_json::json!({
                    "scope": "desktop",
                    "path": "hid",
                    "effect": "unverifiable"
                })),
                Ok(Err(error)) => ToolResult::error(format!("desktop drag failed: {error}")),
                Err(error) => ToolResult::error(format!("desktop drag task failed: {error}")),
            };
        }
        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Background (default): post the gesture to the exact window without
        // moving the pointer or raising it, after the focus-without-raise
        // prologue background clicks use. Foreground: briefly front the
        // window and drive the physical pointer (the explicit last resort for
        // surfaces that drop background events). Both need the exact window.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        let fg = delivery_mode.is_foreground();
        // Coerce integer or float from JSON for coordinate fields.
        let coerce = |key: &str| -> Option<f64> {
            args.opt_f64(key)
                .or_else(|| args.opt_i64(key).map(|i| i as f64))
        };

        let mut from_x = match coerce("from_x") {
            Some(v) => v,
            None => return super::invalid_arguments("Missing required parameter: from_x"),
        };
        let mut from_y = match coerce("from_y") {
            Some(v) => v,
            None => return super::invalid_arguments("Missing required parameter: from_y"),
        };
        let mut to_x = match coerce("to_x") {
            Some(v) => v,
            None => return super::invalid_arguments("Missing required parameter: to_x"),
        };
        let mut to_y = match coerce("to_y") {
            Some(v) => v,
            None => return super::invalid_arguments("Missing required parameter: to_y"),
        };

        let Some(wid) = args.opt_u64("window_id").map(|v| v as u32) else {
            return window_id_required(fg);
        };
        let window_id = Some(wid);
        let duration_ms = args.u64_or("duration_ms", 500).min(10_000);
        let steps = (args.u64_or("steps", 20) as usize).clamp(1, 200);
        let from_zoom = args.bool_or("from_zoom", false);
        let button_str = args.str_or("button", "left");
        let modifiers: Vec<String> = args.str_array("modifier");

        let button = match button_str.to_lowercase().as_str() {
            "left" => DragButton::Left,
            "right" => DragButton::Right,
            "middle" => DragButton::Middle,
            other => {
                return super::invalid_arguments(format!(
                    "Unknown button \"{other}\" — expected left, right, or middle."
                ))
            }
        };

        if from_zoom && matches!(space, super::window_points::PointSpace::WindowPoints(_)) {
            return super::invalid_arguments(
                "coordinate_space window_points cannot be combined with from_zoom.",
            );
        }
        let mut validated_frame: Option<super::px_frame::WindowPxFrame> = None;
        // from_zoom: translate from last zoom crop context.
        if from_zoom {
            match super::zoom_context(&self.state, &args, pid, window_id) {
                Ok(ctx) => {
                    let (wx, wy) = ctx.zoom_to_window(from_x, from_y);
                    let (wx2, wy2) = ctx.zoom_to_window(to_x, to_y);
                    from_x = wx;
                    from_y = wy;
                    to_x = wx2;
                    to_y = wy2;
                }
                Err(refusal) => return refusal,
            }
        } else {
            // Both ends against one frame read: a stale basis refuses the drag.
            match super::window_points::to_window_pixels(
                &self.state,
                &args,
                pid,
                window_id,
                space,
                &[(from_x, from_y), (to_x, to_y)],
            )
            .await
            {
                Ok(resolved) => {
                    (from_x, from_y) = resolved.pixels[0];
                    (to_x, to_y) = resolved.pixels[1];
                    validated_frame = resolved.frame;
                }
                Err(refusal) => return refusal,
            }
        }

        // Translate window-local screenshot pixels → screen coordinates, and
        // window-local logical coords for the foreground HID stamp. Both ends
        // of the drag resolve against ONE frame, so a window that closed
        // mid-call refuses rather than dragging across the desktop behind it.
        let (from_sx, from_sy, from_lx, from_ly, to_sx, to_sy, to_lx, to_ly) =
            match super::window_points::frame_or_resolve(validated_frame.clone(), wid).await {
                Ok(frame) => {
                    // Keep the actual translation frame for EVERY route:
                    // the drag's window-local stamps use its origin too.
                    validated_frame = Some(frame.clone());
                    let (fsx, fsy, flx, fly) = frame.to_screen(from_x, from_y);
                    let (tsx, tsy, tlx, tly) = frame.to_screen(to_x, to_y);
                    (fsx, fsy, flx, fly, tsx, tsy, tlx, tly)
                }
                Err(refusal) => return refusal,
            };

        // A toolkit that reads the hardware pointer (Tk) or drops routed
        // mouse events (LibreOffice VCL) cannot take a background gesture;
        // refuse before any activation or dispatch.
        if !fg {
            if let Err(refusal) =
                super::pixel_route::resolve(pid, false, window_id, "mouse_drag").await
            {
                return refusal;
            }
            // Chromium reads a mouse event's held buttons from the hardware
            // state, which a window-routed press does not change, so an
            // Electron page sees the drag moves with buttons == 0, releases
            // its pointer capture and loses the drop. Send the gesture from
            // the app's main process instead, where the held button is part
            // of the event; refuse when that route is unavailable.
            if crate::browser::ElectronJs::is_electron(pid) {
                if let Err(refusal) =
                    super::window_points::recheck_now(space, window_id, validated_frame.as_ref())
                {
                    return refusal;
                }
                return self
                    .electron_background_drag(
                        &cursor_key,
                        pid,
                        wid,
                        (from_lx, from_ly),
                        (to_lx, to_ly),
                        (from_sx, from_sy),
                        (to_sx, to_sy),
                        duration_ms,
                        steps,
                        &modifiers,
                        button,
                        (space, validated_frame.clone()),
                    )
                    .await;
            }
        }

        // Animate agent cursor along drag path (start → end).
        crate::cursor::overlay::send_command(
            cursor_key.clone(),
            cursor_overlay::OverlayCommand::PinAbove(wid as u64),
        );
        crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), from_sx, from_sy).await;

        if let Err(refusal) =
            super::window_points::recheck_now(space, window_id, validated_frame.as_ref())
        {
            return refusal;
        }

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // Drags can trigger drag-and-drop side-effects that spawn helper
        // windows (drop on Dock, drop on background app icon). In the
        // background the target itself may become AppKit-active without
        // raising; anything else that takes focus is reported and undone.
        let prior_front = apps::frontmost_pid();
        let snapshot = if fg {
            WindowChangeDetector::snapshot(prior_front)
        } else {
            WindowChangeDetector::snapshot_allowing_activation(prior_front, pid)
        };

        // Background prologue, as for a background pixel click: keep the
        // user's key focus if the gesture opens a menu, remember their key
        // window, and make the target window AppKit-key without raising it.
        let _key_focus = if fg {
            None
        } else {
            latency::spawn_blocking(move || crate::key_focus_guard::protect(pid))
                .await
                .ok()
                .flatten()
        };
        let prior_key_wid = match prior_front {
            Some(prior) if !fg && prior != pid => latency::spawn_action_blocking(move || {
                crate::input::skylight::key_window_of_pid(prior)
            })
            .await
            .ok()
            .flatten(),
            _ => None,
        };
        let focus_without_raise = if fg {
            false
        } else {
            match latency::spawn_action_blocking(move || {
                // A key hand-back still running would move key focus under
                // this action's focus record: send nothing instead.
                crate::input::skylight::await_key_restores()
                    .then(|| crate::input::mouse::prepare_background_pixel_click(pid, wid))
            })
            .await
            {
                Ok(None) => {
                    return super::key_restore_pending_refusal("drag", Some(pid), Some(wid));
                }
                Ok(Some(activated)) => {
                    if !activated && crate::input::skylight::windowserver_connection_invalid() {
                        return super::windowserver_connection_refusal(pid, Some(wid));
                    }
                    crate::cursor::overlay::send_command(
                        cursor_key.clone(),
                        cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                    );
                    activated
                }
                Err(error) => {
                    return ToolResult::error(format!(
                        "Background drag activation task failed: {error}"
                    ))
                }
            }
        };

        // Dispatch blocking drag synthesis.
        let mods_owned = modifiers.clone();
        let cursor_for_drag = cursor_key.clone();
        crate::cursor::overlay::send_command(
            cursor_key.clone(),
            cursor_overlay::OverlayCommand::SetPressed(true),
        );
        let dispatch_guard = match super::window_points::DispatchGuard::new(
            space,
            window_id,
            validated_frame.clone(),
        ) {
            Ok(guard) => guard,
            Err(refusal) => return refusal,
        };
        let result = focus_guard::with_focus_suppressed(
            // Neither mode suppresses the target: foreground deliberately
            // activates it for the HID gesture, and background lets the
            // no-raise prologue make it active; the restore below hands focus
            // back. Other apps that take focus are still suppressed.
            None,
            prior_front,
            "drag.CGEvent",
            || async move {
                latency::spawn_action_blocking(move || -> anyhow::Result<DragDispatchOutcome> {
                    let m: Vec<&str> = mods_owned.iter().map(String::as_str).collect();
                    let observe = move |x: f64, y: f64| {
                        crate::cursor::overlay::send_command(
                            cursor_for_drag.clone(),
                            cursor_overlay::track_pointer_command(x, y),
                        );
                    };
                    if fg {
                        // Keep the exact target window active for the
                        // complete HID gesture, then restore the prior
                        // front process.
                        let mut selection = None;
                        return crate::input::skylight::with_foreground_hid_activation(
                            pid as libc::pid_t,
                            wid,
                            || {
                                // After the activation and its focus wait,
                                // right before the press.
                                super::window_points::check_guard(&dispatch_guard)?;
                                let before = focused_selection(pid, wid);
                                super::window_points::check_guard(&dispatch_guard)?;
                                crate::input::mouse::drag_at_xy_observed(
                                    pid,
                                    from_sx,
                                    from_sy,
                                    to_sx,
                                    to_sy,
                                    Some((from_lx, from_ly)),
                                    Some((to_lx, to_ly)),
                                    Some(wid),
                                    duration_ms,
                                    steps,
                                    &m,
                                    button,
                                    true,
                                    observe,
                                )?;
                                let after = focused_selection(pid, wid);
                                selection = selection_confirmation(before.as_ref(), after.as_ref());
                                // Hold the front until AppKit finishes the
                                // drop: restoring the previous app while the
                                // session is still open cancels it.
                                await_drag_session_end(pid);
                                Ok(())
                            },
                        )
                        .map(|()| DragDispatchOutcome {
                            native_drag_session: false,
                            selection,
                        });
                    }
                    // A drag that starts a system drag-and-drop session
                    // (list reordering, file drags, HTML5 draggable) is
                    // tracked by macOS with the hardware pointer, so the
                    // drop cannot land at the requested point. AppKit shows
                    // the drag image in a window of the target at the
                    // dragging level; when one appears, release at once so
                    // the session ends where the user's pointer is not
                    // moving, and report it.
                    let before = drag_image_windows(pid);
                    // After the window read, right before the press.
                    super::window_points::check_guard(&dispatch_guard)?;
                    let before_selection = focused_selection(pid, wid);
                    super::window_points::check_guard(&dispatch_guard)?;
                    let mut session_seen = false;
                    let origin = validated_frame
                        .as_ref()
                        .map(|frame| (frame.bounds.x, frame.bounds.y));
                    let outcome = match origin {
                        Some(origin) => crate::input::mouse::drag_at_xy_background_with_origin(
                            pid,
                            (from_sx, from_sy),
                            (to_sx, to_sy),
                            wid,
                            duration_ms,
                            steps,
                            &m,
                            button,
                            observe,
                            || {
                                session_seen = !drag_image_windows(pid).is_subset(&before);
                                !session_seen
                            },
                            origin,
                        ),
                        None => crate::input::mouse::drag_at_xy_background(
                            pid,
                            (from_sx, from_sy),
                            (to_sx, to_sy),
                            wid,
                            duration_ms,
                            steps,
                            &m,
                            button,
                            observe,
                            || {
                                session_seen = !drag_image_windows(pid).is_subset(&before);
                                !session_seen
                            },
                        ),
                    }?;

                    let session = session_seen
                        || !drag_image_windows(pid).is_subset(&before)
                        || matches!(
                            outcome,
                            crate::input::mouse::BackgroundDragOutcome::StoppedEarly { .. }
                        );
                    let after_selection = focused_selection(pid, wid);
                    let selection =
                        selection_confirmation(before_selection.as_ref(), after_selection.as_ref());
                    Ok(DragDispatchOutcome {
                        native_drag_session: session,
                        selection,
                    })
                })
                .await
            },
        )
        .await;
        crate::cursor::overlay::send_command(
            cursor_key.clone(),
            cursor_overlay::OverlayCommand::SetPressed(false),
        );
        if matches!(
            &result,
            Ok(Ok(DragDispatchOutcome {
                native_drag_session: false,
                ..
            }))
        ) {
            self.state
                .cursor_registry
                .update_position(&cursor_key, to_sx, to_sy);
        }

        if !fg {
            super::click::restore_user_focus_after_background_pixel(
                pid,
                prior_front,
                prior_key_wid,
                window_id,
                focus_without_raise,
            )
            .await;
        }

        let changes = super::finish_window_observation(snapshot).await;
        if let Ok(Err(error)) = &result {
            if let Some(refusal) = super::window_points::moved_refusal(error, space) {
                return refusal;
            }
        }

        crate::cursor::overlay::send_command(
            cursor_key.clone(),
            cursor_overlay::OverlayCommand::PinAbove(wid as u64),
        );

        let mod_suffix = if modifiers.is_empty() {
            String::new()
        } else {
            format!(" with {}", modifiers.join("+"))
        };
        let btn_suffix = if button_str == "left" {
            String::new()
        } else {
            format!(" ({button_str} button)")
        };
        let (route_note, path) = if fg {
            (
                "delivery_mode:foreground; window fronted, hardware pointer moved, HID event tap",
                "cgevent_hid",
            )
        } else {
            (
                "background: routed to the window's pid, pointer not moved, window not raised",
                "cgevent",
            )
        };
        match result {
            Ok(Ok(DragDispatchOutcome {
                native_drag_session: true,
                ..
            })) if cua_driver_core::delivery::brief_foreground_allowed()
                && !cua_driver_core::delivery::background_requested_explicitly(&args) =>
            {
                self.brief_foreground_drag(&args, pid, wid, prior_front)
                    .await
            }
            Ok(Ok(DragDispatchOutcome {
                native_drag_session: true,
                ..
            })) => drag_and_drop_session_result(pid, wid, changes.result_suffix()),
            Ok(Ok(DragDispatchOutcome {
                native_drag_session: false,
                selection,
            })) => {
                let verification_note = if selection.is_some() {
                    "text selection confirmed by accessibility readback"
                } else {
                    "not driver-verified — confirm via screenshot"
                };
                drag_success_result(
                    format!(
                        "✅ Posted drag{btn_suffix}{mod_suffix} to pid {pid} window {wid} \
                         from window-pixel ({}, {}) → ({}, {}), \
                         screen ({}, {}) → ({}, {}) \
                         in {duration_ms}ms / {steps} steps ({route_note}; {verification_note}).{}",
                        from_x as i64,
                        from_y as i64,
                        to_x as i64,
                        to_y as i64,
                        from_sx as i64,
                        from_sy as i64,
                        to_sx as i64,
                        to_sy as i64,
                        changes.result_suffix(),
                    ),
                    path,
                    focus_without_raise,
                    fg,
                    selection,
                )
            }
            Ok(Err(e)) => ToolResult::error(format!("drag failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

impl DragTool {
    /// The operator allowed a brief foreground step (trycua/cua#5478) and the
    /// background drag started a native drag-and-drop session, which macOS
    /// tracks with the real pointer. Repeat only this drag in the foreground,
    /// which fronts the window, drives the pointer, then hands the frontmost
    /// app and the pointer back. Check both and report the step in the result.
    async fn brief_foreground_drag(
        &self,
        args: &Value,
        pid: i32,
        wid: u32,
        prior_front: Option<i32>,
    ) -> ToolResult {
        let pointer_before = latency::spawn_action_blocking(pointer_location)
            .await
            .ok()
            .flatten();
        tracing::info!(
            pid,
            window_id = wid,
            ?prior_front,
            "drag: background drop impossible (drag-and-drop session); \
             operator allowed a brief foreground step"
        );
        let mut foreground_args = args.clone();
        foreground_args["delivery_mode"] = serde_json::json!("foreground");
        let mut result = self.invoke(foreground_args).await;

        // The foreground drag leaves the pointer on the drop point (after the
        // drop has finished, see `await_drag_session_end`); this step promised
        // to hand it back, so put it where the user left it.
        if let Some((x, y)) = pointer_before {
            let _ = latency::spawn_action_blocking(move || {
                crate::input::mouse::move_cursor_desktop(x, y)
            })
            .await;
        }
        let pointer_after = latency::spawn_action_blocking(pointer_location)
            .await
            .ok()
            .flatten();
        let front_after = apps::frontmost_pid();
        let restored_frontmost = prior_front.is_none() || front_after == prior_front;
        let restored_pointer = match (pointer_before, pointer_after) {
            (Some((bx, by)), Some((ax, ay))) => Some((bx - ax).hypot(by - ay) <= 1.0),
            _ => None,
        };
        tracing::info!(
            pid,
            window_id = wid,
            restored_frontmost,
            ?restored_pointer,
            failed = result.is_error == Some(true),
            "drag: brief foreground step finished"
        );

        let step = brief_foreground_step(restored_frontmost, restored_pointer);
        let note = brief_foreground_note(restored_frontmost, restored_pointer);
        let mut structured = result
            .structured_content
            .take()
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(fields) = structured.as_object_mut() {
            fields.insert("foreground_step".to_owned(), step);
        }
        result.structured_content = Some(structured);
        result
            .content
            .insert(0, cua_driver_core::protocol::Content::text(note));
        result
    }

    /// Background drag in an Electron window through its main process
    /// ([`crate::input::electron_drag`]). Refuses when the app's inspector
    /// is unavailable (for example the inspector fuse is off).
    #[allow(clippy::too_many_arguments)]
    async fn electron_background_drag(
        &self,
        cursor_key: &str,
        pid: i32,
        wid: u32,
        from_local: (f64, f64),
        to_local: (f64, f64),
        from_screen: (f64, f64),
        to_screen: (f64, f64),
        duration_ms: u64,
        steps: usize,
        modifiers: &[String],
        button: DragButton,
        (space, validated_frame): (
            super::window_points::PointSpace,
            Option<super::px_frame::WindowPxFrame>,
        ),
    ) -> ToolResult {
        use crate::input::electron_drag::{
            electron_drag_js, parse_electron_drag_reply, ElectronDragButton, ElectronDragReply,
            ElectronDragSpec,
        };
        let spec = ElectronDragSpec {
            window_id: wid,
            from: from_local,
            to: to_local,
            steps,
            duration_ms,
            button: match button {
                DragButton::Left => ElectronDragButton::Left,
                DragButton::Right => ElectronDragButton::Right,
                DragButton::Middle => ElectronDragButton::Middle,
            },
            modifiers: modifiers.to_vec(),
        };
        let cursor_key = cursor_key.to_owned();
        crate::cursor::overlay::send_command(
            cursor_key.clone(),
            cursor_overlay::OverlayCommand::PinAbove(wid as u64),
        );
        crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), from_screen.0, from_screen.1)
            .await;
        crate::cursor::overlay::send_command(
            cursor_key.clone(),
            cursor_overlay::OverlayCommand::SetPressed(true),
        );
        // The cursor animation above takes time: the window must still be
        // where the window points were translated.
        if let Err(refusal) =
            super::window_points::recheck_now(space, Some(wid), validated_frame.as_ref())
        {
            crate::cursor::overlay::send_command(
                cursor_key.to_owned(),
                cursor_overlay::OverlayCommand::SetPressed(false),
            );
            return refusal;
        }
        let reply = crate::browser::ElectronJs::execute_main(&electron_drag_js(&spec), pid).await;
        let outcome = match &reply {
            Ok(text) => parse_electron_drag_reply(text),
            Err(_) => None,
        };
        if matches!(outcome, Some(ElectronDragReply::Delivered { .. })) {
            crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), to_screen.0, to_screen.1)
                .await;
            self.state
                .cursor_registry
                .update_position(&cursor_key, to_screen.0, to_screen.1);
        }
        crate::cursor::overlay::send_command(
            cursor_key.clone(),
            cursor_overlay::OverlayCommand::SetPressed(false),
        );
        match outcome {
            Some(ElectronDragReply::Delivered {
                route,
                drag_intercepted,
                html5_drop,
            }) => {
                let dnd = if html5_drop {
                    " The gesture started an HTML5 drag and drop; the driver kept it out of the \
                     native drag session and dropped it at the end point."
                } else {
                    ""
                };
                ToolResult::text(format!(
                    "✅ Posted drag to Electron pid {pid} window {wid} from window-point \
                     ({:.0}, {:.0}) → ({:.0}, {:.0}) through the app's main process \
                     ({route}); pointer not moved, window not raised, focus kept.{dnd} \
                     Not driver-verified — confirm via screenshot.",
                    from_local.0, from_local.1, to_local.0, to_local.1,
                ))
                .with_structured(serde_json::json!({
                    // Chromium trusted input (CDP Input or sendInputEvent), the
                    // same public route as browser_pointer.
                    "path": "trusted",
                    "electron_route": route,
                    "drag_intercepted": drag_intercepted,
                    "html5_drop": html5_drop,
                    "delivery_mode": "background",
                    "verified": false,
                    "effect": "unverifiable",
                }))
            }
            other => {
                let reason = match (other, &reply) {
                    (Some(ElectronDragReply::Refused { reason }), _) => reason,
                    (_, Err(error)) => error.to_string(),
                    (_, Ok(text)) => format!("unexpected inspector reply: {text}"),
                };
                electron_drag_refusal(&reason)
            }
        }
    }
}

/// Wait (up to 2 s) until `pid` shows no drag image, so a drag-and-drop
/// session started by a foreground drag has finished dropping, then let AppKit
/// settle briefly. A drag that started no session returns almost at once.
fn await_drag_session_end(pid: i32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !drag_image_windows(pid).is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
}

/// Current hardware pointer location in global display points.
fn pointer_location() -> Option<(f64, f64)> {
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
    let location = CGEvent::new(source).ok()?.location();
    Some((location.x, location.y))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectedTextRange {
    location: u64,
    length: u64,
}

#[derive(Debug)]
struct RetainedAxElement(usize);

impl Drop for RetainedAxElement {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) }
    }
}

#[derive(Clone, Debug)]
enum OptionalStringAttr {
    Value(Option<String>),
    Unreadable,
}

#[derive(Clone, Debug)]
struct SelectionReadback {
    range: Option<SelectedTextRange>,
    selected_text: Option<String>,
    expose_selected_text: bool,
}

#[derive(Debug)]
struct FocusedSelection {
    element: RetainedAxElement,
    readback: SelectionReadback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectionConfirmation {
    range: SelectedTextRange,
    selected_text: Option<String>,
}

#[derive(Debug)]
struct DragDispatchOutcome {
    native_drag_session: bool,
    selection: Option<SelectionConfirmation>,
}

#[repr(C)]
struct CfRange {
    location: isize,
    length: isize,
}

unsafe fn optional_string_attr(element: AXUIElementRef, name: &str) -> OptionalStringAttr {
    let attr = CFString::new(name);
    let mut value: CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if error == kAXErrorNoValue || error == kAXErrorAttributeUnsupported {
        if !value.is_null() {
            CFRelease(value);
        }
        return OptionalStringAttr::Value(None);
    }
    if error != kAXErrorSuccess || value.is_null() {
        if !value.is_null() {
            CFRelease(value);
        }
        return OptionalStringAttr::Unreadable;
    }
    if CFGetTypeID(value) != CFString::type_id() {
        CFRelease(value);
        return OptionalStringAttr::Unreadable;
    }
    OptionalStringAttr::Value(Some(
        CFString::wrap_under_create_rule(value as _).to_string(),
    ))
}

fn selected_text_exposure_allowed(attributes: &[OptionalStringAttr; 7]) -> bool {
    if attributes
        .iter()
        .any(|attr| matches!(attr, OptionalStringAttr::Unreadable))
    {
        return false;
    }
    let value = |index: usize| match &attributes[index] {
        OptionalStringAttr::Value(value) => value.as_deref(),
        OptionalStringAttr::Unreadable => None,
    };
    let Some(role) = value(0).filter(|role| !role.trim().is_empty()) else {
        return false;
    };
    !cua_driver_core::snapshot_compaction::is_secure_text_field(
        &cua_driver_core::snapshot_compaction::FieldSecurityMeta {
            role,
            subrole: value(1),
            title: value(2),
            description: value(3),
            placeholder: value(4),
            identifier: value(5),
            help: value(6),
            is_password_flag: false,
        },
    )
}

unsafe fn selected_text_range(element: AXUIElementRef) -> Option<SelectedTextRange> {
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
        location: range.location as u64,
        length: range.length as u64,
    })
}

fn focused_selection(pid: i32, window_id: u32) -> Option<FocusedSelection> {
    unsafe {
        let element = crate::ax::exact_target::focused_element_in_window(pid, window_id)?;
        let element = RetainedAxElement(element as usize);
        let attributes = [
            optional_string_attr(element.0 as AXUIElementRef, "AXRole"),
            optional_string_attr(element.0 as AXUIElementRef, "AXSubrole"),
            optional_string_attr(element.0 as AXUIElementRef, "AXTitle"),
            optional_string_attr(element.0 as AXUIElementRef, "AXDescription"),
            optional_string_attr(element.0 as AXUIElementRef, "AXPlaceholderValue"),
            optional_string_attr(element.0 as AXUIElementRef, "AXIdentifier"),
            optional_string_attr(element.0 as AXUIElementRef, "AXHelp"),
        ];
        let expose_selected_text = selected_text_exposure_allowed(&attributes);
        let readback = SelectionReadback {
            range: selected_text_range(element.0 as AXUIElementRef),
            selected_text: expose_selected_text
                .then(|| copy_string_attr(element.0 as AXUIElementRef, "AXSelectedText"))
                .flatten(),
            expose_selected_text,
        };
        Some(FocusedSelection { element, readback })
    }
}

fn selection_confirmation(
    before: Option<&FocusedSelection>,
    after: Option<&FocusedSelection>,
) -> Option<SelectionConfirmation> {
    let (before, after) = (before?, after?);
    unsafe {
        if CFEqual(before.element.0 as CFTypeRef, after.element.0 as CFTypeRef) == 0 {
            return None;
        }
    }
    selection_change(&before.readback, &after.readback)
}

fn selection_change(
    before: &SelectionReadback,
    after: &SelectionReadback,
) -> Option<SelectionConfirmation> {
    let before_range = before.range?;
    let after_range = after.range?;
    if after_range.length == 0 {
        return None;
    }
    let range_changed = before_range != after_range;
    let text_changed = match (before.selected_text.as_ref(), after.selected_text.as_ref()) {
        (Some(before), Some(after)) => before != after,
        _ => false,
    };
    if !range_changed && !text_changed {
        return None;
    }
    Some(SelectionConfirmation {
        range: after_range,
        selected_text: after
            .expose_selected_text
            .then(|| after.selected_text.clone())
            .flatten(),
    })
}

fn drag_action_record(foreground: bool) -> ActionExecutionRecord {
    let (transport, requested, actual) = if foreground {
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
    ActionExecutionRecord::builder(ActionEffect::Confirmed, transport, requested)
        .actual_delivery(actual)
        .evidence(ActionEvidence {
            kind: EvidenceKind::AccessibilityReadback,
            detail: "The focused element changed to a nonempty UTF-16 selection".into(),
        })
        .build()
        .expect("confirmed drag selection has accessibility evidence")
}

fn drag_success_result(
    text: String,
    path: &str,
    focus_without_raise: bool,
    foreground: bool,
    selection: Option<SelectionConfirmation>,
) -> ToolResult {
    let mut structured = serde_json::json!({
        "path": path,
        "delivery_mode": if foreground { "foreground" } else { "background" },
        "verified": selection.is_some(),
        "effect": if selection.is_some() { "confirmed" } else { "unverifiable" },
        "focus_without_raise": focus_without_raise,
    });
    let Some(selection) = selection else {
        return ToolResult::text(text).with_structured(structured);
    };
    structured["selection_changed"] = serde_json::json!(true);
    structured["selected_text_range"] = serde_json::json!({
        "location": selection.range.location,
        "length": selection.range.length,
        "units": "utf16",
    });
    if let Some(selected_text) = selection.selected_text {
        structured["selected_text"] = serde_json::json!(selected_text);
    }
    ToolResult::text(text)
        .with_structured(structured)
        .with_action_record(drag_action_record(foreground))
}

#[cfg(test)]
mod wave400c_tests {
    use super::*;

    #[test]
    fn wave400c_drag_result_promotes_a_confirmed_selection() {
        let result = drag_success_result(
            "drag delivered".into(),
            "cgevent",
            true,
            false,
            Some(SelectionConfirmation {
                range: SelectedTextRange {
                    location: 2,
                    length: 4,
                },
                selected_text: Some("text".into()),
            }),
        );
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["effect"], "confirmed");
        assert_eq!(structured["verified"], true);
        assert_eq!(structured["selection_changed"], true);
        assert_eq!(
            structured["selected_text_range"],
            serde_json::json!({"location": 2, "length": 4, "units": "utf16"})
        );
        assert_eq!(structured["selected_text"], "text");
        let record = result.action_record.unwrap();
        assert_eq!(record.effect, ActionEffect::Confirmed);
        assert_eq!(record.evidence[0].kind, EvidenceKind::AccessibilityReadback);
    }

    fn readback(before: (u64, u64), text: &str, expose_selected_text: bool) -> SelectionReadback {
        SelectionReadback {
            range: Some(SelectedTextRange {
                location: before.0,
                length: before.1,
            }),
            selected_text: Some(text.into()),
            expose_selected_text,
        }
    }

    #[test]
    fn wave400c_selection_requires_a_nonempty_real_change() {
        let collapsed = readback((0, 0), "", true);
        let selected = readback((2, 4), "text", true);
        assert!(selection_change(&collapsed, &selected).is_some());

        let changed_text = readback((2, 4), "new!", true);
        assert!(selection_change(&selected, &changed_text).is_some());
        assert!(selection_change(&selected, &selected).is_none());
        assert!(selection_change(&selected, &collapsed).is_none());

        let missing_before = SelectionReadback {
            range: None,
            ..collapsed.clone()
        };
        assert!(selection_change(&missing_before, &selected).is_none());
    }

    #[test]
    fn wave400c_secure_or_unreadable_metadata_never_exposes_selected_text() {
        let ordinary: [OptionalStringAttr; 7] = std::array::from_fn(|index| {
            OptionalStringAttr::Value((index == 0).then(|| "AXTextField".into()))
        });
        assert!(selected_text_exposure_allowed(&ordinary));

        for index in 0..ordinary.len() {
            let mut unreadable = ordinary.clone();
            unreadable[index] = OptionalStringAttr::Unreadable;
            assert!(
                !selected_text_exposure_allowed(&unreadable),
                "index {index}"
            );
        }
        for (role, title) in [
            ("AXSecureTextField", ""),
            ("AXTextField", "Password"),
            ("AXTextField", "One-time code"),
        ] {
            let mut secure = ordinary.clone();
            secure[0] = OptionalStringAttr::Value(Some(role.into()));
            secure[2] = OptionalStringAttr::Value(Some(title.into()));
            assert!(!selected_text_exposure_allowed(&secure));
        }

        let before = readback((0, 0), "", false);
        let after = readback((0, 4), "secret", false);
        let confirmation = selection_change(&before, &after).unwrap();
        assert_eq!(confirmation.selected_text, None);
        let result = drag_success_result(
            "selection changed".into(),
            "cgevent",
            true,
            false,
            Some(confirmation),
        );
        assert!(result
            .structured_content
            .unwrap()
            .get("selected_text")
            .is_none());
        assert!(!result.action_record.unwrap().evidence[0]
            .detail
            .contains("secret"));
    }
}

/// Structured record of a brief foreground step, for the agent and the log.
fn brief_foreground_step(restored_frontmost: bool, restored_pointer: Option<bool>) -> Value {
    serde_json::json!({
        "used": true,
        "reason": "drag_and_drop_session",
        "allowed_by": cua_driver_core::delivery::BRIEF_FOREGROUND_ENV,
        "background_attempt": "no_drop",
        "restored_frontmost": restored_frontmost,
        "restored_pointer": restored_pointer,
    })
}

fn brief_foreground_note(restored_frontmost: bool, restored_pointer: Option<bool>) -> String {
    let mut note = String::from(
        "Brief foreground step: the background drag started a macOS drag-and-drop session, \
         which follows the real pointer, so the driver repeated this one drag in the \
         foreground because the operator allowed it. The window came forward and the \
         pointer moved for the drag only.",
    );
    note.push_str(if restored_frontmost {
        " The previous frontmost app is back in front."
    } else {
        " The previous frontmost app did NOT come back to the front; tell the user."
    });
    note.push_str(match restored_pointer {
        Some(true) => " The pointer is back where it was.",
        Some(false) => " The pointer did NOT return to where it was; tell the user.",
        None => " The pointer position could not be checked.",
    });
    note
}

/// `kCGDraggingWindowLevel`: AppKit shows the image of a live (or sliding
/// back) drag-and-drop session in a window of the source app at this level.
const DRAGGING_WINDOW_LEVEL: i32 = 500;

const ELECTRON_BACKGROUND_DRAG_UNAVAILABLE: &str = "Background drag is unavailable for this \
     Electron window on macOS: a window-routed drag cannot hold the page's mouse button \
     (Chromium reads it from the hardware), and the app's main process did not take the drag.";

fn electron_drag_refusal(reason: &str) -> ToolResult {
    cua_driver_core::delivery::background_unavailable_result_with_suggestion(
        format!("{ELECTRON_BACKGROUND_DRAG_UNAVAILABLE} {DRAG_LADDER} ({reason})"),
        "background_unavailable",
        "the Electron app's main process could not take the drag",
        serde_json::json!({
            "effect": "refused",
            "event_kind": "mouse_drag",
            "reason": "electron_main_process_unavailable",
            "requires": ["delivery_mode:foreground", "window_id"],
        }),
        DRAG_LADDER,
    )
}

fn drag_image_windows(pid: i32) -> std::collections::HashSet<u32> {
    crate::windows::pid_on_screen_windows_any_layer(pid)
        .into_iter()
        .filter(|window| window.layer == DRAGGING_WINDOW_LEVEL)
        .map(|window| window.window_id)
        .collect()
}

fn drag_and_drop_session_result(pid: i32, wid: u32, suffix: String) -> ToolResult {
    cua_driver_core::delivery::background_unavailable_result_with_suggestion(
        format!(
            "The background drag in pid {pid} window {wid} started a macOS drag-and-drop \
             session (for example reordering a list, moving a file or dragging page content). \
             macOS tracks drag-and-drop with the real pointer, so the drop did not land at the \
             requested point; the driver released at once. {DRAG_LADDER}{suffix}"
        ),
        "background_unavailable",
        "drag-and-drop sessions follow the hardware pointer on macOS",
        serde_json::json!({
            "effect": "no_drop",
            "event_kind": "mouse_drag",
            "reason": "drag_and_drop_session",
            "requires": ["delivery_mode:foreground", "window_id"],
        }),
        DRAG_LADDER,
    )
}

fn window_id_required(foreground: bool) -> ToolResult {
    let mode = if foreground {
        "foreground"
    } else {
        "background"
    };
    super::pre_dispatch_refusal(
        "window_id_required",
        format!(
            "A {mode} drag on macOS needs window_id: the gesture is addressed to one exact \
         window. Read it from list_windows or get_window_state and retry."
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn window_drag_without_window_id_is_refused_in_both_modes() {
        let tool = DragTool::new(Arc::new(ToolState::new(false, false, None)));
        for delivery_mode in [None, Some("background"), Some("foreground")] {
            let mut args = serde_json::json!({
                "pid": 4242,
                "from_x": 1, "from_y": 2, "to_x": 3, "to_y": 4,
            });
            if let Some(mode) = delivery_mode {
                args["delivery_mode"] = serde_json::json!(mode);
            }
            let result = tool.invoke(args).await;
            assert_eq!(result.is_error, Some(true));
            let structured = result.structured_content.expect("structured refusal");
            assert_eq!(structured["code"], "window_id_required");
            assert_eq!(structured["effect"], "refused");
        }
    }

    #[test]
    fn brief_foreground_step_reports_what_was_restored() {
        let step = super::brief_foreground_step(true, Some(true));
        assert_eq!(step["used"], true);
        assert_eq!(step["reason"], "drag_and_drop_session");
        assert_eq!(step["allowed_by"], "CUA_DRIVER_ALLOW_BRIEF_FOREGROUND");
        assert_eq!(step["restored_frontmost"], true);
        assert_eq!(step["restored_pointer"], true);
        let unknown = super::brief_foreground_step(false, None);
        assert_eq!(unknown["restored_frontmost"], false);
        assert!(unknown["restored_pointer"].is_null());

        let note = super::brief_foreground_note(true, Some(true));
        assert!(note.contains("Brief foreground step"));
        assert!(note.contains("frontmost app is back"));
        assert!(note.contains("pointer is back"));
        let failed = super::brief_foreground_note(false, Some(false));
        assert!(failed.contains("did NOT come back"));
        assert!(failed.contains("did NOT return"));
    }

    #[test]
    fn schema_says_macos_drag_runs_in_the_background() {
        let def = def();
        assert!(def
            .description
            .contains("runs in the background by default"));
        assert!(def.description.contains("browser_pointer"));
        assert!(!def.description.contains("no background drag"));
        let mode = def.input_schema["properties"]["delivery_mode"]["description"]
            .as_str()
            .unwrap();
        assert!(mode.contains("\"background\" (default)"));
        assert!(mode.contains("without moving the pointer"));
    }

    fn text_of(result: &ToolResult) -> String {
        match &result.content[0] {
            cua_driver_core::protocol::Content::Text { text, .. } => text.clone(),
            other => panic!("text content expected: {other:?}"),
        }
    }

    /// cua-driver#22: every drag fallback names a non-drag route by
    /// element_token, then the keyboard, and the foreground last with its
    /// pointer takeover, in the schema and in the refusals themselves.
    #[test]
    fn drag_guidance_puts_token_and_keyboard_before_foreground() {
        use crate::tools::pixel_route::tests::assert_foreground_last;
        let rungs = ["element_token", "keyboard"];
        let def = def();
        assert_foreground_last(&def.description, "The result is not", &rungs);
        let mode = def.input_schema["properties"]["delivery_mode"]["description"]
            .as_str()
            .unwrap();
        assert!(mode.contains("use it last"), "{mode}");
        assert!(
            mode.find("use it last") < mode.find("element_token"),
            "{mode}"
        );

        for (result, anchor) in [
            (
                drag_and_drop_session_result(1, 2, String::new()),
                "drop did not land",
            ),
            (
                electron_drag_refusal("inspector off"),
                "did not take the drag",
            ),
        ] {
            assert_foreground_last(&text_of(&result), anchor, &rungs);
            let structured = result.structured_content.expect("structured refusal");
            assert_foreground_last(
                structured["suggestion"].as_str().expect("suggestion"),
                "",
                &rungs,
            );
            assert_eq!(structured["escalation"]["recommended"], "foreground");
        }
    }
}
