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
    ax_get_window_id, copy_children_checked, copy_element_attr, copy_string_attr, kAXErrorSuccess,
    perform_action, AXUIElementCopyAttributeValue, AXUIElementCreateApplication, AXUIElementRef,
    AXUIElementSetMessagingTimeout,
};

use super::ToolState;

// This seam keeps the menu identity and its lease together across the async
// admission and the worker that closes it.
pub(super) async fn dispatch_owned_cancel<C, P, A, F, T>(
    clock: &(impl cua_driver_core::actionability::Clock + Sync),
    budget: Duration,
    context: C,
    probe: P,
    perform: A,
) -> Result<T, ToolResult>
where
    C: Sync,
    P: for<'a> FnMut(
            &'a C,
            Duration,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = cua_driver_core::actionability::ActionabilityReport,
                    > + Send
                    + 'a,
            >,
        > + Send,
    A: FnOnce(C) -> F,
    F: std::future::Future<Output = T>,
{
    super::actionability::dispatch_semantic(clock, budget, context, probe, perform).await
}

pub(super) async fn dispatch_show_menu<C, P, A, F, T>(
    clock: &(impl cua_driver_core::actionability::Clock + Sync),
    budget: Duration,
    advertises_show_menu: bool,
    context: C,
    probe: P,
    perform: A,
) -> Result<T, ToolResult>
where
    C: Sync,
    P: for<'a> FnMut(
            &'a C,
            Duration,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = cua_driver_core::actionability::ActionabilityReport,
                    > + Send
                    + 'a,
            >,
        > + Send,
    A: FnOnce(C) -> F,
    F: std::future::Future<Output = T>,
{
    if advertises_show_menu {
        super::actionability::dispatch_semantic(clock, budget, context, probe, perform).await
    } else {
        Ok(perform(context).await)
    }
}

pub struct RightClickTool {
    state: Arc<ToolState>,
}

