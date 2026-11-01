//! type_text tool — matches the Swift reference TypeTextTool.swift.
//!
//! Inserts text via `AXSelectedText` attribute write — an atomic single-call
//! insertion at the current cursor position. This is the preferred path for
//! all standard Cocoa text views (NSTextField, NSTextView, WKWebView text
//! inputs in Safari, etc.) and is significantly faster than per-keystroke
//! CGEvent synthesis.
//!
//! For Chromium / Electron inputs that don't implement `kAXSelectedText`,
//! the tool falls back to character-by-character CGEvent keystrokes so the
//! caller doesn't need to detect the app type themselves.
//!
//! When the target pid belongs to a terminal emulator (Ghostty,
//! Terminal.app, iTerm2, Alacritty, kitty, WezTerm, Hyper, Warp — see
//! [`crate::terminal::TERMINAL_BUNDLE_IDS`]), the AX path is skipped
//! entirely: terminals expose `AXTextArea` for their grid but the
//! `AXSelectedText` write never reaches the pty, so the tool would
//! report success while the shell sees nothing. We go straight to
//! CGEvent key-event synthesis (`path: "key_events"`).
//!
//! Use `type_text_chars` when you explicitly need per-character pacing
//! (e.g., to trigger live-search debounce handlers).

use crate::goat::action_latency as latency;

use async_trait::async_trait;
use cua_driver_contract::TypeTextInput;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_string_attr, focused_element_of_pid, kAXErrorSuccess, set_string_attr, AXUIElementRef,
};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::CFRelease;
use cua_driver_core::background_input::BackgroundRefusal;

use super::ToolState;

pub struct TypeTextTool {
    pub state: Arc<ToolState>,
}

impl TypeTextTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "type_text".into(),
        description: "Insert text via `AXSetAttribute(kAXSelectedText)` into the `element_token` element or the pid's \
            focused element. Uses AX insertion where trusted and PID key events otherwise; use `press_key` / `hotkey` for standalone Return, Escape and arrows. \
            Where AX insertion is unsupported (Chromium/Electron) it falls back to CGEvent typing when the \
            route fits the transport budget; longer text is refused with a safe chunk size. In web content \
            (browser tabs, Slack, VS Code) AXValue does not prove the DOM saw the text, so the result is \
            `effect:\"unverifiable\"`; exact web targets use pointer focus and PID key delivery. For a browser \
            tab use the `page` tool; for an embedded web view pass `x, y` (no element_token) to pixel-click \
            the field and type in one call (AX-press a closed control open first).".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["text"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it." },
                "pid":  { "type": "integer", "description": "Target process ID." },
                "text": { "type": "string",  "description": "Text to insert." },
                "replace": { "type": "boolean", "default": false, "description": "Replace the exact editable field's entire value after verified full selection and focus. Window/element targets only; control characters are refused." },
                "window_id": {
                    "type": "integer",
                    "description": "Window ID. Omit with element_token."
                },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "x": { "type": "number", "description": "Screenshot-pixel X of the field (get_window_state PNG, as in click). With y and no element_token, pixel-clicks to focus, then types." },
                "y": { "type": "number", "description": "Screenshot-pixel Y (see x)." },
                "delay_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 200,
                    "description": "Ms between characters in the CGEvent fallback. Default 30."
                },
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "\"desktop\" with no pid/window_id types into the frontmost app." },
                "delivery_mode": {
                    "type": "string",
                    "enum": ["background", "foreground"],
                    "description": "Default \"background\": AX insert, then CGEvent keys, no focus steal. \"foreground\": front the window, type, restore; last resort for surfaces (WhatsApp/Catalyst) where background keys do not land."
                }
            },
            "additionalProperties": false
        }),
        read_only:   false,
        destructive: true,
        idempotent:  false,
        open_world:  true,
    })
}

fn screen_sharing_delivery_error(
    is_screen_sharing: bool,
    foreground: bool,
    window_id: Option<u32>,
) -> Option<ToolResult> {
    if !is_screen_sharing || (foreground && window_id.is_some()) {
        return None;
    }
    Some(
        ToolResult::error(
            "Screen Sharing text input requires delivery_mode:\"foreground\" and window_id \
             so Cua Driver can deliver physical HID key transitions safely.",
        )
        .with_structured(serde_json::json!({
            "code": "SCREEN_SHARING_REQUIRES_FOREGROUND_HID",
            "effect": "refused",
            "escalation": {
                "recommended": "foreground",
                "reason": "Screen Sharing forwards physical keycodes; background PID-routed \
                           Unicode events can corrupt guest text.",
                "requires": ["window_id"]
            }
        })),
    )
}

#[async_trait]
impl Tool for TypeTextTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let replace = args.bool_or("replace", false);
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            if replace {
                return super::invalid_arguments(
                    "type_text replace:true requires an exact window/element target, not desktop scope.",
                );
            }
            let input = match parse_typed_projection::<TypeTextInput>("type_text", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let text =
                cua_driver_core::text_sanitize::strip_trailing_agent_protocol_tags(&input.text)
                    .into_owned();
            let delay_ms = args.u64_or("delay_ms", 30).min(200);
            if let Some(refusal) = synthesis_preflight(
                TextDeliveryRoute::UnicodeSynthesis,
                text.chars().count(),
                delay_ms,
            ) {
                return synthesis_refusal_result("hid", &refusal, AxAttempt::NotAttempted);
            }
            // Desktop scope dispatches globally: check the frontmost app's
            // focused element and fail closed before any key is sent.
            let early_front_pid = crate::apps::frontmost_pid();
            let initial_focus = early_front_pid
                .and_then(|pid| unsafe { crate::ax::bindings::focused_element_of_pid(pid) })
                .map(|ptr| unsafe {
                    // focused_element_of_pid already returned +1: adopt it
                    // into the wrapper and balance with one CFRelease, so
                    // exactly one release happens on drop.
                    let owned = crate::ax::snapshot::RetainedElement::retain(ptr as usize);
                    core_foundation::base::CFRelease(ptr as core_foundation::base::CFTypeRef);
                    owned
                });
            match early_front_pid {
                Some(front_pid) => {
                    if let Some(refusal) =
                        super::not_editable::guard_untargeted(front_pid, None).await
                    {
                        return refusal;
                    }
                }
                None => {
                    return super::not_editable::not_editable_result(
                        "the frontmost application could not be determined, so editability is                          unknown; nothing was sent."
                            .into(),
                        super::not_editable::FocusDispatch::None,
                    )
                }
            }
            let checked_pid = early_front_pid.unwrap_or_default();
            let posted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let posted_ref = posted.clone();

            let result = latency::spawn_action_blocking(move || {
                super::not_editable::desktop_dispatch_guard(checked_pid, true)?;
                crate::input::keyboard::type_text_global_with_guard(
                    &text,
                    delay_ms,
                    || {
                        // The focused-element read can be slow: the pid is
                        // checked again AFTER it, so a switch during that read
                        // cannot redirect the next character.
                        crate::apps::frontmost_pid() == Some(checked_pid)
                            && unsafe {
                                super::not_editable::sync_focused_cell_allows(checked_pid, None)
                            }
                            && crate::apps::frontmost_pid() == Some(checked_pid)
                            && initial_focus
                                .as_ref()
                                .map(|el| {
                                    crate::input::ax_actions::is_element_focused(
                                        checked_pid,
                                        el.as_ptr(),
                                    )
                                })
                                .unwrap_or(false)
                            && crate::apps::frontmost_pid() == Some(checked_pid)
                    },
                    posted_ref.as_ref(),
                )
            })
            .await;
            if let Ok(Err(error)) = &result {
                if let Some(abort) =
                    error.downcast_ref::<crate::input::keyboard::PasteSequenceAbort>()
                {
                    let restored = match abort.clipboard_restored {
                        crate::input::keyboard::PasteRestoration::Unknown => "unknown",
                        crate::input::keyboard::PasteRestoration::Restored => "restored",
                        crate::input::keyboard::PasteRestoration::SkippedConcurrentChange => {
                            "restoration skipped (concurrent change)"
                        }
                        crate::input::keyboard::PasteRestoration::Failed => "restoration failed",
                    };
                    let count = abort.posted;
                    return ToolResult::text(format!(
                        "Desktop typing stopped before the paste went out: {}. At least {}                          character(s) were sent earlier (clipboard_restored={}). Read fresh                          state before retrying.",
                        abort.reason, count, restored
                    ))
                    .with_structured(serde_json::json!({
                        "code": "not_editable",
                        "effect": "unverifiable",
                        "clipboard_restored": restored,
                    }));
                }
            }
            if result.is_err() || matches!(result, Ok(Err(_))) {
                let count = posted.load(std::sync::atomic::Ordering::SeqCst);
                if count > 0 {
                    // Every failure after some characters were posted keeps
                    // that count, including event-construction and task
                    // failures.
                    return ToolResult::text(format!(
                        "Desktop typing stopped after at least {count} character(s) were sent.                          Any event or paste in flight is unverifiable; read fresh state before                          retrying."
                    ))
                    .with_structured(serde_json::json!({
                        "code": "not_editable",
                        "effect": "partial",
                        "dispatched": true,
                        "delivered_count": count,
                    }))
                    .with_action_record(
                        cua_driver_core::action_record::ActionExecutionRecord::builder(
                            cua_driver_core::action_record::ActionEffect::Partial,
                            cua_driver_core::action_record::ActionTransport::MacosCgEventHid,
                            cua_driver_core::action_record::RequestedDelivery::Foreground,
                        )
                        .actual_delivery(cua_driver_core::action_record::ActualDelivery::Foreground)
                        .delivered_count(count as u32)
                        .build()
                        .expect("desktop partial record is valid"),
                    );
                }
            }
            return match result {
                Ok(Ok(_)) => ToolResult::text("Typed text into the frontmost desktop application.")
                    .with_structured(serde_json::json!({
                        "scope": "desktop",
                        "path": "hid",
                        "effect": "unverifiable"
                    })),
                Ok(Err(error)) => super::not_editable::account_focus_receipt(
                    text_delivery_failure("desktop type_text failed", &error),
                    None,
                ),
                Err(error) => ToolResult::text(format!(
                    "The desktop typing task failed ({error}); it may have posted input before                      failing. Read fresh state before retrying."
                ))
                .with_structured(serde_json::json!({
                    "code": "desktop_sequence_unverified",
                    "effect": "unverifiable",
                })),
            };
        }
        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let text_raw = match args.require_str("text") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Strip trailing agent-protocol closing tags — see
        // cua_driver_core::text_sanitize docs for rationale.
        let text = cua_driver_core::text_sanitize::strip_trailing_agent_protocol_tags(&text_raw)
            .into_owned();
        if replace && text.contains(['\n', '\r', '\t']) {
            return super::invalid_arguments(
                "type_text replace:true refuses newline, carriage return and tab; use replace:false for submission or multiline insertion.",
            );
        }
        if replace && crate::terminal::is_terminal_pid(pid) {
            return super::invalid_arguments(
                "type_text replace:true is not supported for terminal surfaces.",
            );
        }
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
        if replace && window_id.is_none() {
            return super::invalid_arguments(
                "type_text replace:true requires window_id and an exact editable element.",
            );
        }
        // A terminal reads keys from its pty: all 13 characters of a test
        // string arrived and read back with no extra delay (Terminal.app),
        // and the 30 ms default cost 0.4 s of a 0.8 s insert. The 8 ms key
        // gap still separates every key.
        let delay_ms = args.opt_u64("delay_ms").unwrap_or_else(|| {
            if crate::terminal::is_terminal_pid(pid) {
                0
            } else {
                30
            }
        });
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        // The person is working in this app: nothing is sent.
        if !delivery_mode.is_foreground() {
            if let Err(refusal) = super::gate_user_takeover(pid, window_id) {
                return refusal;
            }
        }
        if replace && crate::input::keyboard::is_screen_sharing_pid(pid) {
            return super::invalid_arguments(
                "type_text replace:true is not supported for Screen Sharing because whole-field replacement cannot be verified.",
            );
        }
        if let Some(error) = screen_sharing_delivery_error(
            crate::input::keyboard::is_screen_sharing_pid(pid),
            delivery_mode.is_foreground(),
            window_id,
        ) {
            return error;
        }

        // Argument-shape errors are reported before any gating or retained
        // lookups: a malformed call must fail the same way regardless of
        // background-target state.
        let px = args.get("x").and_then(|v| v.as_f64());
        let py = args.get("y").and_then(|v| v.as_f64());
        if px.is_some() && py.is_some() && element_index.is_some() {
            return super::invalid_arguments(
                "Pass either element_token (ax) or x,y (px) to type_text, not both.",
            );
        }

        let element_guard = element_guard.zip(element_index);

        // ── Exact-target background gate (macOS background input v1) ──
        // A window-addressed background insert must prove exact delivery
        // before any input — including the px focus click — is sent. The pure
        // core decides once from fresh facts: full keyboard ladder, semantic
        // AX write only (exact element, no CGEvent fallback), or a structured
        // refusal. delivery_mode:"foreground" stays the caller's explicit
        // last resort and is not gated here.
        let (mut _mutation_lease, keyboard_policy) = match window_id {
            Some(wid) if !delivery_mode.is_foreground() => {
                let gate_element_ptr = element_guard.as_ref().map(|(g, _)| g.as_ptr());
                match background_keyboard_policy(pid, wid, gate_element_ptr).await {
                    Ok((lease, policy)) => (Some(lease), policy),
                    Err(refusal_result) => return refusal_result,
                }
            }
            _ => (None, BackgroundKeyboardPolicy::Allowed),
        };

        // The exact-window keyboard route must refuse before the TOKEN guard
        // can write AX focus, even when focus-record SPIs are unavailable.
        if window_id.is_some()
            && !delivery_mode.is_foreground()
            && matches!(keyboard_policy, BackgroundKeyboardPolicy::Allowed)
        {
            if let Err(refusal) = super::key_post_preflight(crate::input::skylight::is_available) {
                return refusal;
            }
        }

        // CELL rule for untargeted typing: a non-editing cell of a
        // row/outline/table (or an unreadable focused element) refuses the
        // call before any key.
        //
        // A bound window that is not its app's key window has no focused
        // element to judge yet: hold it as key (whichever app is in front)
        // and insert there in one detached worker (type_text_hold).
        if let Some(wid) = window_id {
            let untargeted = element_guard.is_none() && px.is_none() && !replace;
            let background_allowed = !delivery_mode.is_foreground()
                && matches!(keyboard_policy, BackgroundKeyboardPolicy::Allowed);
            let spi = crate::input::skylight::is_focus_without_raise_available();
            // Terminals take text from their pty (an AX write never reaches
            // it) and Chromium native fields have their own submission
            // route: both keep the ordinary path.
            let ax_insert_route = super::type_text_hold::ax_insert_route(
                crate::terminal::is_terminal_pid(pid),
                &apps::bundle_id_for_pid(pid).unwrap_or_default(),
            );
            if untargeted && background_allowed && spi && ax_insert_route {
                let routes = latency::spawn_action_blocking(move || {
                    crate::input::skylight::window_routes_keys(pid as libc::pid_t, wid)
                })
                .await
                .unwrap_or(true);
                if super::type_text_hold::hold_needed(untargeted, background_allowed, spi, routes) {
                    super::cursor_tools::position_keyboard_cursor(
                        &self.state,
                        &args,
                        window_id,
                        None,
                    )
                    .await;
                    return super::type_text_hold::held_window_insert(
                        _mutation_lease.take(),
                        pid,
                        wid,
                        text.clone(),
                    )
                    .await;
                }
            }
        }
        if element_guard.is_none() && px.is_none() && !replace {
            if let Some(refusal) = super::not_editable::guard_untargeted(pid, window_id).await {
                return refusal;
            }
        }

        let element_ptr = element_guard
            .as_ref()
            .map(|(g, idx)| (g.as_ptr(), Some(*idx)));
        let used_pixel_focus = px.is_some() && py.is_some();
        let mut px_receipt = None;

        let chromium_native_candidate = !delivery_mode.is_foreground()
            && !used_pixel_focus
            && is_chromium_identity(&apps::bundle_id_for_pid(pid).unwrap_or_default());
        // TOKEN rule: after the gate, attempt focus and refuse a text-role
        // element that shows no editing evidence (handoff 3.7).
        let token_receipt = if let (Some((guard, _)), Some(wid)) = (&element_guard, window_id) {
            match super::not_editable::token_not_editable_receipt(pid, guard.as_ptr(), wid).await {
                Ok(receipt) => Some(receipt),
                Err(refusal) => return refusal,
            }
        } else {
            None
        };

        // ── px form: focus by pixel-click, then type into the focused element ──
        // Pass x,y (no element_token) for an *element px action*: pixel-click the
        // field to give the Chromium/Electron renderer the real keyboard focus the
        // AX path can't, then fall through to the focused-element type path (which
        // escalates AX → CGEvent and lands once focused). Reuses ClickTool's exact
        // coordinate translation + delivery_mode, so it lands on the same pixel a
        // px-click would.
        // A background px insert makes the window key before the focus click
        // and keeps it so through delivery, as press_key and hotkey do: the
        // focus read-back then reads this window's focused element, and the
        // keys reach the field it confirmed. The delivery's own key scope
        // finds the window already key.
        let _px_key_scope = if used_pixel_focus
            && !delivery_mode.is_foreground()
            && matches!(keyboard_policy, BackgroundKeyboardPolicy::Allowed)
        {
            match super::acquire_key_scope(pid, window_id, false).await {
                Ok(scope) => scope,
                Err(refusal) => return refusal,
            }
        } else {
            None
        };
        if let (Some(cx), Some(cy)) = (px, py) {
            // The px form has no exact element for a semantic-only write; when
            // the keyboard rung is refused, refuse before the focus click too.
            if let BackgroundKeyboardPolicy::SemanticOnly(ref refusal) = keyboard_policy {
                let wid = window_id.expect("gate ran only with window_id");
                return super::background_refusal_result(pid, wid, refusal);
            }
            let from_zoom = args
                .get("from_zoom")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let focus_receipt = match super::focus_by_pixel(
                &self.state,
                pid,
                window_id,
                cx,
                cy,
                delivery_mode.is_foreground(),
                args.opt_str("session"),
                args.opt_str("_session_id"),
                from_zoom,
                _mutation_lease.as_ref(),
                true,
            )
            .await
            {
                Ok(receipt) => receipt,
                Err(e) => return e,
            };
            px_receipt = Some(focus_receipt);
            // Verify the focus the click produced before typing: if it
            // landed on a non-editing cell, no text is sent and the result
            // reports the focus action truthfully.
            if !super::not_editable::focused_cell_allows_typing(pid, window_id).await {
                return super::not_editable::not_editable_result(
                    "the element focused by the click is not being edited; no text was sent."
                        .into(),
                    focus_receipt,
                );
            }
            // element_index stays None → the type path below writes to the now-
            // focused element via the CGEvent (key_events) rung.
        }

        if let (Some((element, _)), Some(wid)) = (element_guard.as_ref(), window_id) {
            let center_guard = element.clone();
            if let Ok((Some((screen_x, screen_y)), target_rect)) =
                latency::spawn_action_blocking(move || unsafe {
                    let el = center_guard.as_ptr() as AXUIElementRef;
                    (
                        crate::ax::bindings::element_screen_center(el),
                        crate::ax::bindings::element_screen_rect(el),
                    )
                })
                .await
            {
                let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
                crate::cursor::overlay::animate_cursor_to_target(
                    cursor_key.clone(),
                    screen_x,
                    screen_y,
                    target_rect,
                )
                .await;
                self.state
                    .cursor_registry
                    .update_position(&cursor_key, screen_x, screen_y);
            }
        }
        // Untargeted text still gives a named session visible feedback: its
        // remembered position, or the window centre on the first action.
        if element_guard.is_none() && !used_pixel_focus {
            super::cursor_tools::position_keyboard_cursor(&self.state, &args, window_id, None)
                .await;
        }
        let text_clone = text.clone();
        let char_count = text.chars().count();
        let mut prior = prior_for_receipt(token_receipt);
        prior = combine_prior(prior, prior_for_receipt(px_receipt));
        if let Some(scope) = &_px_key_scope {
            prior = prior_for_key_entry(prior, scope.entry());
        }

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // Typing into a field can trigger autocomplete popovers or
        // Chrome/Safari's "Save Password?" prompt, both of which open
        // helper windows. Wrap so callers see them in the result suffix
        // and the wildcard suppressor catches reflex activations.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);

        // Terminal-emulator short-circuit: when the target pid belongs
        // to a known terminal (Ghostty / Terminal.app / iTerm2 / …), the
        // AX value-set is silently dropped — see crate::terminal docs.
        // Skip the AX path entirely so the caller never sees the
        // "success but nothing typed" symptom.
        let is_terminal_target = crate::terminal::is_terminal_pid(pid);

        let blocking_policy = keyboard_policy.clone();
        let native_guard = element_guard.clone();
        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "type_text.AXSelectedText",
            || async move {
                latency::spawn_action_blocking(move || {
                    let _native_guard = native_guard;
                    type_text_blocking(
                        pid,
                        &text_clone,
                        element_ptr,
                        delay_ms,
                        is_terminal_target,
                        KeyboardRoute {
                            delivery_mode,
                            window_id,
                            keyboard_policy: blocking_policy,
                            explicit_target: element_ptr.is_some() || used_pixel_focus,
                            key_window_confirmed: false,
                            replace,
                            force_key_events: replace,
                            chromium_native_candidate,
                            prior,
                            focused_key_route: !delivery_mode.is_foreground()
                                && element_ptr.is_some()
                                && crate::input::skylight::is_focus_without_raise_available(),
                        },
                    )
                })
                .await
            },
        )
        .await;

        let changes = super::finish_window_observation(snapshot).await;

        // Unwrap the delivery envelope: a structured refusal means no
        // actuator ran and the caller gets the exact reason.
        let result = match result {
            Ok(Ok(TypeTextDelivery::Refused(refusal))) => {
                let wid = window_id.expect("background refusals require a window target");
                let result = super::not_editable::account_focus_receipt(
                    super::background_refusal_result(pid, wid, &refusal),
                    token_receipt,
                );
                return super::not_editable::account_focus_receipt(result, px_receipt);
            }
            Ok(Ok(TypeTextDelivery::SynthesisRefused {
                path,
                refusal,
                ax_attempt,
            })) => {
                let result = super::not_editable::account_focus_receipt(
                    synthesis_refusal_result(path, &refusal, ax_attempt),
                    token_receipt,
                );
                return super::not_editable::account_focus_receipt(result, px_receipt);
            }
            Ok(Ok(TypeTextDelivery::Typed(outcome))) => Ok(Ok(outcome)),
            Ok(Ok(TypeTextDelivery::InvalidArguments(reason))) => {
                return super::invalid_arguments(reason)
            }
            Ok(Err(error)) => Ok(Err(error)),
            Err(error) => Err(error),
        };

        let result = match result {
            Ok(Ok(outcome)) if outcome.delivered_chars.is_some_and(|n| n < char_count) => {
                let delivered_chars = outcome.delivered_chars.unwrap_or_default();
                let mut result = ToolResult::error(format!(
                    "type_text incomplete: delivered {delivered_chars} of {char_count} character(s){}; retry only the remaining suffix",
                    outcome.detail
                ))
                .with_structured(serde_json::json!({
                    "code": "type_text_incomplete",
                    "path": outcome.path,
                    "effect": "partial",
                    "requested_chars": char_count,
                    "delivered_chars": delivered_chars,
                    "retryable": true,
                    "retry_from_character": delivered_chars,
                }));
                if outcome.path == PATH_AX {
                    result =
                        result.with_disposition(cua_driver_contract::ErrorDisposition::DELIVERED);
                }
                result
            }
            Ok(Ok(outcome)) => type_text_outcome_result(
                outcome,
                char_count,
                used_pixel_focus,
                crate::browser::electron_js::ElectronJs::is_electron(pid),
                &changes.result_suffix(),
            ),
            Ok(Err(e)) => text_delivery_failure("type_text failed", &e),
            Err(e) => ToolResult::error(format!(
                "Text worker failed: {e}; delivery is unknown. Read fresh state before retrying."
            ))
            .with_structured(serde_json::json!({"effect": "unverifiable", "verified": false}))
            .with_disposition(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP),
        };
        // Any refusal after a delivered focus action carries its receipt.
        let result = super::not_editable::account_focus_receipt(result, token_receipt);
        super::not_editable::account_focus_receipt(result, px_receipt)
    }
}

fn type_text_outcome_result(
    outcome: TypeTextOutcome,
    char_count: usize,
    used_pixel_focus: bool,
    electron: bool,
    suffix: &str,
) -> ToolResult {
    let TypeTextOutcome {
        detail,
        path,
        verified,
        delivered_chars,
        web_area,
    } = outcome;
    // SURFACE-AWARE VERIFICATION. On any web-content surface —
    // Chromium/WebKit/Electron — AXValue is not independent renderer
    // evidence. It can report a changed value after either an AX write
    // or synthesized keystrokes while the renderer/DOM still observes
    // no edit (the Slack-search AND Chrome-on-X false-confirms). Detect
    // this at the ELEMENT level (an `AXWebArea` ancestor) so it covers
    // every browser + Electron uniformly, yet a browser's OWN native
    // chrome (address bar, toolbar) stays trusted. Probe ONLY when a
    // path with AXValue-only verification would otherwise confirm, so
    // native types and already-unverified deliveries pay nothing.
    let web_area = ((verified || path == PATH_AX) && path_has_untrusted_web_readback(path))
        .then_some(web_area);
    let target_is_web_content = web_area
        .is_some_and(|classification| target_readback_is_untrusted(classification, electron));
    let verification = surface_verification(path, verified, target_is_web_content);
    let verified = verification.verified;
    let untrusted_web_readback =
        verification.untrusted_web_readback || (path == PATH_AX && target_is_web_content);
    let electron_web_content = untrusted_web_readback && electron;

    // `verified:false` means the driver could not confirm the text
    // landed (Electron AX echo, unreadable AXValue on Catalyst, or a
    // CGEvent rung the app may have dropped). Don't dress that as a
    // confirmed insert — tell the agent to look, and point at the
    // right next rung.
    let (mark, note) = if verified {
        ("✅ Inserted", String::new())
    } else if untrusted_web_readback {
        let web = web_area == Some(WebAreaClassification::WebContent);
        let next_step = if electron_web_content && used_pixel_focus {
            "The pixel-focus rung already ran, so do not repeat it; verify the \
                         result via the screenshot."
        } else if web {
            "For a browser tab use the `page` tool (it drives the DOM); for an \
                         embedded web view, re-type with the px form (x,y)."
        } else {
            "Read the window to check the text before sending it again."
        };
        let surface = if web {
            "web-content surface (Chromium / WebKit / Electron): AXValue \
                         read-back is not independent proof that the renderer/DOM observed \
                         the input."
        } else {
            "the window's focused element could not be read, so the AXValue \
                         read-back cannot be tied to the target window."
        };
        ("📨 Sent (unverified)", format!(" — {surface} {next_step}"))
    } else if path == PATH_KEY_EVENTS_FG {
        (
            "📨 Sent (unverified)",
            " — driver could not confirm; verify via screenshot.".to_string(),
        )
    } else {
        (
            "📨 Sent (unverified)",
            " — driver could not confirm the text landed; verify via screenshot, \
                      and re-call with delivery_mode:\"foreground\" if it didn't."
                .to_string(),
        )
    };
    ToolResult::text(format!(
        "{mark} {char_count} char(s){detail}.{note}{}",
        suffix
    ))
    .with_structured({
        // `effect` mirrors `verified`'s read-back tri-state: a TRUSTED
        // positive read-back is "confirmed"; an unreadable/unchanged
        // AXValue, a dropped CGEvent rung, or an Electron AX echo we
        // refuse to trust is "unverifiable".
        let mut s = serde_json::json!({
            "path": path,
            "characters": char_count,
            "requested_chars": char_count,
            "verified": verified,
            "effect": if verified { "confirmed" } else { "unverifiable" },
        });
        if !untrusted_web_readback {
            if let Some(delivered_chars) = delivered_chars {
                s["delivered_chars"] = serde_json::json!(delivered_chars);
            }
        }
        if untrusted_web_readback {
            // Web-content AXValue read-back. A real browser TAB → the
            // `page` tool (drives the DOM via CDP) is the reliable rung;
            // an embedded web view (Electron, no CDP) → the element px
            // action. It's a renderer/DOM-focus problem, never a
            // foreground one.
            let escalation = match web_readback_next_rung(electron_web_content, used_pixel_focus) {
                Some("px") => Some((
                    "px",
                    "Electron web view — AXValue read-back cannot prove \
                                 that the renderer observed the input. Confirm via the \
                                 screenshot; if it didn't land, re-type with the \
                                 element px action (x,y to pixel-focus the field, then \
                                 type).",
                )),
                Some("page") => Some((
                    "page",
                    "Browser web content — AXValue read-back cannot prove \
                                 that the DOM observed the input (and AX type_text on a \
                                 contenteditable is racy). Drive the tab's DOM with the \
                                 `page` tool: execute_javascript + el.value/innerText for a \
                                 plain input; for a rich-text contenteditable \
                                 (Draft.js/Lexical/Slate-style editors can silently discard \
                                 a one-shot DOM write on their next render) try insert_text \
                                 first (one CDP call, cheap), then type_keystrokes if that \
                                 also gets discarded (real per-character keyboard events, \
                                 slower but most durable). Or confirm via the screenshot.",
                )),
                _ => None,
            };
            if let Some((recommended, reason)) = escalation {
                s["escalation"] = serde_json::json!({
                    "recommended": recommended,
                    "reason": reason,
                });
            }
        } else if !verified && path != PATH_KEY_EVENTS_FG {
            s["escalation"] = serde_json::json!({
                "recommended": "foreground",
                "reason": "background insert could not be confirmed — \
                           re-call with delivery_mode:\"foreground\" if a \
                           screenshot shows the text didn't land."
            });
        }
        s
    })
}