impl RightClickTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn lifecycle_session(args: &Value) -> String {
    args.get("_session_id")
        .or_else(|| args.get("session"))
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned()
}

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
                "action": { "type": "string", "enum": ["open", "close"], "description": "open right-clicks; close AXCancels only this tool's opener-owned menu." },
                "opener_id": { "type": "integer", "minimum": 1, "description": "Numeric owner id returned in right_click open text; required for close." },
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
        if args.get("simulate").is_some() {
            return super::invalid_arguments(
                "right_click does not support simulate; nothing was sent.",
            );
        }
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
    async fn close_owned(&self, pid: i32, args: &Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let window_id = match args.require_u32("window_id") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let opener_id = match args.require_u64("opener_id") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let session = lifecycle_session(args);
        let Some((menu, target)) =
            crate::ax::exact_target::context_menu_for_close(&session, pid, window_id, opener_id)
        else {
            return ToolResult::error(
                "right_click close refused: opener_id is stale, foreign, or belongs to another session/window",
            );
        };
        let _mutation_lease = match super::gate_background_window_action(
            pid,
            window_id,
            Some(menu.0),
            cua_driver_core::background_input::BackgroundAction::AxSemantic,
        )
        .await
        {
            Ok(lease) => lease,
            Err(error) => return error,
        };
        let clock = super::actionability::semantic_clock();
        let result = match dispatch_owned_cancel(
            &clock,
            cua_driver_core::actionability::DEFAULT_BUDGET,
            (_mutation_lease, menu, target),
            |(lease, menu, _), _| {
                let target = unsafe { crate::ax::snapshot::RetainedElement::retain(menu.0) };
                super::actionability::semantic_probe(
                    Some(lease),
                    target,
                    pid,
                    window_id,
                    super::actionability::semantic::SemanticIntent::PerformAction(
                        "AXCancel".into(),
                    ),
                )
            },
            |(lease, menu, target)| async move {
                latency::spawn_action_blocking(move || unsafe {
                    let _lease = lease;
                    let error = perform_action(menu.0 as AXUIElementRef, "AXCancel");
                    if error != kAXErrorSuccess {
                        return Err(format!("AXCancel failed with AXError {error}"));
                    }
                    let deadline = Instant::now() + Duration::from_millis(750);
                    while Instant::now() < deadline {
                        let menus =
                            ax_menus(pid, target.as_ref().map(|target| target.0), Some(window_id))
                                .ok_or_else(|| {
                                    "context-menu close readback is unavailable".to_owned()
                                })?;
                        if !menus.iter().any(|candidate| {
                            CFEqual(candidate.0 as CFTypeRef, menu.0 as CFTypeRef) != 0
                        }) {
                            return Ok(());
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Err("context menu did not disappear after AXCancel".into())
                })
                .await
            },
        )
        .await
        {
            Ok(result) => result,
            Err(refusal) => return refusal,
        };
        match result {
            Ok(Ok(())) => {
                crate::ax::exact_target::finish_context_menu_close(opener_id);
                let record = ActionExecutionRecord::builder(
                    ActionEffect::Confirmed,
                    ActionTransport::MacosAxAction,
                    RequestedDelivery::Background,
                )
                .actual_delivery(ActualDelivery::Background)
                .evidence(ActionEvidence {
                    kind: EvidenceKind::AccessibilityReadback,
                    detail: "The opener-owned context menu disappeared after AXCancel".into(),
                })
                .build()
                .expect("right-click close record");
                ToolResult::text(format!("Context menu {opener_id} closed with AXCancel."))
                    .with_structured(serde_json::json!({
                        "path": "ax",
                        "verified": true,
                        "effect": "confirmed",
                        "menu_opened": false,
                        "menu_items": []
                    }))
                    .with_action_record(record)
            }
            Ok(Err(error)) => ToolResult::error(format!("right_click close failed: {error}")),
            Err(error) => ToolResult::error(format!("right_click close task failed: {error}")),
        }
    }

    async fn invoke_in(&self, args: Value, space: super::window_points::PointSpace) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let action = args.opt_str("action").unwrap_or_else(|| "open".into());
        if action == "close" {
            return self.close_owned(pid, &args).await;
        }
        if action != "open" {
            return super::invalid_arguments("right_click action must be open or close");
        }
        // delivery_mode: foreground briefly fronts the window before the pixel
        // right-click (the explicit last resort for surfaces that drop
        // background CGEvents), via the same skylight assist click uses. The AX
        // (AXShowMenu) path is background-by-design and untouched.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
        let session = lifecycle_session(&args);

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

        // The person is working in this app: nothing is sent.
        if !delivery_mode.is_foreground() {
            if let Err(refusal) = super::gate_user_takeover(pid, window_id) {
                return refusal;
            }
        }

        // ── AX element path ──────────────────────────────────────────────────
        if let (Some(idx), Some(wid), Some(element_guard)) =
            (element_index, window_id, element_guard)
        {
            let element_ptr = element_guard.as_ptr();
            let ownership = OpenOwnership {
                session: session.clone(),
                generation: crate::ax::exact_target::context_menu_generation(&session),
                pid,
                window_id: wid,
                target_frame: unsafe {
                    crate::ax::bindings::element_screen_rect(element_ptr as AXUIElementRef)
                },
                target_element: {
                    unsafe { core_foundation::base::CFRetain(element_ptr as CFTypeRef) };
                    Some(crate::ax::exact_target::ContextMenuElement(element_ptr))
                },
                click: None,
            };

            let mutation_lease = match super::gate_background_window_action(
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

            let action_guard = element_guard.clone();
            let advertised = latency::spawn_blocking(move || {
                super::actionability::action_advertised(
                    action_guard.as_ptr() as AXUIElementRef,
                    "AXShowMenu",
                )
            })
            .await
            .unwrap_or_else(|error| {
                cua_driver_core::actionability::Evidence::Unknown(format!(
                    "action read worker failed: {error}"
                ))
            });
            let advertises_show_menu = super::actionability::may_use_semantic_route(&advertised);

            // The context menu steals key focus from the user's app when the
            // target is in the background; the guard hands it back.
            let probe_target = element_guard.clone();
            let clock = super::actionability::semantic_clock();
            let perform = |mutation_lease| async move {
                latency::spawn_action_blocking(move || -> anyhow::Result<_> {
                let _action = latency::span(latency::Stage::ActionBlocking);
                let element_ptr = element_guard.as_ptr();
                let input_watch = MenuInputWatch::start(pid);
                let before = MenuSnapshot::read(pid, Some(element_ptr), Some(wid));
                let action = move |dispatch| {
                    let _mutation_lease = mutation_lease;
                    let _element_guard = element_guard;
                    let _key_focus = crate::key_focus_guard::protect(pid);
                    let element = element_ptr as AXUIElementRef;
                    unsafe {
                        AXUIElementSetMessagingTimeout(element, 0.25);
                    }
                    let result = ax_show_menu(element_ptr, idx, pid, wid, advertises_show_menu, dispatch);
                    unsafe {
                        AXUIElementSetMessagingTimeout(element, 0.0);
                    }
                    result
                };
                let (completion, mut menu) = observe_right_click_action(before, action, || {
                    MenuSnapshot::read(pid, Some(element_ptr), Some(wid))
                });
                menu.input_quiet = input_watch.no_person_could_open();
                let result = match completion {
                    ObservedAction::Completed(result) => result,
                    ObservedAction::TimedOut if menu.opened => {
                        let mut menu = menu;
                        menu.completion_indeterminate = true;
                        return Ok((
                            ElementRightClick::Ax(
                                "Right-click dispatched; native completion is still running".into(),
                            ),
                            menu,
                        ))
                    }
                    ObservedAction::TimedOut => {
                        return Err(anyhow::anyhow!(
                            "right-click native action exceeded its completion budget; a menu may be open"
                        ))
                    }
                };
                result.map(|delivery| (delivery, menu))
            }).await
            };
            let result = match dispatch_show_menu(
                &clock,
                cua_driver_core::actionability::DEFAULT_BUDGET,
                advertises_show_menu,
                mutation_lease,
                |lease, _| {
                    super::actionability::route_semantic_probe(
                        &advertised,
                        Some(lease),
                        probe_target.clone(),
                        pid,
                        wid,
                        super::actionability::semantic::SemanticIntent::RightClick,
                    )
                },
                perform,
            )
            .await
            {
                Ok(result) => result,
                Err(refusal) => return refusal,
            };

            return match result {
                Ok(Ok((ElementRightClick::Ax(msg), menu))) => right_click_result_with_ownership(
                    &msg,
                    "ax",
                    menu,
                    false,
                    Some(ownership.clone()),
                ),
                Ok(Ok((ElementRightClick::Pixel(msg), menu))) => right_click_result_with_ownership(
                    &msg,
                    "cgevent",
                    menu,
                    false,
                    Some(ownership.clone()),
                ),
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

        let mutation_lease = if !delivery_mode.is_foreground() {
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
        let ownership = OpenOwnership {
            session: session.clone(),
            generation: crate::ax::exact_target::context_menu_generation(&session),
            pid,
            window_id: window_id.unwrap_or_default(),
            target_frame: None,
            target_element: None,
            click: Some((screen_x, screen_y)),
        };
        let result = latency::spawn_blocking(move || -> anyhow::Result<_> {
            let _action = latency::span(latency::Stage::ActionBlocking);
            let do_it =
                move |dispatch: std::sync::mpsc::Sender<ActionEvent>| -> anyhow::Result<()> {
                    let _mutation_lease = mutation_lease;
                    let _key_focus = (!fg)
                        .then(|| crate::key_focus_guard::protect(pid))
                        .flatten();
                    super::window_points::check_guard(&dispatch_guard)?;
                    let m: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    let _ = dispatch.send(ActionEvent::Dispatching(Instant::now()));
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
            let input_watch = MenuInputWatch::start(pid);
            // The element under the click point, when it is inside the exact
            // window: its menu hangs off it or one of its ancestors.
            let anchor = window_id
                .and_then(|wid| unsafe { element_in_window_at(pid, wid, screen_x, screen_y) });
            let anchor_ptr = anchor.as_ref().map(|anchor| anchor.0);
            let before = MenuSnapshot::read(pid, anchor_ptr, window_id);
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
            let (completion, mut menu) = observe_right_click_action(before, action, || {
                MenuSnapshot::read(pid, anchor_ptr, window_id)
            });
            menu.input_quiet = input_watch.no_person_could_open();
            drop(anchor);
            let result = match completion {
                ObservedAction::Completed(result) => result,
                ObservedAction::TimedOut if menu.opened => {
                    let mut menu = menu;
                    menu.completion_indeterminate = true;
                    return Ok(((), menu));
                }
                ObservedAction::TimedOut => {
                    return Err(anyhow::anyhow!(
                    "right-click native action exceeded its completion budget; a menu may be open"
                ))
                }
            };
            result.map(|()| ((), menu))
        })
        .await;
        if let Ok(Err(error)) = &result {
            if let Some(refusal) = super::window_points::moved_refusal(error, space) {
                return refusal;
            }
        }
        match result {
            Ok(Ok(((), menu))) => right_click_result_with_ownership(
                &format!(
                    "Right-clicked{mod_suffix} at ({screen_x:.1}, {screen_y:.1}) ({}).",
                    super::pixel_route::delivery_note(route)
                ),
                super::pixel_route::path_label(route),
                menu,
                fg,
                Some(ownership),
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
pub(super) enum ElementRightClick {
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

#[derive(Debug, PartialEq, Eq)]
struct RetainedAxElement(usize);

impl Clone for RetainedAxElement {
    fn clone(&self) -> Self {
        unsafe { core_foundation::base::CFRetain(self.0 as CFTypeRef) };
        Self(self.0)
    }
}

impl Drop for RetainedAxElement {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) }
    }
}

impl RetainedAxElement {
    /// # Safety
    ///
    /// `element` must be a valid, live `AXUIElementRef`.
    unsafe fn retain(element: usize) -> Self {
        core_foundation::base::CFRetain(element as CFTypeRef);
        Self(element)
    }
}

#[derive(Debug)]
struct MenuSnapshot {
    window_ids: Option<HashSet<u32>>,
    ax_menus: Option<Vec<RetainedAxElement>>,
    /// The first `linked` entries of `ax_menus` hang off the right-clicked
    /// element or one of its AX ancestors inside the window: AppKit attaches
    /// a view's context menu there (Finder's list view holds a row's menu).
    linked: usize,
}

impl MenuSnapshot {
    fn read(pid: i32, target: Option<usize>, window_id: Option<u32>) -> Self {
        let (ax_menus, linked) = match unsafe { ax_menus_linked(pid, target, window_id) } {
            Some((menus, linked)) => (Some(menus), linked),
            None => (None, 0),
        };
        Self {
            window_ids: menu_window_ids(pid),
            ax_menus,
            linked,
        }
    }

    fn is_linked(&self, menu: &RetainedAxElement) -> bool {
        self.ax_menus.as_ref().is_some_and(|menus| {
            menus[..self.linked.min(menus.len())]
                .iter()
                .any(|linked| unsafe { CFEqual(linked.0 as CFTypeRef, menu.0 as CFTypeRef) != 0 })
        })
    }
}

/// The element under a screen point when it is proven inside `window_id`.
///
/// # Safety
///
/// Calls into Accessibility for `pid`.
unsafe fn element_in_window_at(
    pid: i32,
    window_id: u32,
    x: f64,
    y: f64,
) -> Option<RetainedAxElement> {
    let element =
        RetainedAxElement(crate::ax::bindings::element_at_screen_position(pid, x, y)? as usize);
    (crate::ax::exact_target::element_window_id(element.0 as AXUIElementRef) == Some(window_id))
        .then_some(element)
}

/// Physical input that could have opened a context menu instead of the
/// right-click: a mouse-button press anywhere, or a key press while the
/// target app could have received it (it was frontmost, or an app
/// activation happened). Pointer movement never counts, and typing counts
/// only when another app was not proven frontmost throughout.
struct MenuInputWatch {
    mouse_before: Option<u64>,
    key_before: Option<u64>,
    from: Instant,
    front: super::sheet_close::OtherAppFront,
}

impl MenuInputWatch {
    fn start(pid: i32) -> Self {
        Self {
            front: super::sheet_close::OtherAppFront::capture(pid),
            mouse_before: super::physical_mouse_down_idle_ms(),
            key_before: super::physical_key_down_idle_ms(),
            from: Instant::now(),
        }
    }

    fn no_person_could_open(&self) -> bool {
        let elapsed = u64::try_from(self.from.elapsed().as_millis()).unwrap_or(u64::MAX);
        let quiet = |before: Option<u64>, now: Option<u64>| {
            before.is_some() && now.is_some_and(|idle| idle >= elapsed)
        };
        menu_input_attributable(
            quiet(self.mouse_before, super::physical_mouse_down_idle_ms()),
            quiet(self.key_before, super::physical_key_down_idle_ms()),
            || self.front.held_throughout(),
        )
    }
}

fn menu_input_attributable(
    mouse_quiet: bool,
    key_quiet: bool,
    other_app_front_throughout: impl FnOnce() -> bool,
) -> bool {
    mouse_quiet && (key_quiet || other_app_front_throughout())
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
    ax_menus_linked(pid, target, window_id).map(|(menus, _)| menus)
}

/// AX parents above the right-clicked element searched for its menu.
const MAX_MENU_ANCHOR_ANCESTRY: usize = 12;

/// The open `AXMenu`s that could be this right-click's: first those that are
/// direct children of `target` or of its AX ancestors below its window
/// (the count is returned), then direct children of the application and of
/// the window. `None` when a read failed.
unsafe fn ax_menus_linked(
    pid: i32,
    target: Option<usize>,
    window_id: Option<u32>,
) -> Option<(Vec<RetainedAxElement>, usize)> {
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
    if let Some(target) = target {
        let mut current = RetainedAxElement::retain(target);
        for _ in 0..=MAX_MENU_ANCHOR_ANCESTRY {
            if matches!(
                copy_string_attr(current.0 as AXUIElementRef, "AXRole").as_deref(),
                Some("AXWindow") | Some("AXSheet") | Some("AXApplication") | None
            ) {
                break;
            }
            if !collect_direct_menus(current.0 as AXUIElementRef, &mut menus) {
                return None;
            }
            let Some(parent) = copy_element_attr(current.0 as AXUIElementRef, "AXParent") else {
                break;
            };
            current = RetainedAxElement(parent as usize);
        }
    }
    let linked = menus.len();
    if !collect_direct_menus(app.0 as AXUIElementRef, &mut menus) {
        return None;
    }
    if let Some(window) = window {
        if !collect_direct_menus(window.0 as AXUIElementRef, &mut menus) {
            return None;
        }
    }
    Some((menus, linked))
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
                menu: Some(menu.clone()),
                unique_ax_menu: true,
                unique_window: false,
                completion_indeterminate: false,
                linked: false,
                input_quiet: false,
                continuous: false,
                window_id: match (&before.window_ids, &after.window_ids) {
                    (Some(before_ids), Some(after_ids)) => {
                        after_ids.difference(before_ids).next().copied()
                    }
                    _ => None,
                },
            });
        }
    }
    if let (Some(before_ids), Some(after_ids)) = (&before.window_ids, &after.window_ids) {
        if after_ids.difference(before_ids).next().is_some() {
            return Some(MenuObservation {
                opened: true,
                items: Vec::new(),
                evidence: MenuEvidence::Window,
                menu: None,
                unique_ax_menu: false,
                unique_window: false,
                completion_indeterminate: false,
                linked: false,
                input_quiet: false,
                continuous: false,
                window_id: after_ids.difference(before_ids).next().copied(),
            });
        }
    }
    None
}

fn new_menu_observation_between(
    before: &MenuSnapshot,
    sent: &MenuSnapshot,
    after: &MenuSnapshot,
    require_absent_at_sent: bool,
) -> Option<MenuObservation> {
    let (new_window_id, unique_window) =
        novel_window_between(before, sent, after, require_absent_at_sent);
    if let (Some(before_menus), Some(sent_menus), Some(after_menus)) =
        (&before.ax_menus, &sent.ax_menus, &after.ax_menus)
    {
        let mut new_menus = Vec::new();
        for candidate in after_menus {
            let absent = |menus: &Vec<RetainedAxElement>| {
                !menus.iter().any(|old| unsafe {
                    CFEqual(old.0 as CFTypeRef, candidate.0 as CFTypeRef) != 0
                })
            };
            if absent(before_menus) && (!require_absent_at_sent || absent(sent_menus)) {
                new_menus.push(candidate);
            }
        }
        if let Some(candidate) = new_menus.first() {
            return Some(MenuObservation {
                opened: true,
                items: unsafe { menu_item_titles(candidate.0 as AXUIElementRef) },
                evidence: MenuEvidence::Accessibility,
                menu: Some((*candidate).clone()),
                unique_ax_menu: new_menus.len() == 1,
                unique_window,
                continuous: false,
                completion_indeterminate: false,
                linked: after.is_linked(candidate),
                input_quiet: false,
                window_id: new_window_id,
            });
        }
    }
    if let (Some(before_ids), Some(sent_ids), Some(after_ids)) =
        (&before.window_ids, &sent.window_ids, &after.window_ids)
    {
        if after_ids
            .difference(before_ids)
            .any(|candidate| !require_absent_at_sent || !sent_ids.contains(candidate))
        {
            return Some(MenuObservation {
                opened: true,
                items: Vec::new(),
                evidence: MenuEvidence::Window,
                menu: None,
                unique_ax_menu: false,
                unique_window,
                continuous: false,
                completion_indeterminate: false,
                linked: false,
                input_quiet: false,
                window_id: new_window_id,
            });
        }
    }
    None
}

fn novel_window_between(
    before: &MenuSnapshot,
    sent: &MenuSnapshot,
    after: &MenuSnapshot,
    require_absent_at_sent: bool,
) -> (Option<u32>, bool) {
    let (Some(before_ids), Some(sent_ids), Some(after_ids)) =
        (&before.window_ids, &sent.window_ids, &after.window_ids)
    else {
        return (None, false);
    };
    let new: Vec<u32> = after_ids
        .difference(before_ids)
        .filter(|candidate| !require_absent_at_sent || !sent_ids.contains(candidate))
        .copied()
        .collect();
    (new.first().copied(), new.len() == 1)
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
    Dispatching(Instant),
    Sent(Instant),
    Done,
}

enum ObservedAction<T> {
    Completed(T),
    TimedOut,
}

/// Time source for the right-click observation window. Production uses the
/// system clock; tests substitute a clock that only advances when the
/// observer sleeps, so deadline ordering is fixed by the test, not the runner.
trait ObservationClock {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
    fn recv_timeout<M>(
        &self,
        receiver: &std::sync::mpsc::Receiver<M>,
        timeout: Duration,
    ) -> Result<M, std::sync::mpsc::RecvTimeoutError>;
}

struct SystemClock;

impl ObservationClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn recv_timeout<M>(
        &self,
        receiver: &std::sync::mpsc::Receiver<M>,
        timeout: Duration,
    ) -> Result<M, std::sync::mpsc::RecvTimeoutError> {
        receiver.recv_timeout(timeout)
    }
}

#[cfg(test)]
impl<T> ObservedAction<T> {
    fn unwrap(self) -> T {
        match self {
            Self::Completed(value) => value,
            Self::TimedOut => panic!("right-click observation timed out"),
        }
    }
}

fn observe_right_click_action<T: Send + 'static>(
    before: MenuSnapshot,
    action: impl FnOnce(std::sync::mpsc::Sender<ActionEvent>) -> T + Send + 'static,
    read: impl FnMut() -> MenuSnapshot,
) -> (ObservedAction<T>, MenuObservation) {
    observe_right_click_action_with(&SystemClock, before, action, read)
}

fn observe_right_click_action_with<T: Send + 'static>(
    clock: &impl ObservationClock,
    before: MenuSnapshot,
    action: impl FnOnce(std::sync::mpsc::Sender<ActionEvent>) -> T + Send + 'static,
    mut read: impl FnMut() -> MenuSnapshot,
) -> (ObservedAction<T>, MenuObservation) {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let action_tx = event_tx.clone();
    crate::input::worker_context::spawn_thread(move || {
        let result = action(action_tx);
        let _ = result_tx.send(result);
        let _ = event_tx.send(ActionEvent::Done);
    });
    let (sent_at, pre_dispatch) = match clock.recv_timeout(&event_rx, Duration::from_millis(250)) {
        Ok(ActionEvent::Dispatching(sent_at)) => (sent_at, true),
        Ok(ActionEvent::Sent(sent_at)) => (sent_at, false),
        _ => return (ObservedAction::TimedOut, MenuObservation::none()),
    };
    let sent_baseline = read();
    let deadline = sent_at + MENU_OBSERVATION;
    let mut previous: Option<MenuObservation> = None;
    let observation = poll_until_deadline(
        deadline,
        Duration::from_millis(15),
        || clock.now(),
        |duration| clock.sleep(duration),
        || {
            let after = read();
            let current =
                new_menu_observation_between(&before, &sent_baseline, &after, !pre_dispatch);
            let continuous = match (&previous, &current) {
                (Some(before), Some(after)) if before.window_id == after.window_id => {
                    match (&before.menu, &after.menu) {
                        (Some(left), Some(right)) => unsafe {
                            CFEqual(left.0 as CFTypeRef, right.0 as CFTypeRef) != 0
                        },
                        (None, None) => true,
                        _ => false,
                    }
                }
                _ => false,
            };
            previous = current.clone();
            if continuous {
                current.map(|mut observation| {
                    observation.continuous = true;
                    observation
                })
            } else {
                None
            }
        },
    )
    .unwrap_or_else(MenuObservation::none);
    let result = clock
        .recv_timeout(&result_rx, Duration::from_millis(250))
        .map(ObservedAction::Completed)
        .unwrap_or(ObservedAction::TimedOut);
    (result, observation)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MenuObservation {
    opened: bool,
    items: Vec<String>,
    evidence: MenuEvidence,
    menu: Option<RetainedAxElement>,
    unique_ax_menu: bool,
    unique_window: bool,
    continuous: bool,
    completion_indeterminate: bool,
    window_id: Option<u32>,
    /// The menu hangs off the right-clicked element or one of its AX
    /// ancestors in the window (see [`MenuSnapshot::linked`]).
    linked: bool,
    /// No physical keyboard or mouse input from the baseline read until the
    /// observation ended: the person did not open it.
    input_quiet: bool,
}

impl MenuObservation {
    fn none() -> Self {
        Self {
            opened: false,
            items: Vec::new(),
            evidence: MenuEvidence::None,
            menu: None,
            unique_ax_menu: false,
            unique_window: false,
            continuous: false,
            completion_indeterminate: false,
            linked: false,
            input_quiet: false,
            window_id: None,
        }
    }
}

#[derive(Clone)]
struct OpenOwnership {
    session: String,
    generation: u64,
    pid: i32,
    window_id: u32,
    target_frame: Option<[f64; 4]>,
    target_element: Option<crate::ax::exact_target::ContextMenuElement>,
    click: Option<(f64, f64)>,
}

fn spatially_attributed(menu: &MenuObservation, ownership: &OpenOwnership) -> bool {
    let Some(window_id) = menu.window_id else {
        return false;
    };
    let Some(frame) = crate::windows::all_windows_any_layer()
        .into_iter()
        .find(|window| {
            window.pid == ownership.pid && window.window_id == window_id && window.layer == 101
        })
        .map(|window| window.bounds)
    else {
        return false;
    };
    // A new AX menu borrows a new menu window's position only when it is
    // that window's menu: its frame matches the window's.
    let ax_frame = menu.menu.as_ref().and_then(|native| unsafe {
        crate::ax::bindings::element_screen_rect(native.0 as AXUIElementRef)
    });
    if !spatial_evidence_is_one_menu(
        menu.evidence,
        ax_frame,
        [frame.x, frame.y, frame.width, frame.height],
    ) {
        return false;
    }
    if let Some((x, y)) = ownership.click {
        return (frame.x - x).abs() <= 96.0 && (frame.y - y).abs() <= 96.0;
    }
    if let Some([x, y, w, h]) = ownership.target_frame {
        let margin = 64.0;
        return frame.x >= x - margin
            && frame.y >= y - margin
            && frame.x <= x + w + margin
            && frame.y <= y + h + margin;
    }
    false
}

/// Window evidence is the menu window itself. Accessibility evidence pairs
/// an AX menu with a separately found new menu window, so it counts only
/// when the AX menu's frame matches that window's.
fn spatial_evidence_is_one_menu(
    evidence: MenuEvidence,
    ax_frame: Option<[f64; 4]>,
    window_frame: [f64; 4],
) -> bool {
    match evidence {
        MenuEvidence::Window => true,
        MenuEvidence::Accessibility => {
            ax_frame.is_some_and(|ax| crate::ax::context_menus::menu_fits_window(ax, window_frame))
        }
        MenuEvidence::None => false,
    }
}

#[allow(dead_code)]
fn right_click_result(
    message: &str,
    path: &str,
    menu: MenuObservation,
    foreground: bool,
) -> ToolResult {
    right_click_result_with_ownership(message, path, menu, foreground, None)
}

/// How long after the right-click is sent a new menu is looked for. The
/// observation returns at the second consistent sighting; Finder showed its
/// list-view menu about 130 ms after the call started, past the old 150 ms
/// bound once the reads themselves were counted.
const MENU_OBSERVATION: Duration = Duration::from_millis(400);

/// A new menu confirms the right-click only when it is tied to it: it hangs
/// off the clicked element or an ancestor (`linked`), or its menu window
/// opened at the click (`spatial`), and no physical input arrived while it
/// was watched. Any other new menu of the app stays unverifiable.
fn menu_confirms(menu: &MenuObservation, spatial: bool) -> bool {
    menu.opened
        && menu.evidence != MenuEvidence::None
        && menu.input_quiet
        && (menu.linked || spatial)
}

fn right_click_result_with_ownership(
    message: &str,
    path: &str,
    menu: MenuObservation,
    foreground: bool,
    ownership: Option<OpenOwnership>,
) -> ToolResult {
    let spatial = ownership
        .as_ref()
        .is_some_and(|ownership| spatially_attributed(&menu, ownership));
    let confirmed = menu_confirms(&menu, spatial);
    let mut text = message.to_owned();
    if confirmed {
        text.push_str(" Context menu opened");
        if !menu.items.is_empty() {
            text.push_str(&format!(": {}", menu.items.join(", ")));
        }
        text.push('.');
        if menu.completion_indeterminate {
            text.push_str(" Native completion is still running.");
        }
    }
    if confirmed {
        if let Some(ownership) = ownership {
            if menu.unique_ax_menu
                && menu.unique_window
                && menu.continuous
                && spatially_attributed(&menu, &ownership)
            {
                if let Some(native_menu) = menu.menu.as_ref() {
                    let linked = unsafe { ax_get_window_id(native_menu.0 as AXUIElementRef) }
                        == menu.window_id;
                    if linked {
                        unsafe { core_foundation::base::CFRetain(native_menu.0 as CFTypeRef) };
                        match crate::ax::exact_target::register_context_menu(
                            &ownership.session,
                            ownership.generation,
                            ownership.pid,
                            ownership.window_id,
                            ownership.target_element,
                            native_menu.0,
                        ) {
                            Some(opener_id) => text.push_str(&format!(" opener_id:{opener_id}")),
                            None => unsafe { CFRelease(native_menu.0 as CFTypeRef) },
                        }
                    }
                }
            }
        }
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
pub(super) enum RightClickAxStage {
    Completed(ElementRightClick),
    PixelFallback,
}

/// Decide whether the semantic actuator completed or a separate pixel stage
/// is permitted. The pixel operation itself is deliberately outside this
/// stage so a caller can perform asynchronous checks between the actuators.
pub(super) fn right_click_ax_stage<T: Copy>(
    target: T,
    label: &str,
    advertises_show_menu: bool,
    show_menu: impl FnOnce(T) -> crate::ax::bindings::AXError,
    ax_delivered: impl FnOnce(),
) -> RightClickAxStage {
    if advertises_show_menu {
        let err = show_menu(target);
        if err == kAXErrorSuccess {
            ax_delivered();
            return RightClickAxStage::Completed(ElementRightClick::Ax(format!(
                "Shown menu for {label} (AXShowMenu)."
            )));
        }
        if !super::click::perform_error_proves_nothing_was_sent(err) {
            ax_delivered();
            return RightClickAxStage::Completed(ElementRightClick::Ax(format!(
                "⚠️ Sent AXShowMenu to {label}, but the app returned AX error {err}, which does \
                 not mean it failed: the menu may be open. Read the window before repeating it."
            )));
        }
        tracing::debug!("AXShowMenu returned {err} for {label}; falling back to pixel right-click");
    }
    RightClickAxStage::PixelFallback
}

pub(super) fn right_click_element<T: Copy>(
    target: T,
    label: &str,
    advertises_show_menu: bool,
    show_menu: impl FnOnce(T) -> crate::ax::bindings::AXError,
    ax_delivered: impl FnOnce(),
    pixel: impl FnOnce(T) -> anyhow::Result<String>,
) -> anyhow::Result<ElementRightClick> {
    match right_click_ax_stage(target, label, advertises_show_menu, show_menu, ax_delivered) {
        RightClickAxStage::Completed(delivery) => Ok(delivery),
        RightClickAxStage::PixelFallback => pixel(target).map(ElementRightClick::Pixel),
    }
}

fn ax_show_menu(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    wid: u32,
    advertises_show_menu: bool,
    dispatch: std::sync::mpsc::Sender<ActionEvent>,
) -> anyhow::Result<ElementRightClick> {
    let element = element_ptr as AXUIElementRef;

    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    let title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();

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
        element,
        &format!("[{idx}] {role} \"{title}\""),
        advertises_show_menu,
        |target| {
            let _ = ax_dispatch.send(ActionEvent::Dispatching(Instant::now()));
            unsafe { perform_action(target, "AXShowMenu") }
        },
        || {
            let _ = ax_dispatch.send(ActionEvent::Sent(Instant::now()));
        },
        |target| {
            let _ = dispatch.send(ActionEvent::Dispatching(Instant::now()));
            pixel_right_click(target, idx, &role, &title, pid, wid)
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
        let attempts = std::cell::Cell::new(0);
        let stage = right_click_ax_stage(
            73_u32,
            "[4]",
            true,
            |target| {
                assert_eq!(target, 73);
                attempts.set(attempts.get() + 1);
                kAXErrorIllegalArgument
            },
            || panic!("proven-unsent AX action cannot report delivery"),
        );
        assert!(matches!(stage, RightClickAxStage::PixelFallback));
        assert_eq!(attempts.get(), 1);
        let delivered = std::cell::Cell::new(0);
        let stage = right_click_ax_stage(
            73_u32,
            "[4]",
            true,
            |target| {
                assert_eq!(target, 73);
                kAXErrorSuccess
            },
            || delivered.set(delivered.get() + 1),
        );
        assert!(matches!(
            stage,
            RightClickAxStage::Completed(ElementRightClick::Ax(_))
        ));
        assert_eq!(delivered.get(), 1);
        for err in [kAXErrorCannotComplete, kAXErrorActionUnsupported] {
            let delivered = std::cell::Cell::new(0);
            let outcome = right_click_element(
                73_u32,
                "[4] AXRow \"a\"",
                true,
                |target| {
                    assert_eq!(target, 73);
                    err
                },
                || delivered.set(delivered.get() + 1),
                |_| panic!("pixel right-click on top of a menu that may be open"),
            )
            .expect("reported, not failed");
            assert_eq!(delivered.get(), 1);
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
                73_u32,
                "[4]",
                advertised,
                |target| {
                    assert_eq!(target, 73);
                    err
                },
                || ax_delivered.set(ax_delivered.get() + 1),
                |target| {
                    assert_eq!(target, 73);
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
                    linked: true,
                    input_quiet: true,
                    ..MenuObservation::none()
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
                linked: true,
                input_quiet: true,
                ..MenuObservation::none()
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
            linked: 0,
        };
        let reexposed = MenuSnapshot {
            window_ids: Some(HashSet::from([7])),
            ax_menus: None,
            linked: 0,
        };
        assert!(new_menu_observation(&before, &reexposed).is_none());
        let new_window = MenuSnapshot {
            window_ids: Some(HashSet::from([7, 8])),
            ax_menus: None,
            linked: 0,
        };
        assert_eq!(
            new_menu_observation(&before, &new_window).unwrap().evidence,
            MenuEvidence::Window
        );
        let incomplete = MenuSnapshot {
            window_ids: None,
            ax_menus: None,
            linked: 0,
        };
        assert!(new_menu_observation(&incomplete, &new_window).is_none());

        let crossed_after = MenuSnapshot {
            window_ids: Some(HashSet::from([7, 8])),
            ax_menus: None,
            linked: 0,
        };
        let other_baseline = MenuSnapshot {
            window_ids: Some(HashSet::from([8])),
            ax_menus: None,
            linked: 0,
        };
        assert!(
            new_menu_observation_between(&before, &other_baseline, &crossed_after, true).is_none()
        );
        let common_new = MenuSnapshot {
            window_ids: Some(HashSet::from([7, 8, 9])),
            ax_menus: None,
            linked: 0,
        };
        assert!(
            new_menu_observation_between(&before, &other_baseline, &common_new, true).is_some()
        );
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
    /// Observation clock whose time moves only when the observer sleeps or a
    /// test advances it. Messages are awaited without a wall-clock timeout, so
    /// thread scheduling cannot move a deadline.
    #[derive(Clone)]
    struct FakeClock {
        now: Arc<std::sync::Mutex<Instant>>,
        sleeps: Arc<std::sync::Mutex<Vec<Duration>>>,
        waits: Arc<std::sync::atomic::AtomicUsize>,
        dropped_at_result_wait: Arc<std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Arc::new(std::sync::Mutex::new(Instant::now())),
                sleeps: Arc::new(std::sync::Mutex::new(Vec::new())),
                waits: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                dropped_at_result_wait: Arc::new(std::sync::Mutex::new(None)),
            }
        }

        fn advance(&self, duration: Duration) {
            *self.now.lock().unwrap() += duration;
        }

        fn sleeps(&self) -> Vec<Duration> {
            self.sleeps.lock().unwrap().clone()
        }
    }

    impl ObservationClock for FakeClock {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }

        fn sleep(&self, duration: Duration) {
            self.advance(duration);
            self.sleeps.lock().unwrap().push(duration);
        }

        fn recv_timeout<M>(
            &self,
            receiver: &std::sync::mpsc::Receiver<M>,
            _timeout: Duration,
        ) -> Result<M, std::sync::mpsc::RecvTimeoutError> {
            // The observer waits first for the dispatch event and then, after
            // polling, for the action result. Dropping the parked sender at
            // the result wait unblocks an action the observer never released,
            // so a fixture regression fails instead of deadlocking.
            if self.waits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1 {
                drop(self.dropped_at_result_wait.lock().unwrap().take());
            }
            receiver
                .recv()
                .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected)
        }
    }

    /// The action reports Sent, waits for the sent baseline read, adds menu
    /// window 2, and then stays blocked until the observer's second sighting
    /// releases it. The action returns whether that release came from the
    /// observer. With `stall_on_second_sighting`, the second sighting read is
    /// held up for 151ms, so it completes after the 150ms deadline although
    /// the first sighting completed inside the window.
    fn observe_while_action_is_blocked(
        stall_on_second_sighting: bool,
    ) -> (ObservedAction<bool>, MenuObservation, Vec<Duration>) {
        let clock = FakeClock::new();
        let (baseline_tx, baseline_rx) = std::sync::mpsc::channel();
        let (inserted_tx, inserted_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *clock.dropped_at_result_wait.lock().unwrap() = Some(release_tx);
        let before = MenuSnapshot {
            window_ids: Some(HashSet::new()),
            ax_menus: None,
            linked: 0,
        };
        let menu_ids = Arc::new(std::sync::Mutex::new(HashSet::from([1])));
        let action_ids = menu_ids.clone();
        let action_clock = clock.clone();
        let read_clock = clock.clone();
        let mut reads = 0;
        let (result, observation) = observe_right_click_action_with(
            &clock,
            before,
            move |sent| {
                let _ = sent.send(ActionEvent::Sent(action_clock.now()));
                baseline_rx.recv().unwrap();
                action_ids.lock().unwrap().insert(2);
                inserted_tx.send(()).unwrap();
                release_rx.recv().is_ok()
            },
            move || {
                reads += 1;
                if reads == 3 && stall_on_second_sighting {
                    read_clock.advance(MENU_OBSERVATION + Duration::from_millis(1));
                }
                let snapshot = MenuSnapshot {
                    window_ids: Some(menu_ids.lock().unwrap().clone()),
                    ax_menus: None,
                    linked: 0,
                };
                match reads {
                    1 => {
                        baseline_tx.send(()).unwrap();
                        inserted_rx.recv().unwrap();
                    }
                    3 => {
                        let release = read_clock.dropped_at_result_wait.lock().unwrap().take();
                        release.unwrap().send(()).unwrap();
                    }
                    _ => {}
                }
                snapshot
            },
        );
        (result, observation, clock.sleeps())
    }

    #[test]
    fn wave400c_observation_runs_while_the_action_is_blocked() {
        let (result, observation, sleeps) = observe_while_action_is_blocked(false);
        assert!(
            result.unwrap(),
            "the action must stay blocked until the second sighting"
        );
        assert!(observation.opened);
        assert!(observation.continuous);
        assert_eq!(observation.evidence, MenuEvidence::Window);
        assert_eq!(observation.window_id, Some(2));
        assert_eq!(sleeps, vec![Duration::from_millis(15)]);
    }

    #[test]
    fn wave400c_deadline_passing_before_the_second_sighting_does_not_confirm() {
        // The ordering a loaded runner produced in tag run 40895615773: the
        // first sighting lands inside the 150ms window, but the deadline
        // passes before the second consistent sighting completes.
        let (result, observation, sleeps) = observe_while_action_is_blocked(true);
        assert!(result.unwrap());
        assert!(!observation.opened);
        assert_eq!(sleeps, vec![Duration::from_millis(15)]);
    }

    #[test]
    fn wave400c_menu_seen_before_the_actual_dispatch_does_not_confirm() {
        let before = MenuSnapshot {
            window_ids: Some(HashSet::new()),
            ax_menus: None,
            linked: 0,
        };
        let menu_ids = Arc::new(std::sync::Mutex::new(HashSet::from([1])));
        let action_ids = menu_ids.clone();
        let clock = FakeClock::new();
        let action_clock = clock.clone();
        let (result, observation) = observe_right_click_action_with(
            &clock,
            before,
            move |sent| {
                action_ids.lock().unwrap().insert(2);
                let _ = sent.send(ActionEvent::Sent(action_clock.now()));
            },
            || MenuSnapshot {
                window_ids: Some(menu_ids.lock().unwrap().clone()),
                ax_menus: None,
                linked: 0,
            },
        );
        result.unwrap();
        assert!(!observation.opened);
        let sleeps = clock.sleeps();
        assert!(
            sleeps
                .iter()
                .all(|sleep| *sleep <= Duration::from_millis(15)),
            "{sleeps:?}"
        );
        assert_eq!(sleeps.iter().sum::<Duration>(), MENU_OBSERVATION);
    }

    #[test]
    fn wave400f2_prompt_window_is_not_lost_to_the_sent_baseline() {
        let before = MenuSnapshot {
            window_ids: Some(HashSet::new()),
            ax_menus: None,
            linked: 0,
        };
        let sent = MenuSnapshot {
            window_ids: Some(HashSet::from([101])),
            ax_menus: None,
            linked: 0,
        };
        let after = MenuSnapshot {
            window_ids: Some(HashSet::from([101])),
            ax_menus: None,
            linked: 0,
        };
        let (window_id, unique) = novel_window_between(&before, &sent, &after, false);
        assert!(unique);
        assert_eq!(window_id, Some(101));
        let observation = new_menu_observation_between(&before, &sent, &after, false)
            .expect("window-only prompt evidence is retained");
        assert!(observation.opened);
        assert_eq!(observation.window_id, Some(101));
    }

    #[test]
    fn wave400c_proven_unsent_fallback_rearms_and_observes_the_pixel_dispatch() {
        use crate::ax::bindings::kAXErrorIllegalArgument;
        let (sent_tx, sent_rx) = std::sync::mpsc::channel();
        let ax_tx = sent_tx.clone();
        let outcome = right_click_element(
            73_u32,
            "[4]",
            true,
            |target| {
                assert_eq!(target, 73);
                kAXErrorIllegalArgument
            },
            move || {
                let _ = ax_tx.send("ax");
            },
            move |target| {
                assert_eq!(target, 73);
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
            linked: 0,
        };
        let clock = FakeClock::new();
        let action_clock = clock.clone();
        let mut reads = 0;
        let (result, observation) = observe_right_click_action_with(
            &clock,
            before,
            move |sent| {
                let _ = sent.send(ActionEvent::Sent(action_clock.now()));
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
                    linked: 0,
                }
            },
        );
        result.unwrap();
        assert!(observation.opened);
        assert_eq!(clock.sleeps(), vec![Duration::from_millis(15)]);
    }

    #[test]
    fn a_menu_that_shows_after_the_old_150ms_bound_is_still_observed() {
        // Finder's list-view menu window appeared about 130 ms after the call
        // started, past the old bound once the reads were counted.
        let before = MenuSnapshot {
            window_ids: Some(HashSet::new()),
            ax_menus: None,
            linked: 0,
        };
        let clock = FakeClock::new();
        let start = clock.now();
        let action_clock = clock.clone();
        let read_clock = clock.clone();
        let (result, observation) = observe_right_click_action_with(
            &clock,
            before,
            move |sent| {
                let _ = sent.send(ActionEvent::Sent(action_clock.now()));
            },
            move || MenuSnapshot {
                window_ids: Some(if read_clock.now() - start >= Duration::from_millis(250) {
                    HashSet::from([1])
                } else {
                    HashSet::new()
                }),
                ax_menus: None,
                linked: 0,
            },
        );
        result.unwrap();
        assert!(observation.opened);
        assert!(observation.continuous);
        let waited: Duration = clock.sleeps().iter().sum();
        assert!(
            waited < MENU_OBSERVATION,
            "returns at the second sighting: {waited:?}"
        );
    }

    #[test]
    fn only_a_menu_tied_to_the_click_with_no_physical_input_confirms() {
        let seen = |evidence, linked, input_quiet| MenuObservation {
            opened: true,
            evidence,
            linked,
            input_quiet,
            ..MenuObservation::none()
        };
        // Hangs off the clicked element or an ancestor.
        assert!(menu_confirms(
            &seen(MenuEvidence::Accessibility, true, true),
            false
        ));
        // A menu window that opened at the click point.
        assert!(menu_confirms(
            &seen(MenuEvidence::Window, false, true),
            true
        ));
        // Any other new menu of the app: seen, not attributed.
        assert!(!menu_confirms(
            &seen(MenuEvidence::Accessibility, false, true),
            false
        ));
        assert!(!menu_confirms(
            &seen(MenuEvidence::Window, false, true),
            false
        ));
        // A person typed or clicked meanwhile, or the idle clock was unreadable.
        assert!(!menu_confirms(
            &seen(MenuEvidence::Accessibility, true, false),
            true
        ));
        assert!(!menu_confirms(&MenuObservation::none(), true));

        let result = right_click_result(
            "menu delivered",
            "cgevent",
            seen(MenuEvidence::Accessibility, false, true),
            false,
        );
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["effect"], "unverifiable", "{structured}");
    }

    #[test]
    fn a_key_press_counts_unless_another_app_was_in_front_throughout() {
        // No press of any kind.
        assert!(menu_input_attributable(true, true, || false));
        // Typing in another app that stayed frontmost.
        assert!(menu_input_attributable(true, false, || true));
        // Typing while the target app could have received it (a keyboard
        // shortcut can open a contextual menu).
        assert!(!menu_input_attributable(true, false, || false));
        // A mouse press anywhere: the person may have right-clicked.
        assert!(!menu_input_attributable(false, true, || true));
    }

    #[test]
    fn an_ax_menu_does_not_borrow_an_unrelated_menu_window_position() {
        let window = [2002.0, 198.0, 391.0, 552.0];
        assert!(spatial_evidence_is_one_menu(
            MenuEvidence::Window,
            None,
            window
        ));
        assert!(spatial_evidence_is_one_menu(
            MenuEvidence::Accessibility,
            Some([2003.0, 199.0, 391.0, 552.0]),
            window
        ));
        // A different menu opened elsewhere at the same time.
        assert!(!spatial_evidence_is_one_menu(
            MenuEvidence::Accessibility,
            Some([300.0, 400.0, 200.0, 120.0]),
            window
        ));
        assert!(!spatial_evidence_is_one_menu(
            MenuEvidence::Accessibility,
            None,
            window
        ));
        assert!(!spatial_evidence_is_one_menu(
            MenuEvidence::None,
            None,
            window
        ));
    }
}