/// The error result for a failed text delivery. A paste that failed after its
/// Cmd+V went out may already be in the field: it says so as
/// `effect:"unverifiable"`, so the caller reads the field before typing again.
pub(crate) fn text_delivery_failure(prefix: &str, error: &anyhow::Error) -> ToolResult {
    let message = format!("{prefix}: {error:#}");
    if let Some(write) = error.downcast_ref::<TextWriteError>() {
        let effect = if write.chain.no_input() {
            "refused"
        } else {
            "unverifiable"
        };
        let mut result = ToolResult::error(message).with_structured(serde_json::json!({
            "path": "ax", "effect": effect, "verified": false,
        }));
        if let Some(disposition) = write.chain.error_disposition(
            cua_driver_core::mutation::Postcondition::Indeterminate,
            false,
        ) {
            result = result.with_disposition(disposition);
        }
        return result;
    }
    if let Some(scope) = error.downcast_ref::<TextScopeError>() {
        let effect = if scope.chain.no_input() {
            "refused"
        } else {
            "unverifiable"
        };
        let mut result = ToolResult::error(message).with_structured(serde_json::json!({
            "code": super::WINDOW_NOT_KEY, "effect": effect, "keys_sent": false,
        }));
        if let Some(disposition) = scope.chain.error_disposition(
            cua_driver_core::mutation::Postcondition::Indeterminate,
            false,
        ) {
            result = result.with_disposition(disposition);
        }
        return result;
    }
    match error.downcast_ref::<crate::input::keyboard::PasteMayHaveLanded>() {
        Some(paste) => ToolResult::error(format!(
            "{message}. Read the field before typing the text again."
        ))
        .with_structured(serde_json::json!({
            "code": "paste_may_have_landed",
            "effect": "unverifiable",
            "clipboard_restored": paste.clipboard_restored(),
            "retryable": false,
        })),
        None => ToolResult::error(message),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct TextWriteError {
    message: String,
    chain: cua_driver_core::mutation::ChainResult,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct TextScopeError {
    message: String,
    chain: cua_driver_core::mutation::ChainResult,
}

// ── Blocking implementation ───────────────────────────────────────────────────

/// Which delivery path was taken. Surfaced as `structuredContent.path`
/// on success.
const PATH_AX: &str = "ax";
const PATH_KEY_EVENTS: &str = "key_events";
const PATH_KEY_EVENTS_FG: &str = "key_events_fg";

// The daemon transport has a 150-second reply deadline. Character synthesis
// is synchronous and costs at least one 8ms key-down gap plus either the
// requested delay or an 8ms key-up gap per character. Keep the complete
// scheduled synthesis sleeps + read-back estimate within 100 seconds, reserving
// 20 seconds for event construction/posting, routing, focus assistance,
// queueing, response serialization, and transport.
// Atomic AX writes are deliberately excluded: their cost does not scale per
// character and a single write remains the preferred route for large text.
const SYNTHESIS_BUDGET_MS: u64 = 100_000;
const KEY_DOWN_GAP_MS: u64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextDeliveryRoute {
    AtomicAx,
    UnicodeSynthesis,
    PhysicalSynthesis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SynthesisRefusal {
    requested_chars: usize,
    estimated_duration_ms: u64,
    per_character_ms: u64,
    max_chunk_chars: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AxAttempt {
    NotAttempted,
    #[cfg(test)]
    Rejected,
    Unverifiable,
}

impl AxAttempt {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotAttempted => "not_attempted",
            #[cfg(test)]
            Self::Rejected => "rejected",
            Self::Unverifiable => "unverifiable",
        }
    }
}

fn synthesis_preflight(
    route: TextDeliveryRoute,
    requested_chars: usize,
    delay_ms: u64,
) -> Option<SynthesisRefusal> {
    if route == TextDeliveryRoute::AtomicAx {
        return None;
    }
    let per_character_ms = match route {
        TextDeliveryRoute::AtomicAx => unreachable!(),
        // PID-routed and desktop Unicode paths post one key-down and one
        // key-up, sleeping 8ms after the down and max(delay, 8) after the up.
        TextDeliveryRoute::UnicodeSynthesis => {
            KEY_DOWN_GAP_MS.saturating_add(delay_ms.max(KEY_DOWN_GAP_MS))
        }
        // Physical HID may need Shift down/up around each printable key. Use
        // that four-event worst case for a payload-independent safe bound.
        TextDeliveryRoute::PhysicalSynthesis => 24u64.saturating_add(delay_ms.max(8)),
    };
    let drain_ms = DELIVERY_DRAIN_TIMEOUT.as_millis() as u64;
    let estimated_duration_ms = (requested_chars as u64)
        .saturating_mul(per_character_ms)
        .saturating_add(drain_ms);
    if estimated_duration_ms <= SYNTHESIS_BUDGET_MS {
        return None;
    }
    let max_chunk_chars = SYNTHESIS_BUDGET_MS
        .saturating_sub(drain_ms)
        .checked_div(per_character_ms)
        .unwrap_or_default() as usize;
    Some(SynthesisRefusal {
        requested_chars,
        estimated_duration_ms,
        per_character_ms,
        max_chunk_chars,
    })
}

fn synthesis_refusal_result(
    path: &'static str,
    refusal: &SynthesisRefusal,
    ax_attempt: AxAttempt,
) -> ToolResult {
    let effect = if ax_attempt == AxAttempt::Unverifiable {
        "indeterminate"
    } else {
        "refused"
    };
    let retryable = ax_attempt != AxAttempt::Unverifiable;
    let escalation = if retryable {
        serde_json::json!({
            "recommended": "chunk",
            "reason": format!(
                "Character synthesis would exceed the bounded transport-safe budget. Retry in chunks of at most {} characters at this delay.",
                refusal.max_chunk_chars
            )
        })
    } else {
        serde_json::json!({
            "recommended": "verify_state",
            "reason": "The atomic AX attempt could not be observed. Re-read the target before deciding whether any suffix remains; do not retry blindly."
        })
    };
    let mut structured = serde_json::json!({
        "code": "type_text_synthesis_budget_exceeded",
        "path": path,
        "effect": effect,
        "requested_chars": refusal.requested_chars,
        "estimated_duration_ms": refusal.estimated_duration_ms,
        "synthesis_budget_ms": SYNTHESIS_BUDGET_MS,
        "per_character_ms": refusal.per_character_ms,
        "max_chunk_chars": refusal.max_chunk_chars,
        "synthesized_chars": 0,
        "atomic_ax_effect": ax_attempt.as_str(),
        "retryable": retryable,
        "escalation": escalation,
    });
    if ax_attempt != AxAttempt::Unverifiable {
        structured["delivered_chars"] = serde_json::json!(0);
        structured["retry_from_character"] = serde_json::json!(0);
    }
    let message = if retryable {
        format!(
            "type_text refused character synthesis before emitting character events: {} characters require an estimated {}ms at {}ms per character, exceeding the {}ms budget; retry in chunks of at most {} characters",
            refusal.requested_chars,
            refusal.estimated_duration_ms,
            refusal.per_character_ms,
            SYNTHESIS_BUDGET_MS,
            refusal.max_chunk_chars,
        )
    } else {
        format!(
            "type_text did not synthesize character events because {} characters require an estimated {}ms, exceeding the {}ms budget; the preceding atomic AX attempt was unverifiable, so re-read the target before retrying",
            refusal.requested_chars, refusal.estimated_duration_ms, SYNTHESIS_BUDGET_MS,
        )
    };
    ToolResult::error(message).with_structured(structured)
}

fn path_has_untrusted_web_readback(path: &str) -> bool {
    path == PATH_AX || path == PATH_KEY_EVENTS || path == PATH_KEY_EVENTS_FG
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfaceVerification {
    verified: bool,
    untrusted_web_readback: bool,
}

fn surface_verification(
    path: &str,
    verified: bool,
    target_is_web_content: bool,
) -> SurfaceVerification {
    let untrusted_web_readback =
        verified && target_is_web_content && path_has_untrusted_web_readback(path);
    SurfaceVerification {
        verified: verified && !untrusted_web_readback,
        untrusted_web_readback,
    }
}

fn web_readback_next_rung(is_electron: bool, used_pixel_focus: bool) -> Option<&'static str> {
    match (is_electron, used_pixel_focus) {
        (true, true) => None,
        (true, false) => Some("px"),
        (false, _) => Some("page"),
    }
}

fn valid_chromium_native_submission(text: &str) -> bool {
    let body = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .or_else(|| text.strip_suffix('\r'));
    body.is_some_and(|body| !body.contains(['\n', '\r', '\t']))
}

const DELIVERY_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const DELIVERY_DRAIN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// Pause before the single `AXFocused` re-apply in the foreground rung. Covers
/// an app that installs its own first responder just after activation is
/// observable, which would otherwise clobber the first write.

/// Keyboard-rung policy for one window-addressed background insert, decided
/// once by the pure exact-target core before any input is posted.
#[derive(Clone, Debug)]
enum BackgroundKeyboardPolicy {
    /// The full ladder may run: foreground requests, pid-only targets, and
    /// window targets whose exact keyboard delivery is proven (singleton
    /// same-pid destination).
    Allowed,
    /// Only the semantic AX write on the proven exact element may run. The
    /// process-scoped CGEvent rung is refused with this refusal — carried so
    /// the exact reason is returned if the AX write does not land.
    SemanticOnly(BackgroundRefusal),
}

/// Delivery envelope for `type_text_blocking`: either an actuator ran
/// (`Typed`), or the exact-target decision refused before any event was
/// posted (`Refused`) and the caller must return the structured refusal.
enum TypeTextDelivery {
    Typed(TypeTextOutcome),
    InvalidArguments(String),
    Refused(BackgroundRefusal),
    SynthesisRefused {
        path: &'static str,
        refusal: SynthesisRefusal,
        ax_attempt: AxAttempt,
    },
}

/// Decide the keyboard policy for a window-addressed background `type_text`.
///
/// Gathers fresh exact-target facts once and asks the pure core:
/// - `InsertText` executes → the full ladder is `Allowed`;
/// - `InsertText` refused but the caller addressed an exact element whose
///   ancestry is proven and semantic AX executes → `SemanticOnly`;
/// - otherwise the structured refusal result is returned and no input of any
///   kind (including a px focus click) may be sent.
async fn background_keyboard_policy(
    pid: i32,
    window_id: u32,
    element_ptr: Option<usize>,
) -> Result<(super::BackgroundMutationLease, BackgroundKeyboardPolicy), ToolResult> {
    use cua_driver_core::background_input::{
        decide_background_input, BackgroundAction, BackgroundInputDecision, ExactWindowTarget,
    };
    let lease = super::acquire_background_mutation(pid).await;
    if element_ptr.is_some_and(|ptr| !element_in_requested_window(pid, window_id, ptr)) {
        return Err(super::background_refusal_result(
            pid,
            window_id,
            &BackgroundRefusal {
                code: "element_outside_target_window",
                reason: "the addressed text element is not in the requested window or its exact sheet ancestry; no text was sent".to_owned(),
                advice: Some("get_window_state"),
            },
        ));
    }
    let element_guard =
        element_ptr.map(|ptr| unsafe { crate::ax::snapshot::RetainedElement::retain(ptr) });
    let facts_window_id = element_ptr
        .filter(|ptr| element_in_requested_window(pid, window_id, *ptr))
        .map(|ptr| gate_window_id_for_element(ptr, window_id))
        .unwrap_or(window_id);
    let facts = match latency::spawn_action_blocking(move || {
        let element_ptr = element_guard.as_ref().map(|guard| guard.as_ptr());
        crate::ax::exact_target::gather_background_facts(pid, facts_window_id, element_ptr)
    })
    .await
    {
        Ok(facts) => facts,
        Err(error) => {
            return Err(ToolResult::error(format!(
                "Could not gather exact-target facts for pid {pid} window {window_id}: {error}"
            )));
        }
    };
    let target = ExactWindowTarget {
        pid,
        window_id: facts_window_id,
    };
    // The exact window is made key for the key post (see
    // `super::post_keys_to_window`), so a sibling window is not ambiguous.
    let action =
        if element_ptr.is_some() && crate::input::skylight::is_focus_without_raise_available() {
            BackgroundAction::ExactWindowKeysFocused
        } else {
            super::window_keyboard_action(BackgroundAction::InsertText)
        };
    match decide_background_input(target, &facts, action) {
        BackgroundInputDecision::Execute { .. } => Ok((lease, BackgroundKeyboardPolicy::Allowed)),
        BackgroundInputDecision::Refuse(refusal) => {
            let semantic_available = element_ptr.is_some()
                && decide_background_input(target, &facts, BackgroundAction::AxSemantic)
                    .is_execute();
            if semantic_available {
                Ok((lease, BackgroundKeyboardPolicy::SemanticOnly(refusal)))
            } else {
                Err(super::background_refusal_result(pid, window_id, &refusal))
            }
        }
    }
}

struct TypeTextOutcome {
    detail: String,
    path: &'static str,
    verified: bool,
    /// Exact when AX exposed the target value. `None` means delivery could not
    /// be observed, so the existing unverifiable contract remains in force.
    delivered_chars: Option<usize>,
    /// Surface classification of the retained receiver, captured for the
    /// outer result projection so a later focus move cannot change trust.
    web_area: WebAreaClassification,
}

fn withheld_ax_insert_outcome(
    role: &str,
    title: &str,
    document: crate::document_edit::DocumentEdited,
    web_area: WebAreaClassification,
) -> TypeTextOutcome {
    TypeTextOutcome {
        detail: format!(
            " via AX write into {role} \"{title}\"; the value could not be safely read{}",
            document.note()
        ),
        path: PATH_AX,
        verified: false,
        delivered_chars: None,
        web_area,
    }
}

fn replacement_readable(
    replace: bool,
    before: &crate::ax::write_outcome::ValueRead,
) -> anyhow::Result<()> {
    if replace && *before == crate::ax::write_outcome::ValueRead::Withheld {
        anyhow::bail!("replace:true requires a readable AXValue before mutation; no replacement payload was sent");
    }
    Ok(())
}

// Preserve the replacement sequence in one place so the native route and
// deterministic tests observe the same write, readback and document mark.
struct TextWriteContext {
    settable: crate::ax::write_outcome::Settable,
    prior: cua_driver_core::mutation::PriorInput,
}

fn replace_ax_with(
    attribute: &str,
    before: &str,
    text: &str,
    context: TextWriteContext,
    send: impl FnOnce() -> i32,
    mark: impl FnOnce() -> crate::document_edit::DocumentEdited,
    verify: impl FnOnce() -> (bool, Option<usize>),
) -> anyhow::Result<(bool, Option<usize>, crate::document_edit::DocumentEdited)> {
    use crate::ax::write_outcome::{native_write_outcome, NativeWriteKind, Settable};
    use cua_driver_core::mutation::{ChainResult, WriteOutcome};
    let call_ran = context.settable != Settable::No;
    let err = if call_ran { send() } else { kAXErrorSuccess };
    let outcome = native_write_outcome(
        NativeWriteKind::SetAttribute,
        call_ran,
        &context.settable,
        err,
    );
    let chain =
        ChainResult::from_recorded(vec![outcome], context.prior).expect("one replacement setter");
    if outcome != WriteOutcome::Delivered {
        return Err(TextWriteError { message: format!("AXUIElementSetAttributeValue({attribute}) was not confirmed (native error {err}, called={call_ran}); no retry was sent"), chain }.into());
    }
    let document = if before != text {
        mark()
    } else {
        crate::document_edit::DocumentEdited::NotADocument
    };
    let (verified, delivered) = verify();
    Ok((verified, delivered, document))
}

enum InsertRead {
    Unsupported,
    Withheld { error: i32 },
    Readable { error: i32, value: Option<String> },
}

fn send_ax_insert_and_read(
    before_withheld: bool,
    send: impl FnOnce() -> i32,
    read: impl FnOnce() -> crate::ax::write_outcome::ValueRead,
) -> InsertRead {
    let error = send();
    let value = read();
    if before_withheld || value == crate::ax::write_outcome::ValueRead::Withheld {
        InsertRead::Withheld { error }
    } else {
        InsertRead::Readable {
            error,
            value: value.text(),
        }
    }
}

fn insert_ax_write(
    before_withheld: bool,
    context: TextWriteContext,
    send: impl FnOnce() -> i32,
    read: impl FnOnce() -> crate::ax::write_outcome::ValueRead,
) -> Result<InsertRead, TextWriteError> {
    use crate::ax::write_outcome::{native_write_outcome, NativeWriteKind, Settable};
    use cua_driver_core::mutation::{ChainResult, WriteOutcome};
    if context.settable == Settable::No {
        let outcome = native_write_outcome(
            NativeWriteKind::SetAttribute,
            false,
            &context.settable,
            kAXErrorSuccess,
        );
        let _chain =
            ChainResult::from_recorded(vec![outcome], context.prior).expect("one insert setter");
        return Ok(InsertRead::Unsupported);
    }
    let observation = send_ax_insert_and_read(before_withheld, send, read);
    let error = match &observation {
        InsertRead::Withheld { error } | InsertRead::Readable { error, .. } => *error,
        InsertRead::Unsupported => unreachable!("the setter was called"),
    };
    let outcome = native_write_outcome(
        NativeWriteKind::SetAttribute,
        true,
        &context.settable,
        error,
    );
    let chain =
        ChainResult::from_recorded(vec![outcome], context.prior).expect("one insert setter");
    if outcome != WriteOutcome::Delivered {
        return Err(TextWriteError { message: format!("AXSelectedText returned error {error}; insertion was not confirmed and no retry was sent"), chain });
    }
    Ok(observation)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypedProgress {
    Complete,
    Partial(usize),
    Unchanged,
    Unverifiable,
}

fn foreground_settle_ms(pid: i32, frontmost_pid: Option<i32>) -> u64 {
    if frontmost_pid == Some(pid) {
        20
    } else {
        200
    }
}

/// Read-back verification for a keystroke rung: did the typed text actually land?
///
/// `before`/`after` are `AXValue` read from the target field before and after
/// the keystrokes. Only `Complete` positively confirms the text landed:
/// - unreadable `after` (`None`) → `Unverifiable` (Catalyst case; the agent
///   must confirm via screenshot).
/// - `after` contains the complete text → `Complete`.
/// - empty input text → trivially `Complete`.
///
/// Otherwise classify an observable insertion without mistaking a prefix for
/// complete delivery. A positive length delta is an exact delivered-character
/// count for insert-at-cursor typing; it is capped at the request size
/// defensively. Apps that normalize input (smart quotes, autocomplete) may fail
/// the substring/length test even though something landed — we report it as
/// unverified rather than erroring, so the agent can still confirm.
fn typed_progress(before: Option<&str>, after: Option<&str>, text: &str) -> TypedProgress {
    if text.is_empty() {
        return TypedProgress::Complete;
    }
    let Some(after) = after else {
        return TypedProgress::Unverifiable;
    };
    if after.contains(text) {
        return TypedProgress::Complete;
    }
    // Enter, Tab and Return are not characters a field keeps: they submit,
    // move focus, or reset the field (a spreadsheet Name Box jumps and
    // rewrites itself). A read-back after them says nothing about how many
    // characters landed, so a shortfall must not be reported as partial
    // delivery that the caller then retries.
    if text.contains(['\n', '\r', '\t']) {
        return TypedProgress::Unverifiable;
    }
    let Some(before) = before else {
        return TypedProgress::Unverifiable;
    };
    let delivered = after
        .chars()
        .count()
        .saturating_sub(before.chars().count())
        .min(text.chars().count());
    if delivered == 0 {
        TypedProgress::Unchanged
    } else {
        TypedProgress::Partial(delivered)
    }
}

fn ax_insert_progress(
    before: Option<&str>,
    after: Option<&str>,
    text: &str,
    selection: Option<(usize, usize)>,
    bound: bool,
) -> TypedProgress {
    use cua_driver_core::mutation::{
        compare, expected_after, Action, Before, Observed, Postcondition,
    };
    if !bound {
        return TypedProgress::Unverifiable;
    }
    let range = match selection {
        Some((start, len)) => match start.checked_add(len) {
            Some(end) => Some(start..end),
            None => return TypedProgress::Unverifiable,
        },
        None => None,
    };
    if let Some(value) = before {
        let units: Vec<u16> = value.encode_utf16().collect();
        match &range {
            Some(range)
                if range.start <= range.end
                    && range.end <= units.len()
                    && String::from_utf16(&units[..range.start]).is_ok()
                    && String::from_utf16(&units[range.end..]).is_ok() => {}
            None if value.is_empty() => {}
            _ => return TypedProgress::Unverifiable,
        }
    }
    let expected = expected_after(
        Action::TypeText(text),
        Before::Value {
            value: before,
            selection: range.clone(),
        },
    );
    match compare(&expected, Observed::Value(after), false) {
        Postcondition::Confirmed => TypedProgress::Complete,
        Postcondition::Unchanged => TypedProgress::Unchanged,
        Postcondition::Subset => {
            let Some((original, current)) = before.zip(after) else {
                return TypedProgress::Unverifiable;
            };
            let range = range.unwrap_or(0..0);
            let units: Vec<u16> = original.encode_utf16().collect();
            let Some(prefix_units) = units.get(..range.start) else {
                return TypedProgress::Unverifiable;
            };
            let Some(suffix_units) = units.get(range.end..) else {
                return TypedProgress::Unverifiable;
            };
            let (Ok(prefix), Ok(suffix)) = (
                String::from_utf16(prefix_units),
                String::from_utf16(suffix_units),
            ) else {
                return TypedProgress::Unverifiable;
            };
            let Some(inserted) = current
                .strip_prefix(&prefix)
                .and_then(|s| s.strip_suffix(&suffix))
            else {
                return TypedProgress::Unverifiable;
            };
            TypedProgress::Partial(inserted.chars().count())
        }
        Postcondition::Indeterminate | Postcondition::Unavailable(_) => TypedProgress::Unverifiable,
    }
}

/// Read the focused/target field's `AXValue`, for before/after read-back.
/// Re-fetches the focused element each call when no explicit element is given
/// (cheap, and focus is stable across our own keystrokes).
fn read_axvalue(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
) -> crate::ax::write_outcome::ValueRead {
    use crate::ax::write_outcome::{read_value_if_allowed, ValueObs, ValueRead};
    if let Some((ptr, _)) = element_ptr_and_idx {
        read_value_if_allowed(
            &mut crate::ax::label_reader::NativeLabelReader,
            ptr as AXUIElementRef,
        )
    } else if let Some(el) = unsafe { focused_element_of_pid(pid) } {
        let v = read_value_if_allowed(&mut crate::ax::label_reader::NativeLabelReader, el);
        unsafe {
            CFRelease(el as _);
        }
        v
    } else {
        ValueRead::Allowed(ValueObs::Missing)
    }
}

/// Window-bound variant of [`read_axvalue`]: when no explicit element is
/// addressed and a `window_id` is known, the focused element is used ONLY when
/// its ancestry provably resolves to that exact window. A sibling window's
/// focused field must never supply before/after evidence for the requested
/// target — an unprovable focus reads as `None` (unverifiable), never as
/// sibling data. Without a window the legacy pid-global read applies.
fn read_axvalue_bound(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
) -> Option<String> {
    read_axvalue_bound_classified(pid, element_ptr_and_idx, window_id).text()
}

fn read_axvalue_bound_classified(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
) -> crate::ax::write_outcome::ValueRead {
    use crate::ax::write_outcome::{read_value_if_allowed, ValueObs, ValueRead};
    if element_ptr_and_idx.is_some() {
        return read_axvalue(pid, element_ptr_and_idx);
    }
    match window_id {
        Some(wid) => unsafe {
            let Some(el) = crate::ax::exact_target::focused_element_in_window(pid, wid) else {
                return ValueRead::Allowed(ValueObs::Missing);
            };
            let v = read_value_if_allowed(&mut crate::ax::label_reader::NativeLabelReader, el);
            CFRelease(el as _);
            v
        },
        None => read_axvalue(pid, None),
    }
}

/// True when the addressed (or focused) AX element sits inside a web-content
/// subtree — an `AXWebArea` ancestor. That covers every Chromium / WebKit /
/// Electron rendered surface (Chrome, Safari, Slack, VS Code, X's compose box…),
/// where an AX write is echoed back through `AXValue` while the renderer/DOM
/// never observes it — so an AX read-back "confirm" there is a shim echo. A
/// browser's OWN native chrome (address bar, toolbar) has no `AXWebArea`
/// ancestor, so it stays trusted. Walks a bounded ancestor chain; each
/// `AXParent` copy is released, and it stops at the window/app boundary.
/// The element's ancestry was read to its window and has no web area: only
/// then is an AXValue read-back evidence of the insert.
pub(super) fn target_is_proven_native(pid: i32, element_ptr: usize, window_id: u32) -> bool {
    classify_target_web_area(pid, Some((element_ptr, None)), Some(window_id))
        == WebAreaClassification::NonWebContent
}

pub(super) fn target_in_web_area(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
) -> bool {
    web_readback_is_untrusted(classify_target_web_area(
        pid,
        element_ptr_and_idx,
        window_id,
    ))
}

/// Native AX focus is sufficient only when the complete ancestry proves that
/// the target is outside web content. An incomplete walk must use the checked
/// pointer route (or refuse) rather than promoting an AX focus echo to proof.
pub(super) fn target_requires_pointer_focus(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
) -> bool {
    classify_target_web_area(pid, element_ptr_and_idx, window_id)
        != WebAreaClassification::NonWebContent
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebAreaClassification {
    WebContent,
    NonWebContent,
    Incomplete,
    WindowFocusUnavailable,
}

fn web_readback_is_untrusted(classification: WebAreaClassification) -> bool {
    matches!(
        classification,
        WebAreaClassification::WebContent | WebAreaClassification::WindowFocusUnavailable
    )
}

fn target_readback_is_untrusted(classification: WebAreaClassification, _is_electron: bool) -> bool {
    classification != WebAreaClassification::NonWebContent
}

fn classify_target_web_area(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
) -> WebAreaClassification {
    use crate::ax::bindings::AXUIElementCopyAttributeValue;
    use core_foundation::base::{CFTypeRef, TCFType};
    use core_foundation::string::CFString;
    unsafe {
        // Start element: the addressed one (borrowed — do NOT release) or the
        // focused element (owned — must release when done). A window-addressed
        // request may only classify from the window's OWN focused element; a
        // pid-global focused element can belong to a same-process sibling and
        // sibling state must never vouch for the target. When window-bound
        // reacquisition fails, fail closed: report web content (untrusted
        // read-back) rather than trusting an unproven surface.
        let (start, start_owned) = match element_ptr_and_idx {
            Some((ptr, _)) => (ptr as AXUIElementRef, false),
            None => match window_id {
                Some(wid) => match crate::ax::exact_target::focused_element_in_window(pid, wid) {
                    Some(el) => (el, true),
                    None => return WebAreaClassification::WindowFocusUnavailable,
                },
                None => match focused_element_of_pid(pid) {
                    Some(el) => (el, true),
                    None => return WebAreaClassification::NonWebContent,
                },
            },
        };
        let parent_attr = CFString::new("AXParent");
        let mut cur = start;
        let mut cur_owned = start_owned;
        let mut classification = WebAreaClassification::Incomplete;
        for _ in 0..40 {
            match copy_string_attr(cur, "AXRole").as_deref() {
                Some("AXWebArea") => {
                    classification = WebAreaClassification::WebContent;
                    break;
                }
                // No web area lives above the window/app root — stop.
                Some("AXWindow") | Some("AXApplication") => {
                    classification = WebAreaClassification::NonWebContent;
                    break;
                }
                None => break,
                _ => {}
            }
            let mut parent: CFTypeRef = std::ptr::null_mut();
            let err =
                AXUIElementCopyAttributeValue(cur, parent_attr.as_concrete_TypeRef(), &mut parent);
            if cur_owned {
                CFRelease(cur as CFTypeRef);
            }
            if err != kAXErrorSuccess || parent.is_null() {
                cur = std::ptr::null_mut();
                cur_owned = false;
                break;
            }
            cur = parent as AXUIElementRef;
            cur_owned = true;
        }
        if cur_owned && !cur.is_null() {
            CFRelease(cur as CFTypeRef);
        }
        classification
    }
}

/// True when the window's own focused element is an editable field that sits
/// OUTSIDE the page's `AXWebArea` — the browser's native chrome (omnibox,
/// find bar). A paste or typed chord aimed at "the page" would land there.
/// Conservative: unknown focus, a non-editable role, or any AX failure is
/// `false`, so an unprobeable browser is never refused.
pub(super) fn window_focus_is_browser_chrome_field(pid: i32, window_id: u32) -> bool {
    unsafe {
        let Some(el) = crate::ax::exact_target::focused_element_in_window(pid, window_id) else {
            return false;
        };
        let role = copy_string_attr(el, "AXRole");
        CFRelease(el as _);
        if !matches!(
            role.as_deref(),
            Some("AXTextField") | Some("AXComboBox") | Some("AXSearchField")
        ) {
            return false;
        }
    }
    matches!(
        classify_target_web_area(pid, None, Some(window_id)),
        WebAreaClassification::NonWebContent
    )
}

/// Whether `pid` is a web browser whose omnibox and pages the foreground
/// typing rung must activate fully (see the browser branch of the rung).
pub(super) fn is_browser_pid(pid: i32) -> bool {
    let bundle_id = apps::bundle_id_for_pid(pid).unwrap_or_default();
    crate::browser::browser_js::BrowserJs::supports(&bundle_id) || is_chromium_identity(&bundle_id)
}

pub(crate) fn is_chromium_identity(bundle_id: &str) -> bool {
    bundle_id
        .to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| {
            matches!(
                token,
                "chrome" | "chromium" | "brave" | "edgemac" | "vivaldi" | "opera" | "thorium"
            )
        })
}

const SELECTED_RANGE: &str = "AXSelectedTextRange";

/// Copy an attribute's raw value (+1 retained), whatever its type.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef`; the caller releases the
/// returned value.
unsafe fn copy_raw_attr(
    element: AXUIElementRef,
    name: &str,
) -> Option<core_foundation::base::CFTypeRef> {
    use core_foundation::{base::TCFType, string::CFString};
    let attr = CFString::new(name);
    let mut value: core_foundation::base::CFTypeRef = std::ptr::null();
    let err = crate::ax::bindings::AXUIElementCopyAttributeValue(
        element,
        attr.as_concrete_TypeRef(),
        &mut value,
    );
    (err == kAXErrorSuccess && !value.is_null()).then_some(value)
}

/// Set an attribute to a raw value previously read with [`copy_raw_attr`].
///
/// # Safety
///
/// `element` and `value` must be valid for the duration of the call.
unsafe fn set_raw_attr(
    element: AXUIElementRef,
    name: &str,
    value: core_foundation::base::CFTypeRef,
) -> i32 {
    use core_foundation::{base::TCFType, string::CFString};
    let attr = CFString::new(name);
    crate::ax::bindings::AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), value)
}

fn restore_range_with(
    settable: crate::ax::write_outcome::Settable,
    prior: cua_driver_core::mutation::PriorInput,
    send: impl FnOnce() -> i32,
) -> Result<(), TextWriteError> {
    use crate::ax::write_outcome::{native_write_outcome, NativeWriteKind};
    use cua_driver_core::mutation::{ChainResult, WriteOutcome};
    let error = send();
    let outcome = native_write_outcome(NativeWriteKind::SetAttribute, true, &settable, error);
    let chain =
        ChainResult::from_recorded(vec![outcome], prior).expect("one restored range setter");
    if outcome == WriteOutcome::Delivered {
        Ok(())
    } else {
        Err(TextWriteError {
            message: format!(
                "restoring AXSelectedTextRange returned error {error}; no text was sent"
            ),
            chain,
        })
    }
}

/// After an activation, wait until WindowServer has kept `window_id`'s
/// process front for [`FRONT_SETTLE_STABLE`], bounded by
/// [`FRONT_SETTLE_TIMEOUT`]. Returns whether it settled.
fn await_front_settled(pid: i32, window_id: u32) -> bool {
    let deadline = std::time::Instant::now() + FRONT_SETTLE_TIMEOUT;
    let mut since: Option<std::time::Instant> = None;
    loop {
        let now = std::time::Instant::now();
        if crate::input::skylight::front_process_matches(pid, window_id) == Some(true) {
            if now.duration_since(*since.get_or_insert(now)) >= FRONT_SETTLE_STABLE {
                return true;
            }
        } else {
            since = None;
        }
        if now >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

const FRONT_SETTLE_STABLE: std::time::Duration = std::time::Duration::from_millis(250);
const FRONT_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// The receiver a keystroke rung types into: the process, its requested
/// window, and the retained element (with its snapshot index) when known.
#[derive(Clone, Copy)]
struct KeyReceiver {
    pid: i32,
    window_id: Option<u32>,
    element: Option<(usize, Option<usize>)>,
}

/// What the keystroke rung sends and how it paces the first key.
#[derive(Clone, Copy)]
struct KeyTextRequest<'a> {
    text: &'a str,
    delay_ms: u64,
    /// First-keystroke settle; the foreground rung passes a positive value.
    settle_ms: u64,
    replace: bool,
    /// A Chromium native field submission: type the body, then one Return.
    single_terminal_return: bool,
}

/// Proofs required immediately before the first key is posted.
#[derive(Clone, Copy)]
struct KeyFocusChecks {
    /// Require CFEqual app focus plus the matching key window on the receiver.
    exact_focus: bool,
    /// A window-only or pixel receiver: re-run the editing-cell check after
    /// the first-key settle, because activation can end an editing session.
    recheck_editability_after_settle: bool,
    prior: cua_driver_core::mutation::PriorInput,
    focus_already_written: bool,
}

fn focus_write_with(
    settable: crate::ax::write_outcome::Settable,
    prior: cua_driver_core::mutation::PriorInput,
    send: impl FnOnce() -> i32,
) -> Result<(), TextWriteError> {
    use crate::ax::write_outcome::{native_write_outcome, NativeWriteKind};
    use cua_driver_core::mutation::{ChainResult, WriteOutcome};
    let error = send();
    let outcome = native_write_outcome(NativeWriteKind::SetAttribute, true, &settable, error);
    let chain = ChainResult::from_recorded(vec![outcome], prior).expect("one focus setter");
    if outcome == WriteOutcome::Delivered {
        Ok(())
    } else {
        Err(TextWriteError {
            message: format!("AXFocused returned error {error}; no text was sent"),
            chain,
        })
    }
}

/// Type via CGEvent keystrokes at the current insertion point, then verify by
/// read-back against `before`. `type_text` is deliberately non-idempotent:
/// it must never clear an existing value merely because AX cannot read that
/// value back.
fn cgevent_type_verified(
    receiver: KeyReceiver,
    request: KeyTextRequest<'_>,
    before: Option<&str>,
    checks: KeyFocusChecks,
) -> anyhow::Result<(
    bool,
    Option<usize>,
    bool,
    crate::document_edit::DocumentEdited,
)> {
    let KeyReceiver {
        pid,
        window_id,
        element: element_ptr_and_idx,
    } = receiver;
    let KeyTextRequest {
        text,
        delay_ms,
        settle_ms,
        replace,
        single_terminal_return,
    } = request;
    let KeyFocusChecks {
        exact_focus: focused_key_route,
        recheck_editability_after_settle,
        mut prior,
        focus_already_written,
    } = checks;
    if replace {
        let (ptr, _) = element_ptr_and_idx.ok_or_else(|| {
            anyhow::anyhow!("replace:true requires an exact window-bound text element")
        })?;
        let element = ptr as AXUIElementRef;
        let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
        let subrole = unsafe { copy_string_attr(element, "AXSubrole") }.unwrap_or_default();
        if !replacement_text_role_allowed(&role, &subrole) {
            anyhow::bail!(
                "replace:true requires an editable non-secure text role, not {role}/{subrole}"
            )
        }
    }
    // A sent focus write is a separate mutation; never repeat it after an
    // uncertain return or merely because a subsequent focus read is stale.
    if let Some((ptr, _)) = element_ptr_and_idx.filter(|_| !focus_already_written) {
        let settable =
            unsafe { crate::ax::write_outcome::query_settable(ptr as AXUIElementRef, "AXFocused") };
        focus_write_with(settable, prior, || {
            crate::input::ax_actions::focus_element_raw(ptr)
        })?;
        prior = cua_driver_core::mutation::PriorInput::MayHaveDelivered;
    }
    let web_delivery = element_ptr_and_idx.is_some()
        && classify_target_web_area(pid, element_ptr_and_idx, window_id)
            != WebAreaClassification::NonWebContent;
    if web_delivery {
        let (ptr, _) = element_ptr_and_idx.expect("web delivery requires an exact element");
        let wid = window_id.ok_or_else(|| {
            anyhow::anyhow!("web text delivery requires an exact window and element")
        })?;
        click_element_center_for_focus_checked(pid, wid, ptr, settle_ms > 0)?;
        if !element_has_exact_app_focus(pid, wid, ptr) {
            anyhow::bail!(
                "the web target did not become the application's focused element after its verified pointer click; no text was sent"
            )
        }
    }
    if focused_key_route && !web_delivery {
        let (ptr, _) = element_ptr_and_idx.expect("focused key route requires an element");
        let wid = window_id.expect("focused key route requires a window");
        if !element_has_exact_app_focus(pid, wid, ptr) {
            anyhow::bail!(
                "the addressed element is not the application's focused element in window {wid}; no text was sent"
            )
        }
    }
    if replace {
        let (ptr, _) = element_ptr_and_idx.ok_or_else(|| {
            anyhow::anyhow!("replace:true requires an exact window-bound text element")
        })?;
        let wid = window_id.ok_or_else(|| anyhow::anyhow!("replace:true requires window_id"))?;
        let before = before.ok_or_else(|| {
            anyhow::anyhow!("replace:true requires a readable AXValue before mutation")
        })?;
        if !element_has_exact_app_focus(pid, wid, ptr) {
            anyhow::bail!(
                "replace:true target is not the application's focused element in the requested window; no selection or text was sent"
            )
        }
        let element = ptr as AXUIElementRef;
        let selected = if web_delivery {
            crate::input::keyboard::press_key(pid, "a", &["command"])?;
            wait_for_full_selection(pid, wid, ptr, before)
        } else {
            // The element focus write above has already run, independently of
            // the following selected-range setter.
            match select_entire_native_value(pid, wid, ptr, before, prior) {
                FullSelection::Selected => true,
                FullSelection::Unsupported => false,
                FullSelection::AllocationFailed(chain) => return Err(TextWriteError { message: "Could not allocate the full-selection range; no replacement payload was sent".into(), chain }.into()),
                FullSelection::NativeFailed(error, chain) => return Err(TextWriteError { message: format!("AXSelectedTextRange returned error {error}; no replacement payload was sent"), chain }.into()),
                FullSelection::Unconfirmed(chain) => return Err(TextWriteError { message: "Full selection was not confirmed; no replacement payload was sent".into(), chain }.into()),
            }
        };
        if !selected {
            if web_delivery {
                anyhow::bail!(
                    "replace:true could not prove the web target's full selection; no replacement payload was sent"
                )
            }
            // A native whole-value write does not depend on the selection. It
            // is safe only while focus and the original value still hold.
            if unsafe { crate::ax::write_outcome::read_text_value(element) }.as_deref()
                != Some(before)
                || !element_has_exact_app_focus(pid, wid, ptr)
            {
                anyhow::bail!(
                    "replace:true selection outcome was uncertain and the target changed; no AXValue fallback was sent"
                )
            }
            let (verified, delivered, document) = replace_ax_with(
                "AXValue",
                before,
                text,
                TextWriteContext {
                    settable: unsafe {
                        crate::ax::write_outcome::query_settable(element, "AXValue")
                    },
                    prior,
                },
                || unsafe { set_string_attr(element, "AXValue", text) },
                || unsafe {
                    crate::document_edit::ensure_marked_after_ax_write(pid, element, false)
                },
                || {
                    await_replacement_delivery(
                        text,
                        std::time::Instant::now() + DELIVERY_DRAIN_TIMEOUT,
                        || read_axvalue_bound(pid, element_ptr_and_idx, window_id),
                    )
                },
            )?;
            return Ok((verified, delivered, true, document));
        }
        if web_delivery {
            if text.is_empty() {
                crate::input::keyboard::press_key(pid, "delete", &[])?;
            } else {
                crate::input::keyboard::type_text_with_delay(pid, text, delay_ms)?;
            }
        } else {
            let (verified, delivered, document) = replace_ax_with(
                "AXSelectedText",
                before,
                text,
                TextWriteContext {
                    settable: unsafe {
                        crate::ax::write_outcome::query_settable(element, "AXSelectedText")
                    },
                    prior,
                },
                || unsafe { set_string_attr(element, "AXSelectedText", text) },
                || unsafe {
                    crate::document_edit::ensure_marked_after_ax_write(pid, element, false)
                },
                || {
                    await_replacement_delivery(
                        text,
                        std::time::Instant::now() + DELIVERY_DRAIN_TIMEOUT,
                        || read_axvalue_bound(pid, element_ptr_and_idx, window_id),
                    )
                },
            )?;
            return Ok((verified, delivered, true, document));
        }
        let deadline = std::time::Instant::now() + DELIVERY_DRAIN_TIMEOUT;
        let (verified, delivered) = await_replacement_delivery(text, deadline, || {
            read_axvalue_bound(pid, element_ptr_and_idx, window_id)
        });
        return Ok((
            verified,
            delivered,
            !web_delivery,
            crate::document_edit::DocumentEdited::NotADocument,
        ));
    }
    // First-keystroke settle (foreground rung only — caller passes `settle_ms > 0`).
    // Even once the window is front and the element focused, the surface isn't
    // ready to accept input for a few tens of ms, so the FIRST synthesized
    // character gets eaten: typing "i love u" rendered "love u" (the leading
    // "i " was dropped). A short sleep here covers that. Background/terminal call
    // sites pass 0 — they have no front transition and must not pay this latency.
    if settle_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(settle_ms));
        if recheck_editability_after_settle {
            late_fg_cell_check(pid, window_id, false)?;
        }
        if focused_key_route || web_delivery {
            let (ptr, _) = element_ptr_and_idx.expect("exact key focus requires an element");
            let wid = window_id.expect("exact key focus requires a window");
            if !element_has_exact_app_focus(pid, wid, ptr) {
                anyhow::bail!(
                    "the receiver lost exact application focus during the first-key settle; no text was sent"
                )
            }
        }
    }
    if single_terminal_return {
        let body = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .or_else(|| text.strip_suffix('\r'))
            .unwrap_or(text);
        crate::input::keyboard::type_text_with_delay(pid, body, delay_ms)?;
        crate::input::keyboard::press_key(pid, "return", &[])?;
    } else {
        crate::input::keyboard::type_text_with_delay(pid, text, delay_ms)?;
    }

    // CGEvent posting is asynchronous with respect to the renderer. In
    // particular, Chromium can acknowledge the posting process while a long
    // tail remains queued. Poll the AX value until the complete payload is
    // visible instead of treating any growth as success. If the deadline
    // expires after observable growth, surface the exact partial count.
    let deadline = std::time::Instant::now() + DELIVERY_DRAIN_TIMEOUT;
    let withheld = std::cell::Cell::new(false);
    let (verified, delivered) = await_typed_delivery(
        before,
        text,
        deadline,
        || {
            let value = read_axvalue_bound_classified(pid, element_ptr_and_idx, window_id);
            if value == crate::ax::write_outcome::ValueRead::Withheld {
                withheld.set(true);
            }
            value.text()
        },
        || {
            target_readback_is_untrusted(
                classify_target_web_area(pid, element_ptr_and_idx, window_id),
                crate::browser::electron_js::ElectronJs::is_electron(pid),
            )
        },
    );
    if withheld.get() {
        Ok((
            false,
            None,
            false,
            crate::document_edit::DocumentEdited::NotADocument,
        ))
    } else {
        Ok((
            verified,
            delivered,
            false,
            crate::document_edit::DocumentEdited::NotADocument,
        ))
    }
}

fn element_has_exact_app_focus(pid: i32, window_id: u32, element_ptr: usize) -> bool {
    element_in_requested_window(pid, window_id, element_ptr)
        && crate::input::ax_actions::is_element_focused(pid, element_ptr)
        && focused_window_matches_element(pid, window_id, element_ptr)
}

/// Confirm the actual key window as well as the focused element. A retained
/// focused element can outlive a key-window transition, and PID-routed keys
/// follow the latter.
pub(super) fn focused_window_matches_element(pid: i32, window_id: u32, element_ptr: usize) -> bool {
    let focused = crate::ax::bindings::focused_window_id_of_pid(pid);
    key_window_matches(
        focused,
        window_id,
        || gate_window_id_for_element(element_ptr, window_id),
        || crate::ax::exact_target::sheet_parent_of(pid, window_id).is_some(),
    )
}

/// A sheet's element maps to the sheet's parent. That parent stands in for
/// the requested window only when the request named the parent (a sheet
/// token from the parent's snapshot); a request that names the sheet itself
/// needs the sheet as the key window.
fn key_window_matches(
    focused: Option<u32>,
    window_id: u32,
    element_window: impl FnOnce() -> u32,
    requested_is_sheet: impl FnOnce() -> bool,
) -> bool {
    match focused {
        Some(focused) if focused == window_id => true,
        Some(focused) => focused == element_window() && !requested_is_sheet(),
        None => false,
    }
}

/// Prove actual window/sheet ancestry, including #104's sheet token contract.
/// A mapped parent id alone is insufficient because menu and unrelated parent
/// elements must not pass. The bounded parent walk accepts only the requested
/// CGWindowID itself.
pub(super) fn element_in_requested_window(pid: i32, window_id: u32, element_ptr: usize) -> bool {
    let _ = pid;
    let element = element_ptr as AXUIElementRef;
    unsafe {
        if crate::ax::exact_target::element_window_id(element) == Some(window_id) {
            return true;
        }
        let mut current = element;
        let mut owned = false;
        for _ in 0..40 {
            if crate::ax::bindings::ax_get_window_id(current) == Some(window_id) {
                if owned {
                    CFRelease(current as _);
                }
                return true;
            }
            let parent = crate::ax::bindings::copy_element_attr(current, "AXParent");
            if owned {
                CFRelease(current as _);
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
            CFRelease(current as _);
        }
    }
    false
}

/// The top-level window used for visibility/ownership gating. A sheet's raw
/// key scope still uses its own id; its parent proves the modal surface facts.
pub(super) fn gate_window_id_for_element(element_ptr: usize, fallback: u32) -> u32 {
    unsafe { crate::ax::exact_target::element_window_id(element_ptr as AXUIElementRef) }
        .unwrap_or(fallback)
}

#[repr(C)]
struct CfRange {
    location: isize,
    length: isize,
}

unsafe fn selected_range(element: AXUIElementRef) -> Option<(usize, usize)> {
    let value = copy_raw_attr(element, SELECTED_RANGE)?;
    let mut range = CfRange {
        location: 0,
        length: 0,
    };
    let ok = crate::ax::bindings::AXValueGetValue(
        value as _,
        crate::ax::bindings::kAXValueCFRangeType,
        &mut range as *mut CfRange as *mut _,
    );
    CFRelease(value);
    (ok && range.location >= 0 && range.length >= 0)
        .then_some((range.location as usize, range.length as usize))
}

fn full_selection_matches(element: AXUIElementRef, before: &str) -> bool {
    unsafe {
        crate::ax::write_outcome::read_text_value(element).as_deref() == Some(before)
            && copy_string_attr(element, "AXSelectedText").as_deref() == Some(before)
            && selected_range(element) == Some((0, before.encode_utf16().count()))
    }
}

fn replacement_text_role_allowed(role: &str, subrole: &str) -> bool {
    if role == "AXSecureTextField" || subrole == "AXSecureTextField" {
        return false;
    }
    matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox"
    )
}

enum FullSelection {
    Selected,
    Unsupported,
    AllocationFailed(cua_driver_core::mutation::ChainResult),
    NativeFailed(i32, cua_driver_core::mutation::ChainResult),
    Unconfirmed(cua_driver_core::mutation::ChainResult),
}

fn select_entire_with(
    settable: crate::ax::write_outcome::Settable,
    prior: cua_driver_core::mutation::PriorInput,
    allocate: impl FnOnce() -> bool,
    send: impl FnOnce() -> i32,
    confirm: impl FnOnce() -> bool,
) -> FullSelection {
    use crate::ax::write_outcome::{native_write_outcome, NativeWriteKind, Settable};
    use cua_driver_core::mutation::{ChainResult, WriteOutcome};
    if settable == Settable::No {
        let outcome = native_write_outcome(
            NativeWriteKind::SetAttribute,
            false,
            &settable,
            kAXErrorSuccess,
        );
        let _chain = ChainResult::from_recorded(vec![outcome], prior).expect("one selection rung");
        return FullSelection::Unsupported;
    }
    if !allocate() {
        let outcome = native_write_outcome(
            NativeWriteKind::SetAttribute,
            false,
            &settable,
            kAXErrorSuccess,
        );
        return FullSelection::AllocationFailed(
            ChainResult::from_recorded(vec![outcome], prior).expect("one selection rung"),
        );
    }
    let error = send();
    let outcome = native_write_outcome(NativeWriteKind::SetAttribute, true, &settable, error);
    let chain = ChainResult::from_recorded(vec![outcome], prior).expect("one selection rung");
    if outcome != WriteOutcome::Delivered {
        return FullSelection::NativeFailed(error, chain);
    }
    if confirm() {
        FullSelection::Selected
    } else {
        FullSelection::Unconfirmed(chain)
    }
}

fn select_entire_native_value(
    pid: i32,
    window_id: u32,
    element_ptr: usize,
    before: &str,
    prior: cua_driver_core::mutation::PriorInput,
) -> FullSelection {
    use core_foundation::{base::TCFType, string::CFString};
    let element = element_ptr as AXUIElementRef;
    let range = CfRange {
        location: 0,
        length: before.encode_utf16().count() as isize,
    };
    let value = std::cell::Cell::new(std::ptr::null());
    let settable = unsafe { crate::ax::write_outcome::query_settable(element, SELECTED_RANGE) };
    select_entire_with(
        settable,
        prior,
        || {
            let allocated = unsafe {
                crate::ax::bindings::AXValueCreate(
                    crate::ax::bindings::kAXValueCFRangeType,
                    &range as *const CfRange as *const _,
                )
            };
            value.set(allocated);
            !allocated.is_null()
        },
        || {
            let attr = CFString::new(SELECTED_RANGE);
            let error = unsafe {
                crate::ax::bindings::AXUIElementSetAttributeValue(
                    element,
                    attr.as_concrete_TypeRef(),
                    value.get() as _,
                )
            };
            unsafe { CFRelease(value.get() as _) };
            error
        },
        || wait_for_full_selection(pid, window_id, element_ptr, before),
    )
}

fn wait_for_full_selection(pid: i32, window_id: u32, element_ptr: usize, before: &str) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    loop {
        if full_selection_matches(element_ptr as AXUIElementRef, before)
            && element_has_exact_app_focus(pid, window_id, element_ptr)
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(DELIVERY_DRAIN_POLL_INTERVAL);
    }
}

fn await_replacement_delivery(
    text: &str,
    deadline: std::time::Instant,
    read_value: impl FnMut() -> Option<String>,
) -> (bool, Option<usize>) {
    await_replacement_delivery_with(
        text,
        read_value,
        || std::time::Instant::now() >= deadline,
        || std::thread::sleep(DELIVERY_DRAIN_POLL_INTERVAL),
    )
}

fn await_replacement_delivery_with(
    text: &str,
    mut read_value: impl FnMut() -> Option<String>,
    mut expired: impl FnMut() -> bool,
    mut pause: impl FnMut(),
) -> (bool, Option<usize>) {
    loop {
        if read_value().as_deref() == Some(text) {
            return (true, Some(text.chars().count()));
        }
        if expired() {
            return (false, None);
        }
        pause();
    }
}

/// Click an exact element only while pointer delivery, the actual hit target,
/// and both coordinate frames still prove the requested destination.
pub(super) fn click_element_center_for_focus_checked(
    pid: i32,
    window_id: u32,
    element_ptr: usize,
    foreground: bool,
) -> anyhow::Result<()> {
    use cua_driver_core::background_input::{
        decide_background_input, BackgroundAction, BackgroundInputDecision, ExactWindowTarget,
    };
    let facts = crate::ax::exact_target::gather_background_facts(pid, window_id, Some(element_ptr));
    if !matches!(
        decide_background_input(
            ExactWindowTarget { pid, window_id },
            &facts,
            BackgroundAction::WindowPointer,
        ),
        BackgroundInputDecision::Execute { .. }
    ) {
        anyhow::bail!(
            "window-local pointer focus is not permitted for the exact target; no click was sent"
        )
    }
    let element = element_ptr as AXUIElementRef;
    let before_rect = unsafe { crate::ax::bindings::element_screen_rect(element) }
        .ok_or_else(|| anyhow::anyhow!("focus target has no readable frame"))?;
    let before_bounds = crate::windows::window_bounds_by_id(window_id)
        .ok_or_else(|| anyhow::anyhow!("window {window_id} has no readable bounds"))?;
    let (screen_x, screen_y) = (
        before_rect[0] + before_rect[2] / 2.0,
        before_rect[1] + before_rect[3] / 2.0,
    );
    let hit = unsafe {
        crate::ax::bindings::element_at_screen_position_bounded(pid, screen_x, screen_y, 0.5)
    }
    .ok_or_else(|| anyhow::anyhow!("no accessibility hit target at the requested field center"))?;
    let hit_matches = element_in_requested_window(pid, window_id, hit as usize)
        && unsafe { element_or_bounded_descendant(hit, element) };
    if !hit_matches {
        let hit_role = unsafe { copy_string_attr(hit, "AXRole") };
        let hit_rect = unsafe { crate::ax::bindings::element_screen_rect(hit) };
        unsafe { CFRelease(hit as _) };
        anyhow::bail!(
            "the requested field center hit another element (hit_role={hit_role:?}, hit_rect={hit_rect:?}, target_rect={before_rect:?}); no pointer click was sent"
        )
    }
    unsafe { CFRelease(hit as _) };
    let after_rect = unsafe { crate::ax::bindings::element_screen_rect(element) };
    let after_bounds = crate::windows::window_bounds_by_id(window_id);
    let frames_match = after_rect == Some(before_rect)
        && after_bounds.as_ref().is_some_and(|after| {
            after.x == before_bounds.x
                && after.y == before_bounds.y
                && after.width == before_bounds.width
                && after.height == before_bounds.height
        });
    if !frames_match {
        anyhow::bail!("the target or window frame moved before pointer focus; no click was sent")
    }
    let wx = screen_x - before_bounds.x;
    let wy = screen_y - before_bounds.y;
    if wx < 0.0 || wx >= before_bounds.width || wy < 0.0 || wy >= before_bounds.height {
        anyhow::bail!("the requested field center lies outside the exact window")
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

pub(super) unsafe fn element_or_bounded_descendant(
    mut current: AXUIElementRef,
    target: AXUIElementRef,
) -> bool {
    use core_foundation::base::{CFEqual, CFTypeRef};
    let mut owns_current = false;
    for _ in 0..40 {
        if CFEqual(current as CFTypeRef, target as CFTypeRef) != 0 {
            if owns_current {
                CFRelease(current as CFTypeRef);
            }
            return true;
        }
        let role = copy_string_attr(current, "AXRole");
        if matches!(role.as_deref(), Some("AXWindow" | "AXApplication")) {
            if owns_current {
                CFRelease(current as CFTypeRef);
            }
            return false;
        }
        let Some(parent) = crate::ax::bindings::copy_element_attr(current, "AXParent") else {
            if owns_current {
                CFRelease(current as CFTypeRef);
            }
            return false;
        };
        if owns_current {
            CFRelease(current as CFTypeRef);
        }
        current = parent;
        owns_current = true;
    }
    if owns_current {
        CFRelease(current as CFTypeRef);
    }
    false
}

fn await_typed_delivery(
    before: Option<&str>,
    text: &str,
    deadline: std::time::Instant,
    mut read_value: impl FnMut() -> Option<String>,
    is_web: impl FnOnce() -> bool,
) -> (bool, Option<usize>) {
    let mut best_partial = None;
    loop {
        let after = read_value();
        match typed_progress(before, after.as_deref(), text) {
            TypedProgress::Complete => return (true, Some(text.chars().count())),
            TypedProgress::Partial(delivered) => {
                best_partial =
                    Some(best_partial.map_or(delivered, |best: usize| best.max(delivered)));
            }
            TypedProgress::Unverifiable => return (false, None),
            TypedProgress::Unchanged => {
                // A readable unchanged value is an observed zero-character
                // delivery, not an unverifiable success.
                best_partial.get_or_insert(0);
            }
        }
        if std::time::Instant::now() >= deadline {
            if best_partial.is_some() && is_web() {
                best_partial = None;
            }
            return (false, best_partial);
        }
        std::thread::sleep(DELIVERY_DRAIN_POLL_INTERVAL);
    }
}

/// How `type_text_blocking` may deliver: the requested mode, the addressed
/// window, and the background keyboard policy decided for it.
struct KeyboardRoute {
    delivery_mode: super::DeliveryMode,
    window_id: Option<u32>,
    keyboard_policy: BackgroundKeyboardPolicy,
    /// The caller chose where the text goes: an `element_token`, or `x,y`
    /// that pixel-focused a field first.
    explicit_target: bool,
    /// The addressed window was confirmed as the app's key window before this
    /// call. An app that is not active reports no focused element even then
    /// (LibreOffice), so a missing focus is not a reason to refuse.
    key_window_confirmed: bool,
    replace: bool,
    force_key_events: bool,
    chromium_native_candidate: bool,
    focused_key_route: bool,
    prior: cua_driver_core::mutation::PriorInput,
}

fn combine_prior(
    a: cua_driver_core::mutation::PriorInput,
    b: cua_driver_core::mutation::PriorInput,
) -> cua_driver_core::mutation::PriorInput {
    use cua_driver_core::mutation::PriorInput;
    if a == PriorInput::MayHaveDelivered || b == PriorInput::MayHaveDelivered {
        PriorInput::MayHaveDelivered
    } else {
        PriorInput::None
    }
}

fn prior_for_receipt(
    receipt: Option<super::not_editable::FocusDispatch>,
) -> cua_driver_core::mutation::PriorInput {
    use cua_driver_core::mutation::PriorInput;
    match receipt {
        None | Some(super::not_editable::FocusDispatch::None) => PriorInput::None,
        Some(_) => PriorInput::MayHaveDelivered,
    }
}

fn prior_for_key_entry(
    prior: cua_driver_core::mutation::PriorInput,
    entry: crate::input::skylight::KeyEntry,
) -> cua_driver_core::mutation::PriorInput {
    use cua_driver_core::mutation::PriorInput;
    if entry.records_posted > 0 || entry.records_uncertain || entry.ax_write {
        PriorInput::MayHaveDelivered
    } else {
        prior
    }
}

fn scope_entry_failure(
    error: crate::input::skylight::ScopeEntryError,
    previous: cua_driver_core::mutation::PriorInput,
) -> TextScopeError {
    use crate::input::skylight::{scope_disclosure, HandBack};
    use cua_driver_core::mutation::{ChainResult, PriorInput, Reason, WriteOutcome};
    let mut prior = prior_for_key_entry(previous, error.entry);
    if error
        .hand_back
        .is_some_and(|hand_back| hand_back != HandBack::NotMoved)
    {
        prior = PriorInput::MayHaveDelivered;
    }
    let chain = ChainResult::from_recorded(
        vec![WriteOutcome::StopNotDelivered(Reason::UnconfirmedAlive)],
        prior,
    )
    .expect("one scope entry stop");
    let disclosure = scope_disclosure(error.entry, error.hand_back);
    TextScopeError {
        message: format!("{}; no text was sent. {disclosure}", error.reason),
        chain,
    }
}

fn text_key_scope(
    pid: i32,
    window_id: Option<u32>,
    prior: cua_driver_core::mutation::PriorInput,
) -> Result<Option<crate::input::skylight::WindowKeyScope>, TextScopeError> {
    use crate::input::skylight;
    let Some(wid) = window_id else {
        return Ok(None);
    };
    if !skylight::is_available() {
        return Err(scope_entry_failure(
            skylight::ScopeEntryError {
                reason: skylight::KEY_POST_SPI_UNAVAILABLE.into(),
                entry: Default::default(),
                hand_back: None,
            },
            prior,
        ));
    }
    if !skylight::is_focus_without_raise_available() {
        return Ok(None);
    }
    skylight::enter_exact_window_key_inline_detailed(pid, wid)
        .map(Some)
        .map_err(|error| scope_entry_failure(error, prior))
}

/// Best-effort-background ladder for `type_text`.
///
/// - `delivery_mode == Background` (default): AX insert → read-back; on a
///   silent/unreadable accept, CGEvent keystrokes → read-back. Never fronts.
/// - `delivery_mode == Foreground`: the agent's explicit last resort — briefly
///   front `window_id`, insert at the current cursor, restore, then read-back.
///
/// Returns `(detail, path, verified)`. `verified` is `true` only when a
/// read-back positively confirmed the text; `false` means the agent must
/// confirm via screenshot (and, for background, can escalate to foreground).
fn type_text_blocking(
    pid: i32,
    text: &str,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    delay_ms: u64,
    is_terminal_target: bool,
    route: KeyboardRoute,
) -> anyhow::Result<TypeTextDelivery> {
    // A window-addressed background insert first makes that window the app's
    // key window, proven by re-reading the app's focused window, BEFORE the
    // focused element is read or any key is posted. Unconfirmed: refuse
    // (CUA-1767: with Find & Replace open the keys went into its Find field).
    let _scope = if !route.delivery_mode.is_foreground()
        && matches!(route.keyboard_policy, BackgroundKeyboardPolicy::Allowed)
    {
        text_key_scope(pid, route.window_id, route.prior)?
    } else {
        None
    };
    let route = KeyboardRoute {
        key_window_confirmed: _scope.is_some(),
        prior: _scope
            .as_ref()
            .map(|scope| prior_for_key_entry(route.prior, scope.entry()))
            .unwrap_or(route.prior),
        ..route
    };
    type_text_in_key_window(
        pid,
        text,
        element_ptr_and_idx,
        delay_ms,
        is_terminal_target,
        route,
    )
}

fn type_text_in_key_window(
    pid: i32,
    text: &str,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    delay_ms: u64,
    is_terminal_target: bool,
    route: KeyboardRoute,
) -> anyhow::Result<TypeTextDelivery> {
    let KeyboardRoute {
        delivery_mode,
        window_id,
        keyboard_policy,
        explicit_target,
        key_window_confirmed,
        replace,
        force_key_events,
        chromium_native_candidate,
        focused_key_route,
        prior,
    } = route;
    // Original field value before any rung drives read-back verification only.
    // An unreadable value is not evidence that the field is empty — and for a
    // window-addressed request it must come from the exact target window,
    // never a same-process sibling.
    // A background terminal insert has no semantic AX rung: when the
    // exact-target decision restricted this request to semantic-only, there is
    // nothing safe to run, and an over-budget synthesis cannot start. Both
    // refusals need no read of the target, so they come before any AX call.
    if is_terminal_target && !delivery_mode.is_foreground() {
        if let BackgroundKeyboardPolicy::SemanticOnly(refusal) = &keyboard_policy {
            return Ok(TypeTextDelivery::Refused(refusal.clone()));
        }
        if let Some(refusal) = synthesis_preflight(
            TextDeliveryRoute::UnicodeSynthesis,
            text.chars().count(),
            delay_ms,
        ) {
            return Ok(TypeTextDelivery::SynthesisRefused {
                path: PATH_KEY_EVENTS,
                refusal,
                ax_attempt: AxAttempt::NotAttempted,
            });
        }
    }
    // Late CELL recheck inside the delivery context: key-window/foreground
    // activation may have replaced the first responder since the early
    // guard, so the actual focused element is re-read before any rung types.
    // Unreadable fails closed through the existing no_focused_text_input
    // refusal; a PID-only target uses an error instead of the window-bound
    // structured refusal (which would panic on window_id.expect).
    if element_ptr_and_idx.is_none() && !replace {
        match unsafe { super::not_editable::classify_untargeted(pid, window_id) } {
            crate::ax::editable::CellVerdict::NotEditing => {
                if window_id.is_none() {
                    anyhow::bail!(
                        "not_editable: the focused element at delivery is a non-editing cell;                          no text was sent"
                    );
                }
                return Ok(TypeTextDelivery::Refused(BackgroundRefusal {
                    code: "not_editable",
                    reason: "the focused element at delivery is a non-editing cell; no text                              was sent."
                        .into(),
                    advice: None,
                }));
            }
            crate::ax::editable::CellVerdict::Unreadable => {
                if window_id.is_none() {
                    anyhow::bail!(
                        "no_focused_text_input: the focused element at delivery could not be                          read; no text was sent"
                    );
                }
                return Ok(TypeTextDelivery::Refused(no_focused_text_input_refusal(
                    false,
                )));
            }
            _ => {}
        }
    }
    // Retain the actual background receiver once. Classification, AX writes,
    // key delivery and readback must all refer to this same element: focus can
    // move between separate lookups, including from native chrome into a web
    // field where AXSelectedText is only an echo.
    let delivery_guard = if element_ptr_and_idx.is_none() {
        window_id.and_then(|wid| unsafe {
            crate::ax::exact_target::focused_element_in_window(pid, wid).map(|ptr| {
                let owned = crate::ax::snapshot::RetainedElement::retain(ptr as usize);
                CFRelease(ptr as _);
                owned
            })
        })
    } else {
        None
    };
    let retained_window_receiver = element_ptr_and_idx.is_none() && delivery_guard.is_some();
    let element_ptr_and_idx = delivery_guard
        .as_ref()
        .map(|guard| (guard.as_ptr(), None))
        .or(element_ptr_and_idx);
    let before_read = read_axvalue_bound_classified(pid, element_ptr_and_idx, window_id);
    let before_withheld = before_read == crate::ax::write_outcome::ValueRead::Withheld;
    replacement_readable(replace, &before_read)?;
    let before = before_read.text();
    let actual_web_classification = classify_target_web_area(pid, element_ptr_and_idx, window_id);
    let actual_web_delivery = actual_web_classification != WebAreaClassification::NonWebContent;
    let chromium_native_field = chromium_native_candidate && !actual_web_delivery;
    if chromium_native_field
        && text.contains(['\n', '\r', '\t'])
        && !valid_chromium_native_submission(text)
    {
        return Ok(TypeTextDelivery::InvalidArguments(
            "Chromium native fields accept only one terminal newline for submission, with no embedded newline, carriage return or tab.".to_owned(),
        ));
    }
    let chromium_native_submission = chromium_native_field && text.contains(['\n', '\r']);
    let force_key_events = force_key_events || actual_web_delivery || chromium_native_submission;
    let focused_key_route = (delivery_mode.is_foreground() && element_ptr_and_idx.is_some())
        || focused_key_route
        || (!delivery_mode.is_foreground()
            && element_ptr_and_idx.is_some()
            && crate::input::skylight::is_focus_without_raise_available());
    let late_fg_targeted = element_ptr_and_idx.is_some() && !retained_window_receiver;

    // --- Foreground rung: explicit agent request (skip AX/background ladder). ---
    if delivery_mode.is_foreground() {
        let screen_sharing_target = crate::input::keyboard::is_screen_sharing_pid(pid);
        if let Some(refusal) = synthesis_preflight(
            if screen_sharing_target {
                TextDeliveryRoute::PhysicalSynthesis
            } else {
                TextDeliveryRoute::UnicodeSynthesis
            },
            text.chars().count(),
            delay_ms,
        ) {
            return Ok(TypeTextDelivery::SynthesisRefused {
                path: if window_id.is_some() {
                    PATH_KEY_EVENTS_FG
                } else {
                    PATH_KEY_EVENTS
                },
                refusal,
                ax_attempt: AxAttempt::NotAttempted,
            });
        }
        // Settle between front+focus and the first keystroke — see the
        // "i love u" -> "love u" first-char-drop note in cgevent_type_verified.
        // A target that was already frontmost pays only 20ms for element-focus
        // settling. Focus-proxy clients that were just activated need longer:
        // an RDP client (Microsoft Windows App) re-arms its keyboard grab with
        // the remote host over hundreds of ms, so at 60ms every keystroke was
        // dropped. 200ms covers that re-grab without penalizing an already
        // armed interactive stream on every text chunk.
        let foreground_settle_ms = foreground_settle_ms(pid, apps::frontmost_pid());
        let do_type = |element: Option<(usize, Option<usize>)>, focus_already_written: bool| {
            cgevent_type_verified(
                KeyReceiver {
                    pid,
                    window_id,
                    element,
                },
                KeyTextRequest {
                    text,
                    delay_ms,
                    settle_ms: foreground_settle_ms,
                    replace,
                    single_terminal_return: chromium_native_submission,
                },
                before.as_deref(),
                KeyFocusChecks {
                    exact_focus: focused_key_route,
                    recheck_editability_after_settle: !late_fg_targeted,
                    focus_already_written,
                    prior: if window_id.is_some() {
                        cua_driver_core::mutation::PriorInput::MayHaveDelivered
                    } else {
                        prior
                    },
                },
            )
        };
        let ((verified, delivered_chars, ax_replacement, document), fronted) = match window_id {
            Some(wid) if screen_sharing_target => {
                // Screen Sharing forwards physical HID transitions to the
                // guest. PID-routed Unicode events all carry keycode 0 (the A
                // key), so a guest sees "aaaa"; modifier flags alone likewise
                // turn Cmd+V into plain "v". The explicit foreground rung may
                // safely use the global HID queue while the exact target is
                // guarded and restored.
                crate::input::skylight::with_foreground_hid_activation(
                    pid as libc::pid_t,
                    wid,
                    || {
                        late_fg_cell_check(pid, Some(wid), late_fg_targeted)?;
                        if let Some((ptr, _)) = element_ptr_and_idx {
                            let settable = unsafe {
                                crate::ax::write_outcome::query_settable(
                                    ptr as AXUIElementRef,
                                    "AXFocused",
                                )
                            };
                            focus_write_with(
                                settable,
                                cua_driver_core::mutation::PriorInput::MayHaveDelivered,
                                || crate::input::ax_actions::focus_element_raw(ptr),
                            )?;
                        }
                        if foreground_settle_ms > 0 {
                            std::thread::sleep(std::time::Duration::from_millis(
                                foreground_settle_ms,
                            ));
                        }
                        late_fg_cell_check(pid, Some(wid), late_fg_targeted)?;
                        if let Some((ptr, _)) = element_ptr_and_idx {
                            if !element_has_exact_app_focus(pid, wid, ptr) {
                                anyhow::bail!(
                                    "the retained receiver is not the application's focused element after Screen Sharing activation; no physical key was sent"
                                )
                            }
                        }
                        crate::input::keyboard::type_text_physical_global(text, delay_ms)
                    },
                )?;
                (
                    (
                        false,
                        None,
                        false,
                        crate::document_edit::DocumentEdited::NotADocument,
                    ),
                    true,
                )
            }
            Some(wid) if is_browser_pid(pid) => {
                // A browser needs a real activation, and its text focus kept.
                // A SkyLight front alone is not a Cocoa activation: the
                // omnibox suggestion window hands the front back to the
                // previous app and the rest of the string is lost. A Cocoa
                // activation re-installs the window's remembered first
                // responder (the page), so keys typed after it miss the
                // omnibox. "file:///…/archive-access.log" arrived as
                // "file://g" (bench CDB-G04, CUA-1223). So: remember the
                // focused field, activate exactly as bring_to_front does, wait
                // until the front holds, put focus back on the field, type,
                // then ask the previous app to take activation back. macOS may
                // refuse that from a background process and leave the browser
                // front, as it already did after a foreground omnibox Return.
                let previous_pid = apps::frontmost_pid().filter(|previous| *previous != pid);
                let remembered_guard = if retained_window_receiver {
                    element_ptr_and_idx.map(|(ptr, _)| unsafe {
                        crate::ax::snapshot::RetainedElement::retain(ptr)
                    })
                } else {
                    None
                };
                let remembered = match (&remembered_guard, element_ptr_and_idx) {
                    (Some(guard), _) => Some(guard.as_ptr() as AXUIElementRef),
                    (None, Some(_)) => None,
                    (None, None) => unsafe {
                        crate::ax::exact_target::focused_element_in_window(pid, wid)
                    },
                };
                // The caret or selection (cmd+L selects the whole URL) is
                // part of what activation resets: keep it too.
                let selection = remembered
                    .and_then(|element| unsafe { copy_raw_attr(element, SELECTED_RANGE) });
                super::bring_to_front::activate_exact_window_blocking(pid, wid);
                await_front_settled(pid, wid);
                let mut typed_delivery = (
                    false,
                    None,
                    false,
                    crate::document_edit::DocumentEdited::NotADocument,
                );
                let fronted =
                    crate::input::skylight::with_foreground_assist(pid as libc::pid_t, wid, || {
                        if let Some(element) = remembered {
                            let settable = unsafe {
                                crate::ax::write_outcome::query_settable(element, "AXFocused")
                            };
                            focus_write_with(
                                settable,
                                cua_driver_core::mutation::PriorInput::MayHaveDelivered,
                                || crate::input::ax_actions::focus_element_raw(element as usize),
                            )?;
                            if let Some(selection) = selection {
                                let settable = unsafe {
                                    crate::ax::write_outcome::query_settable(
                                        element,
                                        SELECTED_RANGE,
                                    )
                                };
                                restore_range_with(
                                    settable,
                                    cua_driver_core::mutation::PriorInput::MayHaveDelivered,
                                    || unsafe { set_raw_attr(element, SELECTED_RANGE, selection) },
                                )?;
                            }
                        }
                        late_fg_cell_check(pid, Some(wid), late_fg_targeted)?;
                        // The remembered field is focused again; the read-back
                        // follows the focused element.
                        typed_delivery = do_type(element_ptr_and_idx, remembered.is_some())?;
                        Ok(())
                    });
                unsafe {
                    if let Some(selection) = selection {
                        CFRelease(selection);
                    }
                    if remembered_guard.is_none() {
                        if let Some(element) = remembered {
                            CFRelease(element as core_foundation::base::CFTypeRef);
                        }
                    }
                }
                if let Some(previous_pid) = previous_pid {
                    super::bring_to_front::reactivate_application(previous_pid);
                }
                (typed_delivery, fronted?)
            }
            Some(wid) => {
                // Front → type → restore. The closure returns the read-back
                // result; with_foreground_assist returns whether it actually
                // fronted (Ok(false) when the fronting SPIs are unavailable —
                // the keystrokes still ran, just as background input).
                let mut typed_delivery = (
                    false,
                    None,
                    false,
                    crate::document_edit::DocumentEdited::NotADocument,
                );
                let fronted = crate::input::skylight::with_foreground_assist(
                    pid as libc::pid_t,
                    wid,
                    || {
                        late_fg_cell_check(pid, Some(wid), late_fg_targeted)?;
                        typed_delivery = do_type(element_ptr_and_idx, false)?;
                        Ok(())
                    },
                )?;
                (typed_delivery, fronted)
            }
            // No window to front — best-effort background keystrokes instead.
            None => (do_type(element_ptr_and_idx, false)?, false),
        };
        // Only claim the `_fg` path when a front actually happened; when no
        // foregrounding occurred (no window, or SPIs unavailable) these were
        // background keystrokes and `path` must say so honestly.
        return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
            detail: if ax_replacement {
                format!(" via native whole-field replacement{}", document.note())
            } else {
                format!(" via foreground keystrokes ({delay_ms}ms delay)")
            },
            path: if ax_replacement {
                PATH_AX
            } else if fronted {
                PATH_KEY_EVENTS_FG
            } else {
                PATH_KEY_EVENTS
            },
            delivered_chars: if before_withheld {
                None
            } else {
                delivered_chars
            },
            verified: verified && !before_withheld,
            web_area: classify_target_web_area(pid, element_ptr_and_idx, window_id),
        }));
    }

    // --- Background rung 0: terminal emulator → CGEvent only (AX is dropped). ---
    if is_terminal_target {
        // The semantic-only and synthesis-budget refusals already ran before
        // the read-back above.
        tracing::debug!(
            "type_text: pid {pid} is a terminal emulator; skipping AX value-set, \
             using CGEvent key-event synthesis"
        );
        let (verified, delivered_chars, _, _) = cgevent_type_verified(
            KeyReceiver {
                pid,
                window_id,
                element: element_ptr_and_idx,
            },
            KeyTextRequest {
                text,
                delay_ms,
                settle_ms: 0,
                replace,
                single_terminal_return: chromium_native_submission,
            },
            before.as_deref(),
            KeyFocusChecks {
                exact_focus: focused_key_route,
                recheck_editability_after_settle: false,
                prior,
                focus_already_written: false,
            },
        )?;
        return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
            detail: format!(" via CGEvent (terminal emulator, {delay_ms}ms delay)"),
            path: PATH_KEY_EVENTS,
            verified: verified && !before_withheld,
            delivered_chars: if before_withheld {
                None
            } else {
                delivered_chars
            },
            web_area: classify_target_web_area(pid, element_ptr_and_idx, window_id),
        }));
    }

    // --- Background rung 1: AX SelectedText write (element or focused). ---
    // Without an explicit element, a window-addressed request may only write
    // to the focused element when it provably belongs to the exact target
    // window — a sibling window's focused field is not the requested target.
    let ax_target: Option<(AXUIElementRef, bool, Option<usize>)> = match element_ptr_and_idx {
        Some((ptr, idx)) => Some((ptr as AXUIElementRef, /*owns=*/ false, idx)),
        None => match window_id {
            Some(wid) => unsafe { crate::ax::exact_target::focused_element_in_window(pid, wid) }
                .map(|el| (el, /*owns=*/ true, None)),
            None => {
                unsafe { focused_element_of_pid(pid) }.map(|el| (el, /*owns=*/ true, None))
            }
        },
    };
    // Untargeted background text goes to whatever the window has focused.
    // When that is not a text input (a Finder list, say), key events do not
    // type: they drive the view instead (Finder's type-to-select moved the
    // selection to another file, and the next Return renamed the wrong
    // item). Refuse before anything is sent.
    if !explicit_target && window_id.is_some() {
        let accepts_text = match ax_target {
            Some((element, _, _)) => unsafe {
                focused_accepts_text(element)
                    || focused_is_editor_surface(pid, element)
                    || target_in_web_area(pid, Some((element as usize, None)), window_id)
            },
            // No focused element, but the window is the confirmed key window:
            // post the keys to it, as press_key does.
            None => unfocused_window_accepts_keys(key_window_confirmed, &bundle_id_of(pid)),
        };
        if !accepts_text {
            if let Some((element, true, _)) = ax_target {
                unsafe { CFRelease(element as _) };
            }
            return Ok(TypeTextDelivery::Refused(no_focused_text_input_refusal(
                ax_target.is_some(),
            )));
        }
    }

    let ax_attempt = AxAttempt::NotAttempted;
    if force_key_events {
        if let Some((element, true, _)) = ax_target {
            unsafe { CFRelease(element as _) };
        }
    } else if let Some((element, owns, idx_opt)) = ax_target {
        let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
        let title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();
        let selection = if before_withheld {
            None
        } else {
            unsafe { selected_range(element) }
        };
        let bound = element_ptr_and_idx.is_some_and(|(ptr, _)| ptr == element as usize)
            && window_id.is_some_and(|wid| element_in_requested_window(pid, wid, element as usize));
        let settable =
            unsafe { crate::ax::write_outcome::query_settable(element, "AXSelectedText") };
        let insertion = insert_ax_write(
            before_withheld,
            TextWriteContext { settable, prior },
            || unsafe { set_string_attr(element, "AXSelectedText", text) },
            || {
                crate::ax::write_outcome::read_value_if_allowed(
                    &mut crate::ax::label_reader::NativeLabelReader,
                    element,
                )
            },
        );
        let insertion = match insertion {
            Ok(insertion) => insertion,
            Err(error) => {
                if owns {
                    unsafe { CFRelease(element as _) };
                }
                return Err(error.into());
            }
        };
        match insertion {
            InsertRead::Unsupported => {
                if owns {
                    unsafe { CFRelease(element as _) };
                }
            }
            InsertRead::Withheld { .. } => {
                let document =
                    unsafe { crate::document_edit::document_status_without_value(element) };
                let outcome_web_area =
                    classify_target_web_area(pid, Some((element as usize, idx_opt)), window_id);
                if owns {
                    unsafe { CFRelease(element as _) };
                }
                return Ok(TypeTextDelivery::Typed(withheld_ax_insert_outcome(
                    &role,
                    &title,
                    document,
                    outcome_web_area,
                )));
            }
            InsertRead::Readable {
                error: _,
                value: after,
            } => {
                let outcome_web_area =
                    classify_target_web_area(pid, Some((element as usize, idx_opt)), window_id);
                let web_untrusted = target_readback_is_untrusted(
                    outcome_web_area,
                    crate::browser::electron_js::ElectronJs::is_electron(pid),
                );
                let ax_progress = if web_untrusted {
                    TypedProgress::Unverifiable
                } else {
                    ax_insert_progress(before.as_deref(), after.as_deref(), text, selection, bound)
                };
                // An AX insert bypasses NSTextView's edit model, so a document is
                // not marked edited and would never save the new text (DS-07).
                let document = if ax_progress == TypedProgress::Complete {
                    unsafe {
                        crate::document_edit::ensure_marked_after_ax_write(pid, element, false)
                    }
                } else {
                    crate::document_edit::DocumentEdited::NotADocument
                };
                if owns {
                    unsafe {
                        CFRelease(element as _);
                    }
                }
                if ax_progress == TypedProgress::Complete {
                    let idx_str = idx_opt.map(|i| format!(" [{i}]")).unwrap_or_default();
                    return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
                        detail: format!(
                            " into{idx_str} {role} \"{title}\"{}",
                            document.note().trim_end_matches('.')
                        ),
                        path: PATH_AX,
                        verified: true,
                        delivered_chars: Some(text.chars().count()),
                        web_area: outcome_web_area,
                    }));
                }
                if let TypedProgress::Partial(delivered_chars) = ax_progress {
                    let idx_str = idx_opt.map(|i| format!(" [{i}]")).unwrap_or_default();
                    return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
                        detail: format!(" via partial AX write into{idx_str} {role} \"{title}\""),
                        path: PATH_AX,
                        verified: false,
                        delivered_chars: Some(delivered_chars),
                        web_area: outcome_web_area,
                    }));
                }
                return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
                    detail: format!(
                        " via AX write into {role} \"{title}\"; delivery could not be confirmed"
                    ),
                    path: PATH_AX,
                    verified: false,
                    delivered_chars: None,
                    web_area: outcome_web_area,
                }));
            }
        }
    } else {
        tracing::debug!("No focused element for pid {pid}; using CGEvent keystrokes");
    }

    // The semantic AX rung did not land and this request is restricted to it:
    // the process-scoped CGEvent rung could reach a sibling window, so return
    // the structured refusal instead of escalating.
    if let BackgroundKeyboardPolicy::SemanticOnly(refusal) = keyboard_policy {
        return Ok(TypeTextDelivery::Refused(refusal));
    }

    if let Some(refusal) = synthesis_preflight(
        TextDeliveryRoute::UnicodeSynthesis,
        text.chars().count(),
        delay_ms,
    ) {
        return Ok(TypeTextDelivery::SynthesisRefused {
            path: PATH_KEY_EVENTS,
            refusal,
            ax_attempt,
        });
    }

    // --- Background rung 2: CGEvent keystrokes with read-back. ---
    // Never clear here: a partial AX write is rare, and clearing would violate
    // insert-at-cursor semantics.
    let (verified, delivered_chars, ax_replacement, document) = cgevent_type_verified(
        KeyReceiver {
            pid,
            window_id,
            element: element_ptr_and_idx,
        },
        KeyTextRequest {
            text,
            delay_ms,
            settle_ms: 0,
            replace,
            single_terminal_return: chromium_native_submission,
        },
        before.as_deref(),
        KeyFocusChecks {
            exact_focus: focused_key_route,
            recheck_editability_after_settle: false,
            prior,
            focus_already_written: false,
        },
    )?;
    Ok(TypeTextDelivery::Typed(TypeTextOutcome {
        detail: if ax_replacement {
            format!(" via native whole-field replacement{}", document.note())
        } else {
            format!(" via CGEvent ({delay_ms}ms delay)")
        },
        path: if ax_replacement {
            PATH_AX
        } else {
            PATH_KEY_EVENTS
        },
        verified: verified && !before_withheld,
        delivered_chars: if before_withheld {
            None
        } else {
            delivered_chars
        },
        web_area: classify_target_web_area(pid, element_ptr_and_idx, window_id),
    }))
}

/// Whether a focused element takes typed text: a text role, or any element
/// that exposes a text selection (custom text views do).
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
pub(super) unsafe fn focused_accepts_text(element: AXUIElementRef) -> bool {
    let role = copy_string_attr(element, "AXRole").unwrap_or_default();
    is_text_input_role(&role)
        || copy_raw_attr(element, SELECTED_RANGE).is_some_and(|range| {
            CFRelease(range);
            true
        })
}

fn is_text_input_role(role: &str) -> bool {
    matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField"
    )
}

/// Roles of a focused element that is an editing surface without being a text
/// field: a spreadsheet grid or cell, a document canvas, the scroll view that
/// hosts one (LibreOffice Calc and Writer, Numbers, Pages). Keys typed into
/// them edit the document, so a pid-routed key post is the right route.
/// Lists and outlines stay out: there typed keys move the selection.
fn is_editor_surface_role(role: &str) -> bool {
    matches!(
        role,
        "AXLayoutArea" | "AXLayoutItem" | "AXScrollArea" | "AXTable" | "AXCell" | "AXGroup"
    )
}

/// Apps whose focused views of those roles are file or item browsers, where
/// typed keys drive type-to-select rather than edit.
/// A window with no focused element takes keys only when it is the confirmed
/// key window and the app is not an item browser.
fn unfocused_window_accepts_keys(key_window_confirmed: bool, bundle_id: &str) -> bool {
    key_window_confirmed && !is_item_browser_app(bundle_id)
}

fn bundle_id_of(pid: i32) -> String {
    crate::apps::bundle_id_for_pid(pid).unwrap_or_default()
}

fn is_item_browser_app(bundle_id: &str) -> bool {
    matches!(bundle_id, "com.apple.finder" | "com.apple.dock")
}

fn editor_surface_accepts_keys(role: &str, bundle_id: &str) -> bool {
    is_editor_surface_role(role) && !is_item_browser_app(bundle_id)
}

/// Whether the focused element is an editing surface that takes typed keys
/// even though it is not a text field (CUA-1767).
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
unsafe fn focused_is_editor_surface(pid: i32, element: AXUIElementRef) -> bool {
    let role = copy_string_attr(element, "AXRole").unwrap_or_default();
    if !is_editor_surface_role(&role) {
        return false;
    }
    let bundle_id = crate::apps::bundle_id_for_pid(pid).unwrap_or_default();
    editor_surface_accepts_keys(&role, &bundle_id)
}

const NO_FOCUSED_TEXT_INPUT: &str = "no_focused_text_input";

/// Recheck the focused element inside an activated foreground delivery
/// closure, before typing. Fails closed on a non-editing cell or an
/// unreadable focus.
fn late_fg_cell_check(pid: i32, window_id: Option<u32>, targeted: bool) -> anyhow::Result<()> {
    if targeted {
        return Ok(());
    }
    match unsafe { super::not_editable::classify_untargeted(pid, window_id) } {
        crate::ax::editable::CellVerdict::NotCell | crate::ax::editable::CellVerdict::Editing => {
            Ok(())
        }
        crate::ax::editable::CellVerdict::NotEditing => anyhow::bail!(
            "not_editable: the focused element after activation is a non-editing cell; no text              was sent"
        ),
        crate::ax::editable::CellVerdict::Unreadable => anyhow::bail!(
            "no_focused_text_input: the focused element after activation could not be read; no              text was sent"
        ),
    }
}

fn no_focused_text_input_refusal(
    something_focused: bool,
) -> cua_driver_core::background_input::BackgroundRefusal {
    let what = if something_focused {
        "the focused element in this window is not a text input"
    } else {
        "no element in this window has keyboard focus"
    };
    cua_driver_core::background_input::BackgroundRefusal {
        code: NO_FOCUSED_TEXT_INPUT,
        reason: format!(
            "{what}, so typed keys would act on the window instead (in Finder they move the \
             selection by type-to-select). Nothing was sent. Pass the field's element_token, or \
             x,y to focus it by pixel first. To rename a file in Finder, use set_value on its \
             name cell."
        ),
        advice: Some("px"),
    }
}

// ── Temporary restore of a minimized window (400-F1 wp2) ────────────────────

/// Whether a minimized window may be restored for one action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RestoreDecision {
    /// The window is visible: nothing to restore.
    NotNeeded,
    /// Proven minimized in a visible app: restore it for the action.
    Restore,
    /// The app is hidden, or a state is unknown: keep the existing refusal.
    Refuse,
}

pub(super) fn restore_decision(
    minimized: Option<bool>,
    app_hidden: Option<bool>,
) -> RestoreDecision {
    if app_hidden != Some(false) {
        return RestoreDecision::Refuse;
    }
    match minimized {
        Some(true) => RestoreDecision::Restore,
        Some(false) => RestoreDecision::NotNeeded,
        None => RestoreDecision::Refuse,
    }
}

#[cfg(test)]
mod selection_outcome_tests {
    use super::*;
    use crate::ax::bindings::{kAXErrorAPIDisabled, kAXErrorCannotComplete};
    use crate::ax::write_outcome::Settable;
    use cua_driver_core::mutation::{Postcondition, PriorInput};
    use std::cell::Cell;

    #[test]
    fn focus_error_stops_before_range_and_preserves_prior() {
        let calls = Cell::new(0);
        let error = focus_write_with(Settable::Yes, PriorInput::None, || {
            calls.set(calls.get() + 1);
            kAXErrorCannotComplete
        })
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert_eq!(
            error
                .chain
                .error_disposition(Postcondition::Indeterminate, false),
            Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
        );
        let denied = focus_write_with(Settable::Yes, PriorInput::MayHaveDelivered, || {
            kAXErrorAPIDisabled
        })
        .unwrap_err();
        assert!(!denied.chain.no_input());
        assert_eq!(
            denied
                .chain
                .error_disposition(Postcondition::Indeterminate, false),
            Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
        );
        assert!(focus_write_with(Settable::Yes, PriorInput::None, || kAXErrorSuccess).is_ok());
        let range_error = restore_range_with(Settable::Yes, PriorInput::MayHaveDelivered, || {
            kAXErrorCannotComplete
        })
        .unwrap_err();
        assert_eq!(
            range_error
                .chain
                .error_disposition(Postcondition::Indeterminate, false),
            Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
        );
        assert_eq!(
            prior_for_receipt(Some(super::super::not_editable::FocusDispatch::SentAx)),
            PriorInput::MayHaveDelivered
        );
        assert_eq!(
            prior_for_key_entry(
                PriorInput::None,
                crate::input::skylight::KeyEntry {
                    records_uncertain: true,
                    ..Default::default()
                }
            ),
            PriorInput::MayHaveDelivered
        );
        let scope = scope_entry_failure(
            crate::input::skylight::ScopeEntryError {
                reason: "unconfirmed".into(),
                entry: crate::input::skylight::KeyEntry {
                    records_uncertain: true,
                    ..Default::default()
                },
                hand_back: Some(crate::input::skylight::HandBack::Deferred),
            },
            PriorInput::None,
        );
        let result = text_delivery_failure("type_text", &scope.into());
        assert_eq!(result.structured_content.unwrap()["effect"], "unverifiable");
        assert_eq!(
            result.disposition,
            Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
        );
        let scope = scope_entry_failure(
            crate::input::skylight::ScopeEntryError {
                reason: "unconfirmed".into(),
                entry: Default::default(),
                hand_back: None,
            },
            PriorInput::None,
        );
        let result = text_delivery_failure("type_text", &scope.into());
        assert_eq!(result.structured_content.unwrap()["effect"], "refused");
        assert_eq!(
            result.disposition,
            Some(cua_driver_contract::ErrorDisposition::NOT_DELIVERED)
        );
    }

    #[test]
    fn select_all_keeps_preflight_allocation_write_and_confirmation_distinct() {
        let calls = Cell::new(0);
        let absent = select_entire_with(
            Settable::No,
            PriorInput::None,
            || panic!("unsupported must not allocate"),
            || panic!("unsupported must not write"),
            || panic!("unsupported must not confirm"),
        );
        assert!(matches!(absent, FullSelection::Unsupported));
        let alloc = select_entire_with(
            Settable::Yes,
            PriorInput::None,
            || false,
            || panic!("allocation failed"),
            || panic!("allocation failed"),
        );
        let FullSelection::AllocationFailed(chain) = alloc else {
            panic!("allocation failure was lost");
        };
        assert!(chain.no_input());
        for error in [kAXErrorCannotComplete, kAXErrorAPIDisabled] {
            let result = select_entire_with(
                Settable::Yes,
                PriorInput::MayHaveDelivered,
                || true,
                || {
                    calls.set(calls.get() + 1);
                    error
                },
                || panic!("failed write must not be confirmed"),
            );
            let FullSelection::NativeFailed(raw, chain) = result else {
                panic!("native error was lost");
            };
            assert_eq!(raw, error);
            assert_eq!(
                chain.error_disposition(Postcondition::Indeterminate, false),
                Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
            );
        }
        assert_eq!(calls.get(), 2);
        let result = select_entire_with(
            Settable::Yes,
            PriorInput::None,
            || true,
            || kAXErrorSuccess,
            || false,
        );
        let FullSelection::Unconfirmed(chain) = result else {
            panic!("confirmation failure was lost");
        };
        assert_eq!(
            chain.error_disposition(Postcondition::Indeterminate, false),
            Some(cua_driver_contract::ErrorDisposition::DELIVERED)
        );
        assert!(matches!(
            select_entire_with(
                Settable::Yes,
                PriorInput::None,
                || true,
                || kAXErrorSuccess,
                || true
            ),
            FullSelection::Selected
        ));
    }
}

#[cfg(test)]
mod u32_outcome_tests {
    use super::*;
    use crate::ax::bindings::{kAXErrorCannotComplete, kAXErrorIllegalArgument};
    use crate::ax::write_outcome::Settable;
    use crate::ax::write_outcome::{ValueObs, ValueRead};
    use crate::document_edit::DocumentEdited;
    use cua_driver_core::mutation::PriorInput;
    use std::cell::Cell;

    #[test]
    fn replace_axvalue_timeout_with_matching_readback_sends_no_document_keys() {
        let marks = Cell::new(0);
        let result = replace_ax_with(
            "AXValue",
            "old",
            "new",
            TextWriteContext {
                settable: Settable::Yes,
                prior: PriorInput::MayHaveDelivered,
            },
            || kAXErrorCannotComplete,
            || {
                marks.set(marks.get() + 1);
                DocumentEdited::NotMarked
            },
            || panic!("late matching readback must not run after timeout"),
        );
        assert_eq!(marks.get(), 0, "uncertain write must not mark the document");
        let error = result.unwrap_err();
        let projected = text_delivery_failure("type_text failed", &error);
        assert_eq!(
            projected.structured_content.unwrap()["effect"],
            "unverifiable"
        );
        assert_eq!(
            projected.disposition,
            Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
        );
    }

    #[test]
    fn replace_selected_text_illegal_argument_is_sent_unconfirmed() {
        let marks = Cell::new(0);
        let result = replace_ax_with(
            "AXSelectedText",
            "old",
            "new",
            TextWriteContext {
                settable: Settable::Yes,
                prior: PriorInput::MayHaveDelivered,
            },
            || kAXErrorIllegalArgument,
            || {
                marks.set(marks.get() + 1);
                DocumentEdited::NotMarked
            },
            || panic!("illegal argument must not be promoted by readback"),
        );
        let error = result.unwrap_err();
        assert!(error
            .to_string()
            .contains(&kAXErrorIllegalArgument.to_string()));
        let projected = text_delivery_failure("type_text failed", &error);
        assert_eq!(
            projected.structured_content.unwrap()["effect"],
            "unverifiable"
        );
        assert_eq!(
            projected.disposition,
            Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
        );
        assert_eq!(marks.get(), 0);
        let (_, _, document) = replace_ax_with(
            "AXSelectedText",
            "old",
            "new",
            TextWriteContext {
                settable: Settable::Yes,
                prior: PriorInput::MayHaveDelivered,
            },
            || kAXErrorSuccess,
            || DocumentEdited::NotMarked,
            || (true, Some(3)),
        )
        .unwrap();
        assert!(document.note().contains("may keep the old content"));
        let projected = type_text_outcome_result(
            TypeTextOutcome {
                detail: format!(" via native whole-field replacement{}", document.note()),
                path: PATH_AX,
                verified: true,
                delivered_chars: Some(3),
                web_area: WebAreaClassification::NonWebContent,
            },
            3,
            false,
            false,
            "",
        );
        assert!(projected.content.iter().any(|item| matches!(item, cua_driver_core::protocol::Content::Text { text, .. } if text.contains("may keep the old content"))));
    }

    #[test]
    fn insert_timeout_never_falls_through_to_keys() {
        let insertion = insert_ax_write(
            false,
            TextWriteContext {
                settable: Settable::Yes,
                prior: PriorInput::MayHaveDelivered,
            },
            || kAXErrorCannotComplete,
            || ValueRead::Allowed(ValueObs::Text("old".into())),
        );
        let keys = Cell::new(0);
        if matches!(&insertion, Ok(InsertRead::Unsupported)) {
            keys.set(keys.get() + 1);
        }
        assert_eq!(keys.get(), 0, "a timed-out insertion must not post keys");
        let error = insertion.err().expect("timeout must remain an error");
        let projected = text_delivery_failure("type_text failed", &error.into());
        assert_eq!(
            projected.structured_content.unwrap()["effect"],
            "unverifiable"
        );
        assert_eq!(
            projected.disposition,
            Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
        );
        let withheld =
            send_ax_insert_and_read(false, || kAXErrorCannotComplete, || ValueRead::Withheld);
        assert!(
            matches!(withheld, InsertRead::Withheld { error } if error == kAXErrorCannotComplete)
        );
        for observation in [
            ValueRead::Withheld,
            ValueRead::Allowed(ValueObs::Text("old".into())),
        ] {
            let writes = Cell::new(0);
            let result = insert_ax_write(
                false,
                TextWriteContext {
                    settable: Settable::Yes,
                    prior: PriorInput::None,
                },
                || {
                    writes.set(writes.get() + 1);
                    kAXErrorSuccess
                },
                || observation,
            );
            assert!(matches!(
                result,
                Ok(InsertRead::Withheld { .. } | InsertRead::Readable { .. })
            ));
            assert_eq!(writes.get(), 1, "a delivered write must never be resent");
        }
        let withheld_error = insert_ax_write(
            false,
            TextWriteContext {
                settable: Settable::Yes,
                prior: PriorInput::None,
            },
            || kAXErrorCannotComplete,
            || ValueRead::Withheld,
        )
        .err()
        .expect("withheld timeout must remain an error");
        assert!(withheld_error
            .message
            .contains(&kAXErrorCannotComplete.to_string()));
        let unsupported = insert_ax_write(
            false,
            TextWriteContext {
                settable: Settable::No,
                prior: PriorInput::None,
            },
            || panic!("unsupported setter must not run"),
            || panic!("unsupported setter must not read"),
        );
        assert!(matches!(unsupported, Ok(InsertRead::Unsupported)));
    }

    #[test]
    fn utf16_selected_splice_requires_exact_bound_target() {
        let progress = ax_insert_progress(Some("😀old"), Some("😀oldx"), "😀", Some((2, 0)), false);
        assert_eq!(
            progress,
            TypedProgress::Unverifiable,
            "a pre-existing substring is not an inserted splice"
        );
        assert_eq!(
            ax_insert_progress(Some("😀old"), Some("😀new"), "new", Some((2, 3)), true),
            TypedProgress::Complete
        );
        assert_eq!(
            ax_insert_progress(Some("😀old"), Some("😀ne"), "new", Some((2, 3)), true),
            TypedProgress::Partial(2)
        );
        assert_eq!(
            ax_insert_progress(Some("😀old"), Some("😀new"), "new", Some((1, 1)), true),
            TypedProgress::Unverifiable
        );
        assert_eq!(
            ax_insert_progress(
                Some("😀old"),
                Some("😀new"),
                "new",
                Some((usize::MAX, 2)),
                true
            ),
            TypedProgress::Unverifiable
        );
        assert_eq!(
            ax_insert_progress(Some("😀old"), Some("😀new"), "new", None, true),
            TypedProgress::Unverifiable
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_request_naming_a_sheet_needs_the_sheet_as_key_window() {
        use super::key_window_matches;
        // Sheet 900 attached to window 700; its elements map to 700.
        assert!(key_window_matches(Some(900), 900, || 700, || true));
        assert!(!key_window_matches(Some(700), 900, || 700, || true));
        assert!(!key_window_matches(None, 900, || 700, || true));
        // A sheet token addressed through the parent keeps working.
        assert!(key_window_matches(Some(700), 700, || 700, || false));
        assert!(key_window_matches(Some(700), 701, || 700, || false));
        assert!(!key_window_matches(Some(888), 701, || 700, || false));
    }

    /// DSH-03: a paste that may have landed is not reported as a plain
    /// failure an agent would answer by typing the text again.
    #[test]
    fn a_paste_that_may_have_landed_is_reported_unverifiable() {
        let mut pb_error = None;
        let _ = crate::input::keyboard::run_pasteboard_transaction(
            &mut FailingRestore::default(),
            "text",
            || Ok(()),
            std::time::Duration::ZERO,
        )
        .map_err(|error| pb_error = Some(error));
        let error = pb_error.expect("the restore failed");
        let result = super::text_delivery_failure("type_text failed", &error);
        assert_eq!(result.is_error, Some(true));
        let data = result.structured_content.expect("structured failure");
        assert_eq!(data["code"], "paste_may_have_landed");
        assert_eq!(data["effect"], "unverifiable");
        assert_eq!(data["clipboard_restored"], false);

        let plain = super::text_delivery_failure("type_text failed", &anyhow::anyhow!("no source"));
        assert_eq!(plain.is_error, Some(true));
        assert!(plain.structured_content.is_none());
    }

    #[derive(Default)]
    struct FailingRestore {
        count: isize,
    }

    impl crate::input::keyboard::PasteboardBackend for FailingRestore {
        fn change_count(&self) -> isize {
            self.count
        }
        fn snapshot_items(
            &self,
        ) -> anyhow::Result<Vec<crate::input::keyboard::PasteboardItemSnapshot>> {
            Ok(Vec::new())
        }
        fn write_items(
            &mut self,
            _: &[crate::input::keyboard::PasteboardItemSnapshot],
        ) -> anyhow::Result<isize> {
            self.count += 1;
            Ok(self.count)
        }
        fn clear(&mut self) -> anyhow::Result<isize> {
            anyhow::bail!("clear failed")
        }
    }

    #[test]
    fn unfocused_window_needs_a_confirmed_key_window() {
        assert!(super::unfocused_window_accepts_keys(
            true,
            "org.libreoffice.script"
        ));
        assert!(!super::unfocused_window_accepts_keys(
            false,
            "org.libreoffice.script"
        ));
        assert!(!super::unfocused_window_accepts_keys(
            true,
            "com.apple.finder"
        ));
    }

    #[test]
    fn editor_surfaces_take_keys_but_item_browsers_do_not() {
        for role in ["AXLayoutArea", "AXScrollArea", "AXTable", "AXCell"] {
            assert!(super::editor_surface_accepts_keys(
                role,
                "org.libreoffice.script"
            ));
            assert!(super::editor_surface_accepts_keys(
                role,
                "com.apple.iWork.Numbers"
            ));
            assert!(!super::editor_surface_accepts_keys(
                role,
                "com.apple.finder"
            ));
        }
        for role in ["AXOutline", "AXList", "AXRow", "AXButton", "AXMenuItem", ""] {
            assert!(!super::editor_surface_accepts_keys(
                role,
                "org.libreoffice.script"
            ));
        }
    }

    #[test]
    fn chromium_native_submission_allows_only_one_terminal_newline() {
        assert!(valid_chromium_native_submission("example.com\n"));
        assert!(valid_chromium_native_submission("example.com\r"));
        assert!(valid_chromium_native_submission("example.com\r\n"));
        assert!(valid_chromium_native_submission("\n"));
        assert!(!valid_chromium_native_submission("example.com\nnext"));
        assert!(!valid_chromium_native_submission("example.com\n\n"));
        assert!(!valid_chromium_native_submission("example.com\t"));
        assert!(!valid_chromium_native_submission("example.com"));
    }

    #[test]
    fn browser_identities_take_the_activating_rung() {
        for id in [
            "com.google.Chrome",
            "com.google.Chrome.canary",
            "org.chromium.Chromium",
            "com.brave.Browser",
            "com.microsoft.edgemac",
            "com.vivaldi.Vivaldi",
        ] {
            assert!(super::is_chromium_identity(id), "{id}");
        }
        for id in [
            "org.libreoffice.script",
            "com.apple.TextEdit",
            "com.tinyspeck.slackmacgap",
            "",
        ] {
            assert!(!super::is_chromium_identity(id), "{id}");
        }
    }

    use super::*;

    /// A semantic-only policy must refuse the terminal short-circuit before
    /// any CGEvent is posted: terminals have no semantic AX rung, so nothing
    /// safe remains and the carried refusal comes back unchanged.
    #[test]
    fn semantic_only_policy_refuses_terminal_cgevent_rung() {
        let refusal = BackgroundRefusal {
            code: cua_driver_core::background_input::refusal_codes::SAME_PID_KEYBOARD_AMBIGUITY,
            reason: "test".into(),
            advice: None,
        };
        let r = type_text_blocking(
            -1,
            "x",
            None,
            0,
            /*is_terminal_target=*/ true,
            KeyboardRoute {
                delivery_mode: super::super::DeliveryMode::Background,
                window_id: Some(7),
                keyboard_policy: BackgroundKeyboardPolicy::SemanticOnly(refusal.clone()),
                explicit_target: false,
                key_window_confirmed: false,
                replace: false,
                force_key_events: false,
                chromium_native_candidate: false,
                focused_key_route: false,
                prior: cua_driver_core::mutation::PriorInput::None,
            },
        );
        match r {
            Ok(TypeTextDelivery::Refused(returned)) => assert_eq!(returned, refusal),
            other => panic!("expected a structured refusal, got {:?}", other.is_ok()),
        }
    }

    /// Untargeted background text with nothing text-like focused in the
    /// window is refused before any key event (Finder type-to-select).
    #[test]
    #[ignore = "live: reads accessibility IPC; runs alone in the macOS live lane"]
    fn untargeted_text_without_a_focused_text_input_is_refused() {
        let result = type_text_in_key_window(
            -1,
            "renamed.txt",
            None,
            0,
            /*is_terminal_target=*/ false,
            KeyboardRoute {
                delivery_mode: super::super::DeliveryMode::Background,
                window_id: Some(7),
                keyboard_policy: BackgroundKeyboardPolicy::Allowed,
                explicit_target: false,
                key_window_confirmed: false,
                replace: false,
                force_key_events: false,
                chromium_native_candidate: false,
                focused_key_route: false,
                prior: cua_driver_core::mutation::PriorInput::None,
            },
        )
        .expect("the refusal is a delivery outcome");
        match result {
            TypeTextDelivery::Refused(refusal) => {
                assert_eq!(refusal.code, NO_FOCUSED_TEXT_INPUT);
                assert!(refusal.reason.contains("Nothing was sent"));
                assert_eq!(refusal.advice, Some("px"));
            }
            _ => panic!("expected the no_focused_text_input refusal"),
        }
    }

    #[test]
    fn text_roles_take_typed_text() {
        for role in ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"] {
            assert!(is_text_input_role(role), "{role}");
        }
        for role in ["AXOutline", "AXList", "AXRow", "AXWindow", "AXGroup", ""] {
            assert!(!is_text_input_role(role), "{role}");
        }
        assert!(no_focused_text_input_refusal(true)
            .reason
            .starts_with("the focused element in this window is not a text input"));
        assert!(no_focused_text_input_refusal(false)
            .reason
            .starts_with("no element in this window has keyboard focus"));
    }

    #[test]
    fn oversized_synthesis_is_refused_before_the_terminal_event_path() {
        let text = "x".repeat(6_500);
        let result = type_text_blocking(
            -1,
            &text,
            None,
            0,
            /*is_terminal_target=*/ true,
            KeyboardRoute {
                delivery_mode: super::super::DeliveryMode::Background,
                window_id: None,
                keyboard_policy: BackgroundKeyboardPolicy::Allowed,
                explicit_target: false,
                key_window_confirmed: false,
                replace: false,
                force_key_events: false,
                chromium_native_candidate: false,
                focused_key_route: false,
                prior: cua_driver_core::mutation::PriorInput::None,
            },
        )
        .expect("preflight refusal must not attempt the invalid pid");
        let TypeTextDelivery::SynthesisRefused {
            path,
            refusal,
            ax_attempt,
        } = result
        else {
            panic!("oversized terminal synthesis must fail before mutation");
        };
        assert_eq!(path, PATH_KEY_EVENTS);
        assert_eq!(ax_attempt, AxAttempt::NotAttempted);
        assert_eq!(refusal.requested_chars, 6_500);
        assert_eq!(refusal.estimated_duration_ms, 106_000);
        assert_eq!(refusal.max_chunk_chars, 6_125);
    }

    #[test]
    fn synthesis_preflight_accepts_the_exact_transport_safe_boundary() {
        assert!(synthesis_preflight(TextDeliveryRoute::UnicodeSynthesis, 6_125, 0).is_none());
        assert!(synthesis_preflight(TextDeliveryRoute::UnicodeSynthesis, 6_126, 0).is_some());
    }

    #[test]
    fn large_atomic_ax_payloads_are_not_subject_to_the_synthesis_budget() {
        assert!(synthesis_preflight(TextDeliveryRoute::AtomicAx, 100_000, 200).is_none());
        assert_eq!(
            typed_progress(None, Some(&"x".repeat(11_500)), &"x".repeat(11_500)),
            TypedProgress::Complete,
            "a successful one-call AX insertion remains eligible regardless of size"
        );
    }

    #[test]
    fn refusal_diagnostics_distinguish_safe_chunking_from_indeterminate_ax() {
        let refusal = synthesis_preflight(TextDeliveryRoute::UnicodeSynthesis, 6_500, 0)
            .expect("payload must exceed the synthesis budget");
        let safe = synthesis_refusal_result(PATH_KEY_EVENTS, &refusal, AxAttempt::Rejected);
        let safe = safe.structured_content.expect("structured refusal");
        assert_eq!(safe["code"], "type_text_synthesis_budget_exceeded");
        assert_eq!(safe["effect"], "refused");
        assert_eq!(safe["delivered_chars"], 0);
        assert_eq!(safe["synthesized_chars"], 0);
        assert_eq!(safe["retryable"], true);
        assert_eq!(safe["escalation"]["recommended"], "chunk");

        let indeterminate =
            synthesis_refusal_result(PATH_KEY_EVENTS, &refusal, AxAttempt::Unverifiable);
        let indeterminate = indeterminate
            .structured_content
            .expect("structured indeterminate result");
        assert_eq!(indeterminate["effect"], "indeterminate");
        assert!(indeterminate.get("delivered_chars").is_none());
        assert_eq!(indeterminate["synthesized_chars"], 0);
        assert_eq!(indeterminate["retryable"], false);
        assert_eq!(indeterminate["escalation"]["recommended"], "verify_state");
    }

    #[test]
    fn typed_progress_classifies_readback() {
        use TypedProgress::*;
        for (before, after, text, expected) in [
            // Catalyst: can't read AXValue back, so delivery cannot be confirmed.
            (None, None, "hi", Unverifiable),
            (Some(""), None, "hi", Unverifiable),
            (None, Some("h"), "hi", Unverifiable),
            (Some(""), Some("hi"), "hi", Complete),
            (Some("ab"), Some("ab hi"), "hi", Complete),
            // An observable prefix is partial delivery, never completion.
            (
                Some(""),
                Some("BEGINpayload"),
                "BEGINpayloadEND",
                Partial(12),
            ),
            (Some("ab"), Some("ab"), "hi", Unchanged),
            (None, None, "", Complete),
            // A submitted field rewrites itself; the shortfall proves nothing.
            (Some("A1"), Some("A1xx"), "$Controls.A1\n", Unverifiable),
            (Some(""), Some("one\ntwo"), "one\ntwo", Complete),
        ] {
            assert_eq!(
                typed_progress(before, after, text),
                expected,
                "before={before:?} after={after:?} text={text:?}"
            );
        }
    }

    #[test]
    fn delivery_waits_through_a_partial_readback_until_complete() {
        let text = "BEGIN-payload-END";
        let mut values = std::collections::VecDeque::from([
            Some("BEGIN-payload".to_owned()),
            Some(text.to_owned()),
        ]);
        let mut reads = 0;
        let delivery = await_typed_delivery(
            Some(""),
            text,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            || {
                reads += 1;
                values.pop_front().flatten()
            },
            || false,
        );
        assert_eq!(delivery, (true, Some(text.chars().count())));
        assert_eq!(reads, 2, "completion must wait past the prefix readback");
    }

    #[test]
    fn drained_prefix_reports_the_delivered_character_count() {
        let delivery = await_typed_delivery(
            Some(""),
            "BEGIN-payload-END",
            std::time::Instant::now(),
            || Some("BEGIN".to_owned()),
            || false,
        );
        assert_eq!(delivery, (false, Some(5)));
    }

    #[test]
    fn delivery_unchanged_web_is_unverifiable() {
        for web in [true, false] {
            assert_eq!(
                await_typed_delivery(
                    Some(""),
                    "hello",
                    std::time::Instant::now(),
                    || Some(String::new()),
                    || web,
                ),
                (false, if web { None } else { Some(0) }),
            );
        }
    }

    #[test]
    fn delivery_web_keeps_polling_but_never_returns_a_retryable_partial_count() {
        for complete in [true, false] {
            let mut reads = 0;
            let result = await_typed_delivery(
                Some(""),
                "hello",
                std::time::Instant::now() + std::time::Duration::from_millis(30),
                || {
                    reads += 1;
                    Some(
                        if reads == 1 {
                            ""
                        } else if complete {
                            "hello"
                        } else {
                            "he"
                        }
                        .to_owned(),
                    )
                },
                || true,
            );
            assert!(reads >= 2);
            assert_eq!(
                result,
                if complete {
                    (true, Some(5))
                } else {
                    (false, None)
                }
            );
        }
    }

    #[test]
    fn delivery_unreadable_does_not_probe_ancestry() {
        assert_eq!(
            await_typed_delivery(
                Some(""),
                "hello",
                std::time::Instant::now(),
                || None,
                || panic!("unreadable values must not probe ancestry"),
            ),
            (false, None),
        );
    }

    #[test]
    fn path_constants_are_stable_tokens() {
        // These string constants are part of the structured-response
        // contract; freezing them here makes the contract a unit test.
        assert_eq!(PATH_AX, "ax");
        assert_eq!(PATH_KEY_EVENTS, "key_events");
        assert_eq!(PATH_KEY_EVENTS_FG, "key_events_fg");
    }

    #[test]
    fn ax_backed_web_readbacks_are_downgraded() {
        for path in [PATH_AX, PATH_KEY_EVENTS, PATH_KEY_EVENTS_FG] {
            assert_eq!(
                surface_verification(path, true, true),
                SurfaceVerification {
                    verified: false,
                    untrusted_web_readback: true,
                },
                "path={path}"
            );
        }
    }

    #[test]
    fn web_readback_distrust_preserves_native_and_unverified_outcomes() {
        assert_eq!(
            surface_verification(PATH_KEY_EVENTS_FG, true, false),
            SurfaceVerification {
                verified: true,
                untrusted_web_readback: false,
            },
            "native browser chrome remains eligible for trusted read-back"
        );
        assert_eq!(
            surface_verification(PATH_KEY_EVENTS_FG, false, true),
            SurfaceVerification {
                verified: false,
                untrusted_web_readback: false,
            },
            "an already-unverified delivery is not reclassified as a web echo"
        );
        assert_eq!(
            surface_verification("independent_renderer_oracle", true, true),
            SurfaceVerification {
                verified: true,
                untrusted_web_readback: false,
            },
            "a future independently verified path must not inherit AXValue distrust"
        );
    }

    #[test]
    fn web_readback_escalation_never_recommends_the_completed_pixel_rung() {
        assert_eq!(web_readback_next_rung(true, false), Some("px"));
        assert_eq!(web_readback_next_rung(true, true), None);
        assert_eq!(web_readback_next_rung(false, false), Some("page"));
        assert_eq!(web_readback_next_rung(false, true), Some("page"));
    }

    #[test]
    fn electron_incomplete_and_web_readbacks_are_never_native_confirmation() {
        for classification in [
            WebAreaClassification::WebContent,
            WebAreaClassification::Incomplete,
            WebAreaClassification::WindowFocusUnavailable,
        ] {
            assert!(target_readback_is_untrusted(classification, true));
        }
        assert!(!target_readback_is_untrusted(
            WebAreaClassification::NonWebContent,
            true
        ));
        assert!(target_readback_is_untrusted(
            WebAreaClassification::Incomplete,
            false
        ));
        assert!(target_readback_is_untrusted(
            WebAreaClassification::WebContent,
            false
        ));
    }

    #[test]
    fn replacement_readback_requires_exact_value_not_substring_growth() {
        let now = std::time::Instant::now();
        assert_eq!(
            await_replacement_delivery("new", now, || Some("old-new-old".to_owned())),
            (false, None)
        );
        assert_eq!(
            await_replacement_delivery("new", now, || Some("new".to_owned())),
            (true, Some(3))
        );
        assert_eq!(
            await_replacement_delivery("", now, || Some("still here".to_owned())),
            (false, None)
        );
        assert_eq!(
            await_replacement_delivery("한글✓😀", now, || Some("한글✓😀".to_owned())),
            (true, Some(4))
        );
    }

    #[test]
    fn replacement_accepts_only_editable_non_secure_text_roles() {
        for role in ["AXTextField", "AXTextArea", "AXSearchField", "AXComboBox"] {
            assert!(replacement_text_role_allowed(role, ""), "{role}");
        }
        assert!(!replacement_text_role_allowed("AXSecureTextField", ""));
        assert!(!replacement_text_role_allowed(
            "AXTextField",
            "AXSecureTextField"
        ));
        assert!(!replacement_text_role_allowed("AXButton", ""));
        assert!(!replacement_text_role_allowed("AXStaticText", ""));
    }

    #[test]
    fn foreground_typing_skips_long_rearm_when_target_is_already_frontmost() {
        assert_eq!(foreground_settle_ms(42, Some(42)), 20);
        assert_eq!(foreground_settle_ms(42, Some(7)), 200);
        assert_eq!(foreground_settle_ms(42, None), 200);
    }

    #[test]
    fn screen_sharing_text_fails_closed_without_foreground_window() {
        for (foreground, window_id) in [(false, None), (false, Some(7)), (true, None)] {
            let result = screen_sharing_delivery_error(true, foreground, window_id)
                .expect("unsafe Screen Sharing route must be refused");
            assert_eq!(result.is_error, Some(true));
            let structured = result.structured_content.unwrap();
            assert_eq!(structured["code"], "SCREEN_SHARING_REQUIRES_FOREGROUND_HID");
            assert_eq!(structured["effect"], "refused");
            assert_eq!(structured["escalation"]["recommended"], "foreground");
            assert_eq!(structured["escalation"]["requires"][0], "window_id");
        }
        assert!(screen_sharing_delivery_error(true, true, Some(7)).is_none());
        assert!(screen_sharing_delivery_error(false, false, None).is_none());
    }
}

#[cfg(test)]
mod secure_read_tests {
    use super::*;
    use crate::ax::label_reader::fake::FakeReader;
    use crate::ax::write_outcome::{read_text_value_with, ValueRead};

    #[test]
    fn type_text_replace_requests_no_value_before_classification() {
        let mut reader = FakeReader::default();
        reader.set(0, "AXRole", "AXSecureTextField");
        reader.set(0, "AXValue", "secret");
        let _before = read_text_value_with(&mut reader, 0);
        assert!(!reader.trace.iter().any(|(_, name)| name == "AXValue"));
    }

    #[test]
    fn type_text_preflight_withheld_reports_nothing_sent() {
        let result = replacement_readable(true, &ValueRead::Withheld);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no replacement payload was sent"));
        assert!(replacement_readable(false, &ValueRead::Withheld).is_ok());
    }

    #[test]
    fn type_text_withheld_in_a_document_keeps_the_save_warning() {
        let outcome = withheld_ax_insert_outcome(
            "AXTextArea",
            "",
            crate::document_edit::DocumentEdited::NotMarked,
            WebAreaClassification::NonWebContent,
        );
        assert!(outcome.detail.contains("may keep the old content"));
        assert!(!outcome.verified);
        assert_eq!(outcome.delivered_chars, None);
        assert_eq!(outcome.path, PATH_AX);
        let result = type_text_outcome_result(outcome, 3, false, false, "");
        assert!(result.content.iter().any(|content| matches!(content, cua_driver_core::protocol::Content::Text { text, .. } if text.contains("may keep the old content"))));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["verified"], false);
    }

    #[test]
    fn withheld_verification_sends_no_duplicate_insert_or_document_keys() {
        let sent = std::cell::RefCell::new(Vec::new());
        let result = send_ax_insert_and_read(
            false,
            || {
                sent.borrow_mut().push("insert");
                0
            },
            || ValueRead::Withheld,
        );
        assert!(matches!(result, InsertRead::Withheld { error } if error == kAXErrorSuccess));
        assert_eq!(*sent.borrow(), ["insert"]);
    }
}

#[cfg(test)]
mod minimized_restore_tests {
    use super::*;

    #[test]
    fn restore_needs_a_visible_app_and_a_proven_minimized_window() {
        assert_eq!(
            restore_decision(Some(true), Some(false)),
            RestoreDecision::Restore
        );
        assert_eq!(
            restore_decision(Some(false), Some(false)),
            RestoreDecision::NotNeeded
        );
        assert_eq!(restore_decision(None, Some(false)), RestoreDecision::Refuse);
        assert_eq!(
            restore_decision(Some(true), Some(true)),
            RestoreDecision::Refuse,
            "hidden app"
        );
        assert_eq!(
            restore_decision(Some(true), None),
            RestoreDecision::Refuse,
            "unknown"
        );
    }
}
