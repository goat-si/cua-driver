use crate::goat::action_latency as latency;

use async_trait::async_trait;
use core_foundation::base::CFRelease;
use cua_driver_contract::PressKeyInput;
use cua_driver_core::{
    action_record::{
        ActionEffect, ActionEvidence, ActionExecutionRecord, ActionTransport, ActualDelivery,
        EvidenceKind, RequestedDelivery,
    },
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use libc;
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{copy_bool_attr, copy_string_attr, AXUIElementRef};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;

use super::ToolState;

pub struct PressKeyTool {
    state: Arc<ToolState>,
}

impl PressKeyTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[derive(Debug)]
enum PressKeyDeliveryOutcome {
    Confirmed,
    Unverifiable,
    Failed(anyhow::Error),
}

fn map_delivery_outcome(result: anyhow::Result<bool>) -> PressKeyDeliveryOutcome {
    match result {
        Ok(true) => PressKeyDeliveryOutcome::Confirmed,
        Ok(false) => PressKeyDeliveryOutcome::Unverifiable,
        Err(error) => PressKeyDeliveryOutcome::Failed(error),
    }
}

fn semantic_action_for_request(key: &str, modifiers: &[String]) -> Option<&'static str> {
    if !modifiers.is_empty() {
        return None;
    }
    match key {
        "return" | "enter" => Some("AXConfirm"),
        "escape" => Some("AXCancel"),
        _ => None,
    }
}

fn advertised_semantic_action(
    key: &str,
    modifiers: &[String],
    advertised: &[String],
) -> Option<&'static str> {
    let action = semantic_action_for_request(key, modifiers)?;
    advertised
        .iter()
        .any(|candidate| candidate == action)
        .then_some(action)
}

fn semantic_press_result(display_key: &str, action: &str, pid: i32, confirmed: bool) -> ToolResult {
    let effect = if confirmed {
        ActionEffect::Confirmed
    } else {
        ActionEffect::Unverifiable
    };
    let mut record = ActionExecutionRecord::builder(
        effect,
        ActionTransport::MacosAxAction,
        RequestedDelivery::Background,
    )
    .actual_delivery(ActualDelivery::Background);
    if confirmed {
        record = record.evidence(ActionEvidence {
            kind: EvidenceKind::AccessibilityReadback,
            detail: "the same native AX element changed value or selection after the semantic key action".into(),
        });
    }
    ToolResult::text(format!(
        "✅ Performed {action} for {display_key} on pid {pid}."
    ))
    .with_structured(serde_json::json!({
        "path": "ax_semantic",
        "verified": confirmed,
        "effect": if confirmed { "confirmed" } else { "unverifiable" },
    }))
    .with_action_record(record.build().expect("semantic key record is valid"))
}

/// A Return/Escape whose addressed sheet window is gone afterwards: the
/// sheet's dismissal is the key's read-back.
fn sheet_closed_result(
    summary: String,
    path: &str,
    sheet_window_id: u32,
    transport: ActionTransport,
) -> ToolResult {
    let record = ActionExecutionRecord::builder(
        ActionEffect::Confirmed,
        transport,
        RequestedDelivery::Background,
    )
    .actual_delivery(ActualDelivery::Background)
    .evidence(ActionEvidence {
        kind: EvidenceKind::WindowChange,
        detail: format!(
            "sheet window {sheet_window_id} is no longer listed by WindowServer after the key"
        ),
    })
    .build()
    .expect("a sheet-closed key record carries evidence");
    ToolResult::text(format!(
        "{summary} The sheet window {sheet_window_id} closed."
    ))
    .with_structured(serde_json::json!({
        "path": path,
        "verified": true,
        "effect": "confirmed",
        "sheet_closed": sheet_window_id,
    }))
    .with_action_record(record)
}

fn validate_post_target(pid: i32) -> anyhow::Result<()> {
    if pid <= 0 {
        anyhow::bail!("target pid {pid} is invalid");
    }
    // Both SLEventPostToPid and CGEventPostToPid are void APIs. A successful
    // call proves only that the request was accepted for posting, not that the
    // target consumed it. Reject the one positive pre-post failure oracle macOS
    // exposes: a process that no longer exists. EPERM still proves liveness.
    let status = unsafe { libc::kill(pid, 0) };
    if status == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EPERM) {
        Ok(())
    } else {
        anyhow::bail!("target pid {pid} is not available for event posting: {error}")
    }
}

/// Post the key (or action) once. Nothing is read before or after it: whether
/// the key changed anything is not checked here, so the result is always
/// unverifiable, and a caller that needs the effect observes the window.
fn dispatch_with_ax_oracle(
    _pid: i32,
    _window_id: Option<u32>,
    _explicit_element_ptr: Option<usize>,
    dispatch: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    dispatch()?;
    Ok(false)
}

fn action_record(confirmed: bool, foreground: bool) -> ActionExecutionRecord {
    let effect = if confirmed {
        ActionEffect::Confirmed
    } else {
        ActionEffect::Unverifiable
    };
    let transport = if foreground {
        ActionTransport::MacosCgEventHid
    } else {
        ActionTransport::MacosCgEventPid
    };
    let requested = if foreground {
        RequestedDelivery::Foreground
    } else {
        RequestedDelivery::Background
    };
    let actual = if foreground {
        ActualDelivery::Foreground
    } else {
        ActualDelivery::Background
    };
    let mut record =
        ActionExecutionRecord::builder(effect, transport, requested).actual_delivery(actual);
    if confirmed {
        record = record.evidence(ActionEvidence {
            kind: EvidenceKind::AccessibilityReadback,
            detail: "the same native AX element changed value or selection after the key post"
                .into(),
        });
    } else {
        record = record.evidence(ActionEvidence {
            kind: EvidenceKind::NativeApiResult,
            detail: if foreground {
                "the key events were constructed and the foreground HID post was attempted".into()
            } else {
                "the key events were constructed and the PID-routed post was attempted".into()
            },
        });
    }
    record.build().expect("press_key record is valid")
}

#[derive(Debug, thiserror::Error)]
#[error("the addressed element did not acquire keyboard focus; no key was dispatched")]
struct KeyFocusUnconfirmed;

/// The pointer focus fallback clicked the element, and focus still did not
/// stick. No key was sent, but the click was, and it may have acted.
#[derive(Debug, thiserror::Error)]
#[error(
    "clicked the addressed element to focus it, but it did not acquire keyboard focus; \
     no key was dispatched, and the click may have acted"
)]
struct KeyFocusUnconfirmedAfterClick;

#[derive(Debug, thiserror::Error)]
enum WindowFocusConflictError {
    #[error("addressed element does not belong to target window {target_window_id}")]
    AddressedElementOutsideWindow {
        pid: i32,
        target_window_id: u32,
        actual_window_id: Option<u32>,
    },
    #[error("target window {target_window_id} cannot receive process-scoped keys safely because sibling window {sibling_window_id} currently holds focus")]
    SiblingWindowFocused {
        pid: i32,
        target_window_id: u32,
        sibling_window_id: u32,
    },
    #[error("sheet window {sheet_window_id} cannot receive process-scoped keys safely: the app's focused element could not be proven to be inside that sheet")]
    SheetFocusUnproven { pid: i32, sheet_window_id: u32 },
}

fn map_focus_conflict_to_refusal(
    pid: i32,
    conflict: crate::ax::exact_target::WindowFocusConflict,
) -> Option<WindowFocusConflictError> {
    match conflict {
        crate::ax::exact_target::WindowFocusConflict::Clear => None,
        crate::ax::exact_target::WindowFocusConflict::AddressedElementOutsideWindow {
            target_window_id,
            actual_window_id,
        } => Some(WindowFocusConflictError::AddressedElementOutsideWindow {
            pid,
            target_window_id,
            actual_window_id,
        }),
        crate::ax::exact_target::WindowFocusConflict::SiblingWindowFocused {
            target_window_id,
            sibling_window_id,
        } => Some(WindowFocusConflictError::SiblingWindowFocused {
            pid,
            target_window_id,
            sibling_window_id,
        }),
        crate::ax::exact_target::WindowFocusConflict::SheetFocusUnproven { sheet_window_id } => {
            Some(WindowFocusConflictError::SheetFocusUnproven {
                pid,
                sheet_window_id,
            })
        }
    }
}

#[cfg(test)]
fn with_confirmed_key_focus<T>(
    target: Option<usize>,
    mut focus: impl FnMut(usize) -> anyhow::Result<()>,
    mut is_focused: impl FnMut(usize) -> bool,
    dispatch: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    with_confirmed_key_focus_or_fallback(
        target,
        &mut focus,
        &mut is_focused,
        |_| Ok(false),
        dispatch,
    )
}

fn with_confirmed_key_focus_or_fallback<T>(
    target: Option<usize>,
    mut focus: impl FnMut(usize) -> anyhow::Result<()>,
    mut is_focused: impl FnMut(usize) -> bool,
    mut fallback_focus: impl FnMut(usize) -> anyhow::Result<bool>,
    dispatch: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if let Some(target) = target {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        loop {
            focus(target)?;
            if is_focused(target) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                if fallback_focus(target)? {
                    let fallback_deadline =
                        std::time::Instant::now() + std::time::Duration::from_millis(120);
                    loop {
                        if is_focused(target) {
                            return dispatch();
                        }
                        if std::time::Instant::now() >= fallback_deadline {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    return Err(KeyFocusUnconfirmedAfterClick.into());
                }
                return Err(KeyFocusUnconfirmed.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    dispatch()
}

/// Late CELL recheck inside the delivery context: activation may have
/// replaced the first responder since the early guard, so the actual focused
/// element is re-read immediately before the key is posted.
fn late_cell_refusal(
    pid: i32,
    window_id: Option<u32>,
    printable: bool,
    element_targeted: bool,
) -> anyhow::Result<()> {
    if printable && !element_targeted {
        // Fail closed at the final check: the earlier classification cannot
        // establish the focus that activation or a focus change produced.
        if !unsafe { super::not_editable::sync_focused_cell_allows(pid, window_id) } {
            anyhow::bail!(
                "not_editable: the focused element at delivery is a non-editing cell or is                  unreadable; no keys were sent"
            );
        }
    }
    Ok(())
}

fn verify_raw_key_target(
    pid: i32,
    window_id: Option<u32>,
    pre_focus_ptr: Option<usize>,
    focused_key_route: bool,
) -> anyhow::Result<()> {
    let Some(wid) = window_id else {
        return Ok(());
    };
    if focused_key_route {
        if let Some(ptr) = pre_focus_ptr {
            let exact_focus = super::type_text::element_in_requested_window(pid, wid, ptr)
                && crate::input::ax_actions::is_element_focused(pid, ptr);
            if !exact_focus {
                anyhow::bail!(
                    "the addressed element is not the application's focused element in window {wid}; no key was sent"
                )
            }
            if !super::type_text::focused_window_matches_element(pid, wid, ptr) {
                anyhow::bail!(
                    "the addressed element remains focused, but the application's key window is not the requested window or its proven sheet parent; no key was sent"
                )
            }
            return Ok(());
        } else if crate::ax::bindings::focused_window_id_of_pid(pid) != Some(wid) {
            anyhow::bail!(
                "window {wid} is not the application's positively confirmed key window; no key was sent"
            )
        }
    }
    let conflict = crate::ax::exact_target::check_window_focus_conflict(pid, wid, pre_focus_ptr);
    if let Some(err) = map_focus_conflict_to_refusal(pid, conflict) {
        return Err(err.into());
    }
    Ok(())
}

/// Whether the resolved key+modifiers is a printable insertion (a single
/// non-control character with no modifiers, or Shift only). Guard
/// eligibility is decided here, independent of the keycode/unicode
/// transport: printable names and shift chords count, named keys do not.
pub(super) fn printable_request(key: &str, modifiers: &[String]) -> bool {
    // The resolver keeps named space as "space"; it inserts a character.
    if key == "space" {
        return modifiers.iter().all(|m| m == "shift");
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) if !ch.is_control() => modifiers.iter().all(|m| m == "shift"),
        _ => false,
    }
}

/// The key a call names, as typed and resolved with its modifiers, or the
/// refusal for a missing or unknown one. Pure: it reads only the arguments.
pub(super) fn requested_key(args: &Value) -> Result<(String, String, Vec<String>), ToolResult> {
    use cua_driver_core::tool_args::ArgsExt;
    let key_raw = args.require_str("key")?;
    let (key, modifiers) =
        crate::input::keyboard::resolve_key_press(&key_raw, &args.str_array("modifiers"))
            .map_err(invalid_key)?;
    Ok((key_raw, key, modifiers))
}

/// The key as the result names it: the chord actually sent (`shift+8`), with
/// the requested name after it when that differs (`shift+8 (*)`).
pub(super) fn pressed_key_display(key_raw: &str, key: &str, modifiers: &[String]) -> String {
    let chord = modifiers
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(key))
        .collect::<Vec<_>>()
        .join("+");
    let raw = key_raw.trim();
    let named = match raw.rsplit_once('+') {
        Some((_, last)) if !last.is_empty() => last,
        _ => raw,
    };
    if named.eq_ignore_ascii_case(key) {
        chord
    } else {
        format!("{chord} ({raw})")
    }
}

/// A key or modifier this driver cannot press, refused before anything is sent.
pub(super) fn invalid_key(error: anyhow::Error) -> ToolResult {
    let message = error.to_string();
    ToolResult::error(&message).with_structured(serde_json::json!({
        "code": "invalid_arguments",
        "effect": "refused",
        "dispatched": false,
        "message": message,
    }))
}

fn delivery_failed(error: anyhow::Error) -> ToolResult {
    if let Some(conflict) = error.downcast_ref::<WindowFocusConflictError>() {
        return match *conflict {
            WindowFocusConflictError::AddressedElementOutsideWindow {
                pid,
                target_window_id,
                actual_window_id,
            } => ToolResult::error(conflict.to_string()).with_structured(serde_json::json!({
                "code": cua_driver_core::background_input::refusal_codes::ELEMENT_OUTSIDE_TARGET_WINDOW,
                "effect": "refused",
                "dispatched": false,
                "pid": pid,
                "window_id": target_window_id,
                "actual_window_id": actual_window_id,
                "message": conflict.to_string(),
            })),
            WindowFocusConflictError::SiblingWindowFocused {
                pid,
                target_window_id,
                sibling_window_id,
            } => ToolResult::error(conflict.to_string()).with_structured(serde_json::json!({
                "code": cua_driver_core::background_input::refusal_codes::SAME_PID_KEYBOARD_AMBIGUITY,
                "effect": "refused",
                "dispatched": false,
                "pid": pid,
                "window_id": target_window_id,
                "sibling_window_id": sibling_window_id,
                "message": conflict.to_string(),
            })),
            WindowFocusConflictError::SheetFocusUnproven {
                pid,
                sheet_window_id,
            } => ToolResult::error(conflict.to_string()).with_structured(serde_json::json!({
                "code": cua_driver_core::background_input::refusal_codes::SAME_PID_KEYBOARD_AMBIGUITY,
                "effect": "refused",
                "dispatched": false,
                "pid": pid,
                "window_id": sheet_window_id,
                "message": conflict.to_string(),
            })),
        };
    }
    if error.is::<KeyFocusUnconfirmed>() {
        return ToolResult::error(error.to_string()).with_structured(serde_json::json!({
            "code": "target_focus_unconfirmed", "effect": "refused", "dispatched": false,
            "message": error.to_string(),
        }));
    }
    if error.is::<KeyFocusUnconfirmedAfterClick>() {
        // Not an error: the click was sent and may have acted. The caller
        // reads the window before sending anything again.
        return ToolResult::text(format!("⚠️ {error}")).with_structured(serde_json::json!({
            "code": "target_focus_unconfirmed_after_click",
            "path": "cgevent",
            "effect": "unverifiable",
            "keys_sent": false,
            "message": error.to_string(),
        }));
    }
    let message = format!("press_key delivery failed: {error}");
    ToolResult::error(&message).with_structured(serde_json::json!({
        "code": "delivery_failed",
        "message": message,
    }))
}

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "press_key".into(),
        description: "Press and release one key. `delivery_mode` as in click/type_text.\n\
            - `background` (default): posts to the pid without raising the window; with `element_token` it focuses that element first.\n\
            - `foreground`: fronts the exact window (needs `window_id`), sends a real HID key transition (Chromium content, inline editors, menu equivalents), then restores the prior app.\n\
            \n\
            An element target whose focus cannot be confirmed is refused with `target_focus_unconfirmed`, effect \"refused\", and no key dispatched.\n\
            Confirmed only when a native AX value/selection read-back changes; otherwise `effect:\"unverifiable\"` (not a delivery failure). Keys: return, tab, escape, up/down/left/right, space, delete, home, end, pageup, pagedown, f1-f12, any letter or digit.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["key"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it." },
                "pid": { "type": "integer", "description": "Target process ID." },
                "key": { "type": "string", "description": "Key name." },
                "modifiers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "cmd, shift, option/alt, ctrl, fn."
                },
                "window_id": { "type": "integer", "description": "Target window; required for foreground. Does not raise by itself." },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "x": { "type": "number", "description": "Screenshot-pixel X: pixel-click to focus, then send the key. With y, no element_token." },
                "y": { "type": "number", "description": "Screenshot-pixel Y (see x)." },
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "\"desktop\" with no pid/window_id sends to the frontmost app." },
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema_with("Default \"background\": no fronting or focus steal. \"foreground\": briefly front the window, act, restore the prior app; last resort when background did not land.")
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    })
}

#[async_trait]
impl Tool for PressKeyTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<PressKeyInput>("press_key", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            if let Some(refusal) = cua_driver_core::browser::protection::frontmost_app_refusal(
                "press_key",
                crate::apps::frontmost_pid().map(i64::from),
            ) {
                return refusal;
            }
            let (key, modifiers) = match crate::input::keyboard::resolve_key_press(
                &input.key,
                &input.modifiers.unwrap_or_default(),
            ) {
                Ok(resolved) => resolved,
                Err(error) => return invalid_key(error),
            };
            let key_for_input = key.clone();
            let display_key = pressed_key_display(&input.key, &key, &modifiers);
            let early_front_pid = crate::apps::frontmost_pid();
            if printable_request(&key, &modifiers) {
                // Desktop scope dispatches globally: check the frontmost
                // app's focused element and fail closed before any key.
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
                            "the frontmost application could not be determined, so editability                              is unknown; no keys were sent."
                                .into(),
                            super::not_editable::FocusDispatch::None,
                        )
                    }
                }
            }
            let checked_pid = early_front_pid.unwrap_or_default();
            let printable_desktop = printable_request(&key, &modifiers);
            let result = latency::spawn_action_blocking(move || {
                super::not_editable::desktop_dispatch_guard(checked_pid, printable_desktop)?;
                let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                crate::input::keyboard::press_key_global(&key_for_input, &modifier_refs)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!("Pressed {display_key} on the desktop."))
                    .with_structured(serde_json::json!({
                        "scope": "desktop",
                        "path": "hid",
                        "effect": "unverifiable"
                    })),
                Ok(Err(error)) => super::not_editable::account_focus_receipt(
                    ToolResult::error(format!("desktop press_key failed: {error}")),
                    None,
                ),
                Err(error) => super::not_editable::account_focus_receipt(
                    ToolResult::error(format!("desktop press_key task failed: {error}")),
                    None,
                ),
            };
        }
        // Resolve the key first: an unknown name is refused before any
        // target work, so nothing is focused or sent for it.
        let (key_raw, key, modifiers) = match requested_key(&args) {
            Ok(requested) => requested,
            Err(refusal) => return refusal,
        };
        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match latency::resolve(|| self.state.snapshots.resolve(pid, &args)) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, pre_focus_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };

        if let Err(error) = validate_post_target(pid) {
            return delivery_failed(error);
        }
        let display_key = pressed_key_display(&key_raw, &key, &modifiers);
        // delivery_mode gates the raise: background (default) never fronts the
        // window (auth-envelope post, even with window_id); foreground is the
        // explicit NSMenu-activation rung. Matches click/type_text/hotkey.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        let fg = delivery_mode.is_foreground();
        // The person is working in this app: nothing is sent.
        if !fg {
            if let Err(refusal) = super::gate_user_takeover(pid, window_id) {
                return refusal;
            }
        }

        // Argument-shape errors are reported before any gating or retained
        // lookups: a malformed call must fail the same way regardless of
        // background-target state.
        let px = args.get("x").and_then(|v| v.as_f64());
        let py = args.get("y").and_then(|v| v.as_f64());
        if px.is_some() && py.is_some() && element_index.is_some() {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to press_key, not both.",
            );
        }

        let pre_focus_ptr: Option<usize> = pre_focus_guard.as_ref().map(|g| g.as_ptr());

        if fg {
            if let Err(refusal) = super::gate_foreground_physical_idle(pid, window_id) {
                return refusal;
            }
        }

        let mut _mutation_lease = if !fg && window_id.is_some() {
            Some(super::acquire_background_mutation(pid).await)
        } else {
            None
        };

        // Return/Escape first use an advertised semantic action on the exact
        // element. Pixel requests skip this branch: their requested point has
        // not been focused yet, so the old focused element must not confirm.
        if !fg && px.is_none() && semantic_action_for_request(&key, &modifiers).is_some() {
            let semantic_guard = if let Some(guard) = pre_focus_guard.clone() {
                Some(guard)
            } else if let Some(wid) = window_id {
                latency::spawn_action_blocking(move || unsafe {
                    crate::ax::exact_target::focused_element_in_window(pid, wid).map(|ptr| {
                        let owned = crate::ax::snapshot::RetainedElement::retain(ptr as usize);
                        CFRelease(ptr as _);
                        owned
                    })
                })
                .await
                .ok()
                .flatten()
            } else {
                None
            };
            if let (Some(wid), Some(lease), Some(guard)) =
                (window_id, _mutation_lease.as_ref(), semantic_guard)
            {
                let ptr = guard.as_ptr();
                let advertised = latency::spawn_action_blocking(move || unsafe {
                    crate::ax::bindings::copy_action_names(ptr as AXUIElementRef)
                })
                .await
                .unwrap_or_default();
                if let Some(action) = advertised_semantic_action(&key, &modifiers, &advertised) {
                    if let Err(refusal) = lease
                        .gate_again(
                            wid,
                            Some(ptr),
                            cua_driver_core::background_input::BackgroundAction::AxSemantic,
                        )
                        .await
                    {
                        return refusal;
                    }
                    let result = latency::spawn_action_blocking(move || {
                        let sheet = super::sheet_close::SheetWatch::for_window(pid, wid);
                        // The observation counts from the action itself, not
                        // from the end of the oracle's own read-back.
                        let posted = std::cell::Cell::new(None);
                        let confirmed = dispatch_with_ax_oracle(pid, Some(wid), Some(ptr), || {
                            let result = crate::input::ax_actions::perform_ax_action(ptr, action);
                            posted.set(Some(std::time::Instant::now()));
                            result
                        })?;
                        Ok::<_, anyhow::Error>((
                            confirmed,
                            sheet
                                .filter(|sheet| {
                                    !confirmed
                                        && posted.get().is_some_and(|at| sheet.closed_after(at))
                                })
                                .map(|sheet| sheet.sheet_window_id()),
                        ))
                    })
                    .await;
                    return match result {
                        Ok(Ok((_, Some(sheet)))) => sheet_closed_result(
                            format!("✅ Performed {action} for {display_key} on pid {pid}."),
                            "ax_semantic",
                            sheet,
                            ActionTransport::MacosAxAction,
                        ),
                        Ok(Ok((confirmed, None))) => {
                            semantic_press_result(&display_key, action, pid, confirmed)
                        }
                        Ok(Err(error)) => delivery_failed(error),
                        Err(error) => {
                            delivery_failed(anyhow::anyhow!("semantic key task failed: {error}"))
                        }
                    };
                }
            }
        }

        let native_background_menu_app = !super::type_text::is_browser_pid(pid)
            && !crate::browser::electron_js::ElectronJs::is_electron(pid);
        if !fg
            && px.is_none()
            && pre_focus_ptr.is_some()
            && native_background_menu_app
            && super::menu_equivalent::Combo::parse(&modifiers, &key).is_some()
        {
            return super::pre_dispatch_refusal(
                "background_unavailable",
                "press_key: an explicit native element Command chord cannot be rebound safely after menu priming, and raw native menu keys can no-op. Use the window-only shortcut or a direct element action; no key or menu action was sent.".to_owned(),
            );
        }

        // ── Menu key equivalents in a native app ──
        // Background key events reach a window's views but never its app's
        // menu key equivalents: a background cmd+a to TextEdit was reported
        // "Pressed" and selected nothing. After the exact-target gate, the key
        // scope and the focus-conflict check above, and under the same
        // per-PID lease, a Command combo that is a menu item's key equivalent
        // runs that item through accessibility, or is refused with no keys
        // sent, as hotkey does. Browsers, Electron and the
        // px form keep the key route.
        if !fg && px.is_none() && pre_focus_ptr.is_none() {
            if let (Some(wid), Some(lease), Some(combo)) = (
                window_id,
                _mutation_lease.as_ref(),
                super::menu_equivalent::Combo::parse(&modifiers, &key),
            ) {
                let lookup = combo.clone();
                let menu_item = latency::spawn_action_blocking(move || {
                    let native = !super::type_text::is_browser_pid(pid)
                        && !crate::browser::electron_js::ElectronJs::is_electron(pid);
                    if native {
                        super::menu_equivalent::find(pid, &lookup)
                    } else {
                        None
                    }
                })
                .await
                .ok()
                .flatten();
                if let Some(item) = menu_item {
                    if let Err(refusal) = lease
                        .gate_again(
                            wid,
                            None,
                            cua_driver_core::background_input::BackgroundAction::AxSemantic,
                        )
                        .await
                    {
                        return refusal;
                    }
                    // The item found above only decides that this chord is a
                    // menu command; it is looked up again once the window is
                    // the app's key window, since enablement and even the item
                    // can differ per window.
                    let _ = item;
                    let lease = _mutation_lease
                        .take()
                        .expect("background window actions hold the per-pid lease");
                    return native_menu_chord(
                        lease,
                        wid,
                        combo,
                        display_key.clone(),
                        (key.clone(), modifiers.clone()),
                    )
                    .await;
                }
            }
        }

        let focused_key_route = !fg && crate::input::skylight::is_focus_without_raise_available();

        // ── A minimized window: restore it for this key, then minimize it again ──
        // Only for a non-printable key (Return, Escape, arrows, Tab) or one
        // addressed to an exact element: an untargeted printable key needs
        // the editing-cell checks of the visible route.
        if !fg
            && px.is_none()
            && (pre_focus_guard.is_some() || !printable_request(&key, &modifiers))
        {
            if let (Some(wid), Some(lease)) = (window_id, _mutation_lease.as_ref()) {
                let facts_guard = pre_focus_guard.clone();
                let decision = latency::spawn_action_blocking(move || {
                    let ptr = facts_guard.as_ref().map(|guard| guard.as_ptr());
                    let facts = crate::ax::exact_target::gather_background_facts(pid, wid, ptr);
                    super::type_text::restore_decision(facts.target_minimized, facts.app_hidden)
                })
                .await
                .unwrap_or(super::type_text::RestoreDecision::Refuse);
                if decision == super::type_text::RestoreDecision::Restore {
                    if let Err(refusal) = lease
                        .gate_again(
                            wid,
                            pre_focus_ptr,
                            cua_driver_core::background_input::BackgroundAction::AxSemantic,
                        )
                        .await
                    {
                        return refusal;
                    }
                    let lease = _mutation_lease
                        .take()
                        .expect("background window actions hold the per-pid lease");
                    return minimized_key_route(
                        lease,
                        wid,
                        pre_focus_guard.clone(),
                        key.clone(),
                        modifiers.clone(),
                        display_key.clone(),
                    )
                    .await;
                }
            }
        }

        if !fg {
            if let (Some(wid), Some(lease)) = (window_id, _mutation_lease.as_ref()) {
                if pre_focus_ptr.is_some_and(|ptr| {
                    !super::type_text::element_in_requested_window(pid, wid, ptr)
                }) {
                    return super::pre_dispatch_refusal(
                        "element_outside_target_window",
                        "press_key: the addressed element is not in the requested window or its exact sheet ancestry; no key was sent.",
                    );
                }
                let action = if focused_key_route {
                    cua_driver_core::background_input::BackgroundAction::ExactWindowKeysFocused
                } else {
                    super::window_keyboard_action(
                        cua_driver_core::background_input::BackgroundAction::GenericKey,
                    )
                };
                let gate_window_id = pre_focus_ptr
                    .filter(|ptr| super::type_text::element_in_requested_window(pid, wid, *ptr))
                    .map(|ptr| super::type_text::gate_window_id_for_element(ptr, wid))
                    .unwrap_or(wid);
                if let Err(refusal) = lease
                    .gate_again(gate_window_id, pre_focus_ptr, action)
                    .await
                {
                    return refusal;
                }
            }
        }

        // The addressed window must be the app's confirmed key window before
        // any key (or focus click) goes out; otherwise refuse (window_not_key).
        let _key_scope = match super::acquire_key_scope(pid, window_id, fg).await {
            Ok(scope) => scope,
            Err(refusal) => return refusal,
        };

        let printable = printable_request(&key, &modifiers);
        // CELL rule for untargeted printable keys: a non-editing cell (or an
        // unreadable focused element) refuses the call before any key.
        if printable && pre_focus_guard.is_none() && px.is_none() {
            if let Some(refusal) = super::not_editable::guard_untargeted(pid, window_id).await {
                return refusal;
            }
        }

        // px form: pixel-click to focus, then the key goes to the focused element.
        // Reuses click's translation + delivery_mode; after it, deliver via the
        // plain background path (the focus-click already handled fronting if fg).
        let mut px_receipt = None;
        let px_focus = {
            if let (Some(cx), Some(cy)) = (px, py) {
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
                    fg,
                    args.opt_str("session"),
                    args.opt_str("_session_id"),
                    from_zoom,
                    _mutation_lease.as_ref(),
                    printable,
                )
                .await
                {
                    Ok(receipt) => receipt,
                    Err(e) => return e,
                };
                px_receipt = Some(focus_receipt);
                if printable
                    && !super::not_editable::focused_cell_allows_typing(pid, window_id).await
                {
                    return super::not_editable::not_editable_result(
                        "the element focused by the click is not being edited; no keys were sent."
                            .into(),
                        focus_receipt,
                    );
                }
                true
            } else {
                false
            }
        };

        // The px form's focus click already moved the cursor. Otherwise place
        // a named session's cursor on the element, or its remembered position,
        // or the window centre, so a keyboard-first session stays visible.
        if !px_focus {
            let element_center = match pre_focus_guard.clone() {
                Some(guard) => latency::spawn_action_blocking(move || unsafe {
                    crate::ax::bindings::element_screen_center(
                        guard.as_ptr() as crate::ax::bindings::AXUIElementRef
                    )
                })
                .await
                .ok()
                .flatten(),
                None => None,
            };
            super::cursor_tools::position_keyboard_cursor(
                &self.state,
                &args,
                window_id,
                element_center,
            )
            .await;
        }

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // Single-key presses can fire autocomplete (Return on a search
        // box opens a results popover) or trigger menu shortcuts that
        // open windows. Wrapping mirrors the hotkey path.
        //
        // The AX focus_element() pre-write also runs inside the closure
        // so any reflex activations it triggers are caught by both the
        // wildcard snapshot suppressor and the targeted FocusGuard lease.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);
        // Return/Escape to a sheet: its window closing is the key's
        // read-back. The baseline is read before the post.
        let sheet_watch = match window_id {
            Some(wid)
                if !fg && !px_focus && semantic_action_for_request(&key, &modifiers).is_some() =>
            {
                latency::spawn_action_blocking(move || {
                    super::sheet_close::SheetWatch::for_window(pid, wid)
                })
                .await
                .ok()
                .flatten()
            }
            _ => None,
        };

        // When the background key post returned: the sheet watch counts from it.
        let posted_at = std::sync::Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
        let posted_in = posted_at.clone();
        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "press_key.CGEvent",
            || async move {
                latency::spawn_action_blocking(move || {
                    let pre_focus_ptr = pre_focus_guard.as_ref().map(|guard| guard.as_ptr());
                    let m: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    // Foreground rung: keep the exact target frontmost through a genuine
                    // physical HID key down/up pair, then restore. PID-routed events without the
                    // authentication envelope reach NSMenu, but Chromium/Electron may
                    // silently discard them even while frontmost; the guarded HID route
                    // is accepted by both. Skipped when px-focus already handled the
                    // target-specific foreground transition.
                    if fg && !px_focus {
                        let wid = window_id.ok_or_else(|| {
                            anyhow::anyhow!(
                                "delivery_mode=foreground requires window_id for press_key"
                            )
                        })?;
                        return dispatch_with_ax_oracle(pid, window_id, pre_focus_ptr, || {
                            crate::input::skylight::with_foreground_hid_activation(
                                pid as libc::pid_t,
                                wid,
                                || {
                                    // Activation may replace the first responder; confirm inside the HID guard.
                                    with_confirmed_key_focus_or_fallback(
                                        pre_focus_ptr,
                                        crate::input::ax_actions::focus_element,
                                        |target| {
                                            crate::input::ax_actions::element_has_keyboard_focus_in_window(
                                                pid,
                                                Some(wid),
                                                target,
                                            )
                                        },
                                        |target| {
                                            if crate::input::ax_actions::element_supports_pointer_focus_fallback(
                                                pid,
                                                target,
                                                Some(wid),
                                            ) {
                                                super::type_text::click_element_center_for_focus_checked(
                                                    pid, wid, target, true,
                                                )?;
                                                Ok(true)
                                            } else {
                                                Ok(false)
                                            }
                                        },
                                        || {
                                verify_raw_key_target(pid, Some(wid), pre_focus_ptr, false)?;
                                late_cell_refusal(pid, Some(wid), printable, pre_focus_ptr.is_some())?;
                                crate::input::keyboard::press_key_bare_global(&key, &m)
                            },
                                    )
                                },
                            )
                        });
                    }
                    // background (default): auth-envelope post, no raise.
                    dispatch_with_ax_oracle(pid, window_id, pre_focus_ptr, || {
                        with_confirmed_key_focus_or_fallback(
                            pre_focus_ptr,
                            crate::input::ax_actions::focus_element,
                            |target| {
                                crate::input::ax_actions::element_has_keyboard_focus_in_window(
                                    pid, window_id, target,
                                )
                            },
                            |target| {
                                if let Some(wid) = window_id {
                                    if crate::input::ax_actions::element_supports_pointer_focus_fallback(
                                        pid,
                                        target,
                                        Some(wid),
                                    ) {
                                        super::type_text::click_element_center_for_focus_checked(
                                            pid, wid, target, false,
                                        )?;
                                        return Ok(true);
                                    }
                                }
                                Ok(false)
                            },
                            || {
                            verify_raw_key_target(pid, window_id, pre_focus_ptr, focused_key_route)?;
                            late_cell_refusal(pid, window_id, printable, pre_focus_ptr.is_some())?;
                            let posted = crate::input::keyboard::press_key(pid, &key, &m);
                            if posted.is_ok() {
                                *posted_in.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(std::time::Instant::now());
                            }
                            posted
                        },
                        )
                    })
                })
                .await
            },
        )
        .await;

        let posted = *posted_at.lock().unwrap_or_else(|e| e.into_inner());
        let sheet_closed = match (&result, sheet_watch, posted) {
            (Ok(Ok(false)), Some(watch), Some(posted)) => {
                latency::spawn_action_blocking(move || {
                    watch.closed_after(posted).then(|| watch.sheet_window_id())
                })
                .await
                .ok()
                .flatten()
            }
            _ => None,
        };
        let changes = super::finish_window_observation(snapshot).await;

        let delivery_outcome = match result {
            Ok(result) => map_delivery_outcome(result),
            Err(error) => {
                PressKeyDeliveryOutcome::Failed(anyhow::anyhow!("posting task failed: {error}"))
            }
        };

        let result = match delivery_outcome {
            PressKeyDeliveryOutcome::Unverifiable if sheet_closed.is_some() => sheet_closed_result(
                format!(
                    "✅ Pressed {display_key} on pid {pid}.{}",
                    changes.result_suffix()
                ),
                "key_events",
                sheet_closed.expect("checked above"),
                ActionTransport::MacosCgEventPid,
            ),
            outcome @ (PressKeyDeliveryOutcome::Confirmed
            | PressKeyDeliveryOutcome::Unverifiable) => {
                let confirmed = matches!(outcome, PressKeyDeliveryOutcome::Confirmed);
                let label = if fg {
                    " (delivery_mode:foreground)"
                } else {
                    ""
                };
                let structured = serde_json::json!({
                    "path": if fg { "key_events_fg" } else { "key_events" },
                    "verified": confirmed,
                    "effect": if confirmed { "confirmed" } else { "unverifiable" },
                });
                ToolResult::text(format!(
                    "✅ Pressed {display_key} on pid {pid}{label}.{}",
                    changes.result_suffix()
                ))
                .with_structured(structured)
                .with_action_record(action_record(confirmed, fg))
            }
            PressKeyDeliveryOutcome::Failed(error) => delivery_failed(error),
        };
        // Any refusal after a delivered focus action carries its receipt.
        super::not_editable::account_focus_receipt(result, px_receipt)
    }
}

// ── Raw key into a minimized window: temporary restore (400-F1 wp2) ─────────

/// What the minimized-window worker did.
struct MinimizedKeyStep {
    /// `None` when refused before any key; otherwise the delivery result.
    outcome: Option<anyhow::Result<bool>>,
    refusal: Option<(&'static str, String)>,
    notes: String,
    /// Focus writes this route sent before the outcome.
    focus_writes: u32,
}

fn minimized_notes(
    restore: super::type_text::RestoreReport,
    scope: Option<(
        crate::input::skylight::KeyEntry,
        crate::input::skylight::HandBack,
    )>,
) -> String {
    let mut notes = Vec::new();
    if let Some((entry, hand_back)) = scope {
        notes.push(crate::input::skylight::scope_disclosure(
            entry,
            Some(hand_back),
        ));
    }
    notes.push(restore.describe());
    notes.retain(|note| !note.is_empty());
    notes.join(" ")
}

/// Restore the minimized window, re-check it as a visible target, hold it as
/// the app's key window, post the key to the exact element (or the window's
/// focused element), hand key back and minimize the window again: all on
/// this thread, so each step finishes before the next local drops.
fn minimized_key_worker(
    pid: i32,
    wid: u32,
    guard: Option<crate::ax::snapshot::RetainedElement>,
    key: String,
    modifiers: Vec<String>,
) -> MinimizedKeyStep {
    use super::type_text::{AxMinimizeOps, MinimizedRestore};
    use cua_driver_core::background_input::{
        decide_background_input, BackgroundAction, BackgroundInputDecision, ExactWindowTarget,
    };
    let refused = |code, text: String, notes: String| MinimizedKeyStep {
        outcome: None,
        refusal: Some((code, text)),
        notes,
        focus_writes: 0,
    };
    let Some(ops) = AxMinimizeOps::of(pid, wid) else {
        return refused(
            cua_driver_core::background_input::refusal_codes::MINIMIZED_OR_HIDDEN,
            format!("window {wid} could not be resolved to restore it; no key was sent."),
            String::new(),
        );
    };
    let mut restore = MinimizedRestore::new(ops);
    if let Err(reason) = restore.begin(|| std::thread::sleep(std::time::Duration::from_millis(25)))
    {
        return refused(
            cua_driver_core::background_input::refusal_codes::MINIMIZED_OR_HIDDEN,
            format!("window {wid} is minimized and {reason}; no key was sent."),
            minimized_notes(restore.finish(), None),
        );
    }
    let ptr = guard.as_ref().map(|guard| guard.as_ptr());
    let facts = crate::ax::exact_target::gather_background_facts(pid, wid, ptr);
    if let BackgroundInputDecision::Refuse(refusal) = decide_background_input(
        ExactWindowTarget {
            pid,
            window_id: wid,
        },
        &facts,
        BackgroundAction::ExactWindowKeysFocused,
    ) {
        return refused(
            refusal.code,
            format!("{}; no key was sent.", refusal.reason),
            minimized_notes(restore.finish(), None),
        );
    }
    // The gate accepts any readable visibility for focused keys; the
    // restored window must read visible in a visible app.
    if !(facts.target_minimized == Some(false) && facts.app_hidden == Some(false)) {
        return refused(
            cua_driver_core::background_input::refusal_codes::MINIMIZED_OR_HIDDEN,
            format!(
                "window {wid} was restored but does not read as visible in a visible app; no key was sent."
            ),
            minimized_notes(restore.finish(), None),
        );
    }
    let scope = match crate::input::skylight::enter_exact_window_key_inline(pid as libc::pid_t, wid)
    {
        Ok(scope) => scope,
        Err(reason) => {
            return refused(
                super::WINDOW_NOT_KEY,
                format!("{reason}."),
                minimized_notes(restore.finish(), None),
            )
        }
    };
    let m: Vec<&str> = modifiers.iter().map(String::as_str).collect();
    let focus_writes = std::cell::Cell::new(0u32);
    let outcome = dispatch_with_ax_oracle(pid, Some(wid), ptr, || {
        with_confirmed_key_focus_or_fallback(
            ptr,
            |target| {
                focus_writes.set(focus_writes.get() + 1);
                crate::input::ax_actions::focus_element(target)
            },
            |target| {
                crate::input::ax_actions::element_has_keyboard_focus_in_window(
                    pid,
                    Some(wid),
                    target,
                )
            },
            |_| Ok(false),
            || {
                verify_raw_key_target(pid, Some(wid), ptr, true)?;
                crate::input::keyboard::press_key(pid, &key, &m)
            },
        )
    });
    let entry = scope.entry();
    let hand_back = scope.finish();
    MinimizedKeyStep {
        outcome: Some(outcome),
        refusal: None,
        notes: minimized_notes(restore.finish(), Some((entry, hand_back))),
        focus_writes: focus_writes.get(),
    }
}

async fn minimized_key_route(
    lease: super::BackgroundMutationLease,
    wid: u32,
    guard: Option<crate::ax::snapshot::RetainedElement>,
    key: String,
    modifiers: Vec<String>,
    display_key: String,
) -> ToolResult {
    let pid = lease.pid();
    let step = run_detached(async move {
        let _lease = lease;
        latency::spawn_action_blocking(move || {
            minimized_key_worker(pid, wid, guard, key, modifiers)
        })
        .await
    })
    .await;
    let step = match step {
        Ok(Ok(step)) => step,
        Ok(Err(error)) | Err(error) => {
            return task_failure(format!(
                "press_key: the minimized-window task failed ({error}); a key may have been sent \
                 and the window may be left restored. Read the window before repeating it."
            ))
        }
    };
    minimized_key_result(step, pid, &display_key)
}

fn minimized_key_result(step: MinimizedKeyStep, pid: i32, display_key: &str) -> ToolResult {
    let notes = if step.notes.is_empty() {
        String::new()
    } else {
        format!(" {}", step.notes)
    };
    if let Some((code, text)) = step.refusal {
        let mut structured = serde_json::json!({
            "code": code,
            "effect": "refused",
            "dispatched": false,
            "keys_sent": false,
        });
        // Error results are not projected into a summary: the restore and
        // hand-back disclosure travels as a structured field too.
        if !notes.trim().is_empty() {
            structured["restore_note"] = serde_json::json!(notes.trim());
        }
        // The whole text (scope entry and hand-back included) as structured.
        let message = format!("press_key: {text}{notes}");
        structured["message"] = serde_json::json!(message.clone());
        return ToolResult::error(message).with_structured(structured);
    }
    match step.outcome.map(map_delivery_outcome) {
        Some(
            outcome @ (PressKeyDeliveryOutcome::Confirmed | PressKeyDeliveryOutcome::Unverifiable),
        ) => {
            let confirmed = matches!(outcome, PressKeyDeliveryOutcome::Confirmed);
            ToolResult::text(format!("✅ Pressed {display_key} on pid {pid}.{notes}"))
                .with_structured(serde_json::json!({
                    "path": "key_events",
                    "verified": confirmed,
                    "effect": if confirmed { "confirmed" } else { "unverifiable" },
                    "restored_minimized_window": true,
                }))
                .with_action_record(action_record(confirmed, false))
        }
        Some(PressKeyDeliveryOutcome::Failed(error)) => {
            // Only these errors prove no key event was posted; any other
            // (a key-down sent before a key-up failed) may have sent one.
            let before_any_key = error.is::<KeyFocusUnconfirmed>()
                || error.is::<KeyFocusUnconfirmedAfterClick>()
                || error.to_string().contains("no key was sent");
            let mut result = delivery_failed(error);
            // Focus writes went out, or a key event may have: never a
            // nothing-sent refusal.
            if step.focus_writes > 0 || !before_any_key {
                if !matches!(
                    result.structured_content,
                    Some(serde_json::Value::Object(_))
                ) {
                    result.structured_content =
                        Some(serde_json::json!({ "effect": "unverifiable" }));
                }
                if let Some(serde_json::Value::Object(fields)) = result.structured_content.as_mut()
                {
                    // Something may have been delivered: never refused, and
                    // never an effect-less error.
                    fields.insert("effect".into(), serde_json::json!("unverifiable"));
                    fields.remove("dispatched");
                    if before_any_key {
                        fields.insert("keys_sent".into(), serde_json::json!(false));
                    } else {
                        fields.remove("keys_sent");
                    }
                    fields.insert(
                        "focus_writes_sent".into(),
                        serde_json::json!(step.focus_writes),
                    );
                }
                let key_note = if before_any_key {
                    "no key was sent."
                } else {
                    "a key event may have been sent. Read the window before repeating it."
                };
                let note = match step.focus_writes {
                    0 => key_note.to_owned(),
                    n => format!("{n} focus write(s) were sent to the element; {key_note}"),
                };
                append_to_summary(&mut result, &note);
                // An unconfirmed delivery, not an error: its record is published.
                result.is_error = None;
                result = result.with_action_record(
                    ActionExecutionRecord::builder(
                        ActionEffect::Unverifiable,
                        if before_any_key {
                            ActionTransport::MacosAxAction
                        } else {
                            ActionTransport::MacosCgEventPid
                        },
                        RequestedDelivery::Background,
                    )
                    .actual_delivery(ActualDelivery::Background)
                    .build()
                    .expect("an unverifiable minimized key record is valid"),
                );
            }
            append_to_summary(&mut result, &notes);
            let message = result
                .content
                .iter()
                .filter_map(|content| match content {
                    cua_driver_core::protocol::Content::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            if let Some(serde_json::Value::Object(fields)) = result.structured_content.as_mut() {
                if !notes.trim().is_empty() {
                    fields.insert("restore_note".into(), serde_json::json!(notes.trim()));
                }
                fields.insert("message".into(), serde_json::json!(message));
            }
            result
        }
        None => ToolResult::error(format!("press_key: no key was sent.{notes}")),
    }
}

/// Append to the result's first text block: the published summary is that
/// block, so a later block would not reach structured-only clients.
pub(super) fn append_to_summary(result: &mut ToolResult, extra: &str) {
    let extra = extra.trim();
    if extra.is_empty() {
        return;
    }
    for content in result.content.iter_mut() {
        if let cua_driver_core::protocol::Content::Text { text, .. } = content {
            text.push(' ');
            text.push_str(extra);
            return;
        }
    }
    result
        .content
        .push(cua_driver_core::protocol::Content::text(extra.to_owned()));
}

// ── Window-only native menu chord (key scope + own menu dispatch) ───────────

/// A task failure after which input may have gone out: unverifiable, with
/// its whole text structured (error results get no summary).
fn task_failure(message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(serde_json::json!({
        "effect": "unverifiable",
        "code": "ax_action_unverified",
        "message": message,
    }))
}

/// Run `work`, which owns the per-process mutation lease, in its own task.
/// Dropping the caller's future does not abort it (a `JoinHandle` drop
/// detaches), so the lease is released only after `work` finished: its
/// blocking menu press and its key hand-back never outlive the lease.
async fn run_detached<T, F>(work: F) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    tokio::spawn(work).await
}

/// The target app is frontmost now. WindowServer's answer is used when it
/// has one: AppKit's frontmost app lags behind it for an app that was just
/// opened or activated.
fn app_is_frontmost(pid: i32, wid: u32) -> bool {
    super::invoke_menu::live_frontmost_pid(pid, wid) == Some(pid)
}

/// The app's focused and main windows are both `wid`: its menu commands
/// apply to that window.
fn menu_bound_to(pid: i32, wid: u32) -> bool {
    use crate::ax::bindings::{ax_get_window_id, copy_element_attr, AXUIElementCreateApplication};
    if crate::ax::bindings::focused_window_id_of_pid(pid) != Some(wid) {
        return false;
    }
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return false;
        }
        let main = copy_element_attr(app, "AXMainWindow");
        CFRelease(app as _);
        let Some(main) = main else {
            return false;
        };
        let bound = ax_get_window_id(main) == Some(wid);
        CFRelease(main as _);
        bound
    }
}

/// The menu item at `path` (titles from the menu bar down), retained.
fn resolve_menu_item(pid: i32, path: &[String]) -> Option<crate::ax::snapshot::RetainedElement> {
    use crate::ax::bindings::{copy_children, copy_element_attr, AXUIElementCreateApplication};
    unsafe fn items_of(parent: AXUIElementRef) -> Vec<AXUIElementRef> {
        let mut out = Vec::new();
        for child in copy_children(parent) {
            if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
                out.extend(copy_children(child));
                CFRelease(child as _);
            } else {
                out.push(child);
            }
        }
        out
    }
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let bar = copy_element_attr(app, "AXMenuBar");
        CFRelease(app as _);
        let mut current = bar?;
        for title in path {
            let mut next = None;
            for item in items_of(current) {
                if next.is_none() && copy_string_attr(item, "AXTitle").as_deref() == Some(title) {
                    next = Some(item);
                } else {
                    CFRelease(item as _);
                }
            }
            CFRelease(current as _);
            current = next?;
        }
        let owned = crate::ax::snapshot::RetainedElement::retain(current as usize);
        CFRelease(current as _);
        Some(owned)
    }
}

/// A Save item (Command-S): it keeps invoke_menu's route, which verifies
/// the write on disk and has a scripting fallback.
fn menu_item_is_save(item: AXUIElementRef) -> bool {
    unsafe {
        copy_string_attr(item, "AXMenuItemCmdChar").is_some_and(|ch| ch.eq_ignore_ascii_case("s"))
            && crate::ax::bindings::copy_number_attr(item, "AXMenuItemCmdModifiers").unwrap_or(0.0)
                == 0.0
    }
}

/// What the chord route did, decided on its blocking thread.
enum ChordStep {
    /// The item was pressed (or may have been).
    Pressed {
        text: String,
        may_have_run: bool,
        validation: crate::input::skylight::ValidationPost,
    },
    /// Nothing from the menu was sent.
    Refused {
        code: &'static str,
        text: String,
        /// What menu validation posted before the refusal, if anything.
        validation: crate::input::skylight::ValidationPost,
    },
    /// A Save item: continue on invoke_menu's route under the same scope.
    Save {
        scope: crate::input::skylight::WindowKeyScope,
        path: Vec<String>,
    },
    /// The item still reads disabled with the window held as key and after
    /// validation: the chord goes to that window as keys, once the keyboard
    /// gate passes (coordinator decision keys_under_scope).
    NeedsKeys {
        scope: crate::input::skylight::WindowKeyScope,
        validation: crate::input::skylight::ValidationPost,
        path: String,
        note: String,
    },
    /// The chord was posted as keys to the held key window.
    KeysPosted {
        text: String,
        validation: crate::input::skylight::ValidationPost,
    },
}

/// What a chord whose item does not read enabled after validation does:
/// only a confirmed disabled read sends the chord as keys; an unreadable
/// one is refused with no key or menu action.
#[derive(Debug, PartialEq, Eq)]
enum NotEnabledRoute {
    Keys,
    Unreadable,
    /// A fresh read shows it enabled after all: press it as an enabled item.
    NowEnabled,
}

fn not_enabled_route(last: Option<bool>) -> NotEnabledRoute {
    match last {
        Some(false) => NotEnabledRoute::Keys,
        Some(true) => NotEnabledRoute::NowEnabled,
        None => NotEnabledRoute::Unreadable,
    }
}

/// The step after the chord's key post, with the hand-back notes.
fn keys_posted_step(
    post: crate::input::keyboard::ChordPost,
    path: &str,
    wid: u32,
    validation: crate::input::skylight::ValidationPost,
    notes: &str,
) -> ChordStep {
    match post {
        crate::input::keyboard::ChordPost::Posted => ChordStep::KeysPosted {
            text: format!(
                "{path} reads disabled for window {wid}, so with the window \
                 held as the app's key window the chord was sent to it as keys. This is \
                 likely a no-op; check the window.{notes}"
            ),
            validation,
        },
        crate::input::keyboard::ChordPost::NotPosted(reason) => ChordStep::Refused {
            code: "background_unavailable",
            text: format!(
                "press_key: {path} reads disabled for this window and the chord could not be \
                 built ({reason}), so it was not sent as keys and no menu action was sent.{notes}"
            ),
            validation,
        },
    }
}

/// Post the chord to the held key window: only while it is still the app's
/// focused and main window, then hand key back.
/// Why the chord fallback did not post: the keyboard gate refused, or right
/// before the post the window stopped being the app's focused and main window.
#[derive(Debug, PartialEq, Eq)]
enum KeysHeld {
    GateRefused(String),
    NoLongerTargeted,
}

/// The chord fallback over its side effects (tests drive it with fakes):
/// post only when the gate passed and `still_targeted` holds right before,
/// and hand key back exactly once on every path. Returns the post (or why
/// none was made) and the hand-back's disclosure.
fn run_keys_under_scope<S>(
    scope: S,
    gate: Result<(), String>,
    still_targeted: impl FnOnce() -> bool,
    post: impl FnOnce() -> crate::input::keyboard::ChordPost,
    hand_back: impl FnOnce(S) -> String,
) -> (Result<crate::input::keyboard::ChordPost, KeysHeld>, String) {
    let outcome = match gate {
        Err(reason) => Err(KeysHeld::GateRefused(reason)),
        Ok(()) if !still_targeted() => Err(KeysHeld::NoLongerTargeted),
        Ok(()) => Ok(post()),
    };
    (outcome, hand_back(scope))
}

#[allow(clippy::too_many_arguments)]
fn keys_under_scope(
    pid: i32,
    wid: u32,
    scope: crate::input::skylight::WindowKeyScope,
    gate: Result<(), String>,
    validation: crate::input::skylight::ValidationPost,
    path: String,
    key: &str,
    modifiers: &[String],
) -> ChordStep {
    let mods: Vec<&str> = modifiers.iter().map(String::as_str).collect();
    let (outcome, hand_back_note) = run_keys_under_scope(
        scope,
        gate,
        || menu_bound_to(pid, wid),
        || crate::input::keyboard::press_chord_no_auth(pid, key, &mods),
        |scope| {
            let entry = scope.entry();
            let hand_back = scope.finish();
            crate::input::skylight::scope_disclosure(entry, Some(hand_back))
        },
    );
    keys_step(outcome, &path, wid, validation, &hand_back_note)
}

/// The chord fallback's step from its outcome and notes.
fn keys_step(
    outcome: Result<crate::input::keyboard::ChordPost, KeysHeld>,
    path: &str,
    wid: u32,
    validation: crate::input::skylight::ValidationPost,
    hand_back_note: &str,
) -> ChordStep {
    let mut notes = vec![validation.disclosure(), hand_back_note.to_owned()];
    notes.retain(|note| !note.is_empty());
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!(" {}", notes.join(" "))
    };
    match outcome {
        Ok(post) => keys_posted_step(post, path, wid, validation, &notes),
        Err(KeysHeld::GateRefused(reason)) => ChordStep::Refused {
            code: "background_unavailable",
            text: format!(
                "press_key: {path}: {reason} The requested chord was not sent and no menu action was \
                 sent.{notes}"
            ),
            validation,
        },
        Err(KeysHeld::NoLongerTargeted) => ChordStep::Refused {
            code: "window_not_key",
            text: format!(
                "press_key: {path}: before the keys, the window stopped being the app's focused and \
                 main window. The requested chord was not sent and no menu action was sent.{notes}"
            ),
            validation,
        },
    }
}

fn chord_refusal(
    code: &'static str,
    reason: &str,
    scope: crate::input::skylight::WindowKeyScope,
    validation: crate::input::skylight::ValidationPost,
) -> ChordStep {
    let entry = scope.entry();
    let hand_back = scope.finish();
    let mut text = format!("press_key: {reason} No menu action was sent.");
    for extra in [
        validation.disclosure(),
        crate::input::skylight::scope_disclosure(entry, Some(hand_back)),
    ] {
        if !extra.is_empty() {
            text.push(' ');
            text.push_str(&extra);
        }
    }
    ChordStep::Refused {
        code,
        text,
        validation,
    }
}

/// A chord task that ended without a result, with what validation had
/// already posted kept apart from the requested chord.
fn chord_task_failure(
    message: String,
    validation: crate::input::skylight::ValidationPost,
    chord_may_have_been_sent: bool,
) -> ToolResult {
    let disclosure = validation.disclosure();
    let message = if disclosure.is_empty() {
        message
    } else {
        format!("{message} {disclosure}")
    };
    let mut result = task_failure(message);
    if let Some(serde_json::Value::Object(fields)) = result.structured_content.as_mut() {
        fields.insert(
            "validation_keys_sent".into(),
            serde_json::json!(validation.key_attempts > 0),
        );
        if validation.called {
            fields.insert("dispatch".into(), serde_json::json!("validation_posted"));
        }
        if chord_may_have_been_sent {
            fields.insert("chord_delivery".into(), serde_json::json!("possible"));
        } else {
            fields.insert("chord_sent".into(), serde_json::json!(false));
            fields.insert(
                "keys_sent".into(),
                serde_json::json!(validation.key_attempts > 0),
            );
        }
    }
    result
}

/// The text of a gate refusal, for the chord's own refusal receipt.
fn refusal_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|content| match content {
            cua_driver_core::protocol::Content::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "the keyboard gate refused the window.".to_owned())
}

/// The chord's menu route on one blocking thread: hold the target window as
/// the app's key window, find and validate the item for that window, and
/// press it only while the window is still the app's focused and main
/// window.
fn chord_in_scope(pid: i32, wid: u32, combo: super::menu_equivalent::Combo) -> ChordStep {
    use crate::input::skylight::ValidationPost;
    let scope = match crate::input::skylight::enter_exact_window_key_inline(pid as libc::pid_t, wid)
    {
        Ok(scope) => scope,
        Err(reason) => {
            return ChordStep::Refused {
                code: "window_not_key",
                text: format!("press_key: {reason}. No menu action was sent."),
                validation: crate::input::skylight::ValidationPost::default(),
            }
        }
    };
    if !menu_bound_to(pid, wid) {
        return chord_refusal(
            "window_not_key",
            "the window is not the app's focused and main window, so its menu command could \
             apply to another window.",
            scope,
            ValidationPost::default(),
        );
    }
    let Some(found) = super::menu_equivalent::find(pid, &combo) else {
        return chord_refusal(
            "background_unavailable",
            "the app has no menu item for this key equivalent now.",
            scope,
            ValidationPost::default(),
        );
    };
    let Some(item) = resolve_menu_item(pid, &found.path) else {
        return chord_refusal(
            "background_unavailable",
            "the menu item could not be resolved.",
            scope,
            ValidationPost::default(),
        );
    };
    let element = item.as_ptr() as AXUIElementRef;
    let mut env = LiveChordEnv {
        pid,
        wid,
        element,
        validation: ValidationPost::default(),
        action: None,
        disabled_note: None,
        evidence: None,
    };
    let decision = decide_chord(&mut env);
    let note = env.disabled_note.as_deref().unwrap_or("");
    match decision {
        ChordDecision::Save => ChordStep::Save {
            scope,
            path: found.path,
        },
        ChordDecision::Unbound => chord_refusal(
            "window_not_key",
            &format!("before the press, the window stopped being the app's focused and main window. {note}"),
            scope,
            env.validation,
        ),
        ChordDecision::NoAction => chord_refusal(
            "background_unavailable",
            &format!("the menu item has no advertised press action. {note}"),
            scope,
            env.validation,
        ),
        ChordDecision::Unreadable => chord_refusal(
            "background_unavailable",
            &format!("the menu item's enablement could not be read for this window. {note}"),
            scope,
            env.validation,
        ),
        ChordDecision::Keys(initial_error) => ChordStep::NeedsKeys {
            scope,
            validation: env.validation,
            path: found.path.join(" > "),
            note: format!("{} {}", initial_error.map(|err| format!("An earlier AX action returned error {err}; no menu action was sent.")).unwrap_or_default(), note).trim().to_owned(),
        },
        ChordDecision::Pressed { err, initial_error } => chord_press_outcome(
            scope,
            found.path.join(" > "),
            wid,
            env.action.expect("press has action"),
            err,
            env.validation,
            (initial_error, env.disabled_note),
        ),
    }
}

trait ChordEnv {
    fn is_save(&mut self) -> bool;
    fn enabled(&mut self) -> Option<bool>;
    fn frontmost(&mut self) -> bool;
    fn bound(&mut self) -> bool;
    fn validate(&mut self) -> Option<bool>;
    fn prepare_press(&mut self, disabled: bool) -> bool;
    fn press(&mut self, disabled: bool) -> crate::ax::bindings::AXError;
}

#[derive(Debug, PartialEq, Eq)]
enum ChordDecision {
    Save,
    Unbound,
    NoAction,
    Unreadable,
    Keys(Option<crate::ax::bindings::AXError>),
    Pressed {
        err: crate::ax::bindings::AXError,
        initial_error: Option<crate::ax::bindings::AXError>,
    },
}

fn decide_chord(env: &mut impl ChordEnv) -> ChordDecision {
    if env.is_save() {
        return ChordDecision::Save;
    }
    let initial = env.enabled();
    let frontmost = initial != Some(true) && env.frontmost();
    if initial.is_none() && frontmost {
        return ChordDecision::Unreadable;
    }
    let mut initial_error = None;
    let early = initial == Some(true) || frontmost;
    if early {
        if !env.prepare_press(initial == Some(false)) {
            return ChordDecision::NoAction;
        }
        if !env.bound() {
            return ChordDecision::Unbound;
        }
        let err = env.press(initial == Some(false));
        if err == crate::ax::bindings::kAXErrorSuccess
            || !super::click::perform_error_proves_nothing_was_sent(err)
        {
            return ChordDecision::Pressed {
                err,
                initial_error: None,
            };
        }
        initial_error = Some(err);
    }
    if !early || env.enabled() != Some(true) {
        match not_enabled_route(env.validate()) {
            NotEnabledRoute::Keys => return ChordDecision::Keys(initial_error),
            NotEnabledRoute::Unreadable => return ChordDecision::Unreadable,
            NotEnabledRoute::NowEnabled => {}
        }
    }
    if !env.prepare_press(false) {
        return ChordDecision::NoAction;
    }
    if !env.bound() {
        return ChordDecision::Unbound;
    }
    let err = env.press(false);
    ChordDecision::Pressed { err, initial_error }
}

struct LiveChordEnv {
    pid: i32,
    wid: u32,
    element: AXUIElementRef,
    validation: crate::input::skylight::ValidationPost,
    action: Option<&'static str>,
    disabled_note: Option<String>,
    evidence: Option<PressEvidence>,
}

struct PressEvidence {
    is_close: bool,
    title: Option<String>,
    sheet: Option<bool>,
    windows: Option<std::collections::HashSet<u32>>,
}

/// Returns None when the target window or its children could not be read.
fn window_has_sheet(pid: i32, wid: u32) -> Option<bool> {
    use crate::ax::bindings::{
        ax_get_window_id, copy_ax_windows, copy_children_checked, AXUIElementCreateApplication,
        AXUIElementSetMessagingTimeout,
    };
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
        AXUIElementSetMessagingTimeout(app, 0.05);
        let windows = copy_ax_windows(app);
        CFRelease(app as _);
        let mut result = None;
        for window in windows {
            AXUIElementSetMessagingTimeout(window, 0.05);
            if std::time::Instant::now() < deadline && ax_get_window_id(window) == Some(wid) {
                if let Some(children) = copy_children_checked(window) {
                    let mut found = false;
                    let mut unknown = false;
                    for child in children {
                        AXUIElementSetMessagingTimeout(child, 0.05);
                        match (std::time::Instant::now() < deadline)
                            .then(|| copy_string_attr(child, "AXRole"))
                            .flatten()
                        {
                            Some(role) if role == "AXSheet" => found = true,
                            Some(_) => {}
                            None => unknown = true,
                        }
                        CFRelease(child as _);
                    }
                    result = if found {
                        Some(true)
                    } else if unknown {
                        None
                    } else {
                        Some(false)
                    };
                }
            }
            CFRelease(window as _);
        }
        result
    }
}

impl LiveChordEnv {
    fn record_disabled(
        &mut self,
        evidence: PressEvidence,
        after_title: Option<String>,
        after_windows: Option<std::collections::HashSet<u32>>,
        after_sheet: Option<bool>,
        after_enabled: Option<bool>,
    ) {
        self.disabled_note = Some(disabled_observation(
            self.wid,
            (evidence.title, after_title),
            (
                evidence
                    .windows
                    .as_ref()
                    .map(|windows| windows.contains(&self.wid)),
                after_windows
                    .as_ref()
                    .map(|windows| windows.contains(&self.wid)),
            ),
            (evidence.sheet, after_sheet),
            after_enabled,
        ));
    }
}

impl ChordEnv for LiveChordEnv {
    fn is_save(&mut self) -> bool {
        menu_item_is_save(self.element)
    }
    fn enabled(&mut self) -> Option<bool> {
        unsafe { copy_bool_attr(self.element, "AXEnabled") }
    }
    fn frontmost(&mut self) -> bool {
        app_is_frontmost(self.pid, self.wid)
    }
    fn bound(&mut self) -> bool {
        menu_bound_to(self.pid, self.wid)
    }
    fn validate(&mut self) -> Option<bool> {
        self.validation =
            crate::input::skylight::prime_menu_validation(self.pid as libc::pid_t, self.wid);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        let mut last = self.enabled();
        while last != Some(true) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            last = self.enabled();
        }
        last
    }
    fn prepare_press(&mut self, disabled: bool) -> bool {
        let actions = unsafe { crate::ax::bindings::copy_action_names(self.element) };
        let Some(action) = ["AXPress", "AXPick"]
            .into_iter()
            .find(|name| actions.iter().any(|candidate| candidate == name))
        else {
            return false;
        };
        self.action = Some(action);
        let is_close = disabled
            && unsafe {
                copy_string_attr(self.element, "AXMenuItemCmdChar")
                    .is_some_and(|ch| ch.eq_ignore_ascii_case("w"))
                    && crate::ax::bindings::copy_number_attr(self.element, "AXMenuItemCmdModifiers")
                        .unwrap_or(0.0)
                        == 0.0
            };
        self.evidence = Some(PressEvidence {
            is_close,
            title: disabled
                .then(|| unsafe { copy_string_attr(self.element, "AXTitle") })
                .flatten(),
            sheet: if is_close {
                window_has_sheet(self.pid, self.wid)
            } else {
                None
            },
            windows: if is_close {
                crate::ax::bindings::ax_window_ids_of_pid(self.pid)
            } else {
                None
            },
        });
        true
    }
    fn press(&mut self, disabled: bool) -> crate::ax::bindings::AXError {
        if !disabled {
            self.disabled_note = None;
        }
        let action = self.action.expect("prepared action");
        let err = unsafe { crate::ax::bindings::perform_action(self.element, action) };
        if disabled {
            let evidence = self.evidence.take().expect("prepared evidence");
            let after_enabled = self.enabled();
            let after_title = unsafe { copy_string_attr(self.element, "AXTitle") };
            let after_windows = if evidence.is_close {
                crate::ax::bindings::ax_window_ids_of_pid(self.pid)
            } else {
                None
            };
            let after_sheet = if evidence.is_close {
                window_has_sheet(self.pid, self.wid)
            } else {
                None
            };
            self.record_disabled(
                evidence,
                after_title,
                after_windows,
                after_sheet,
                after_enabled,
            );
        }
        err
    }
}

fn disabled_observation(
    wid: u32,
    titles: (Option<String>, Option<String>),
    windows: (Option<bool>, Option<bool>),
    sheets: (Option<bool>, Option<bool>),
    after_enabled: Option<bool>,
) -> String {
    let (before_title, after_title) = titles;
    let (before_window, after_window) = windows;
    let (before_sheet, after_sheet) = sheets;
    let mut observations = Vec::new();
    if let (Some(before), Some(after)) = (before_title, after_title) {
        if before != after {
            observations.push(format!(
                "The item title changed from ‘{before}’ to ‘{after}’."
            ));
        }
    }
    if before_window == Some(true) && after_window == Some(false) {
        observations.push(format!(
            "Window {wid} is no longer in the app's AX window list."
        ));
    }
    if before_sheet == Some(false) && after_sheet == Some(true) {
        observations.push("A sheet appeared on the target window.".to_owned());
    }
    let enabled = match after_enabled {
        Some(true) => "enabled",
        Some(false) => "disabled",
        None => "unreadable",
    };
    observations.push(format!(
        "The item read disabled before the press; afterwards enablement was {enabled}."
    ));
    observations.join(" ")
}

fn chord_press_outcome(
    scope: crate::input::skylight::WindowKeyScope,
    path: String,
    wid: u32,
    action: &str,
    err: crate::ax::bindings::AXError,
    validation: crate::input::skylight::ValidationPost,
    details: (Option<crate::ax::bindings::AXError>, Option<String>),
) -> ChordStep {
    let entry = scope.entry();
    let hand_back = scope.finish();
    let scope_note = crate::input::skylight::scope_disclosure(entry, Some(hand_back));
    chord_press_step(path, wid, action, err, validation, details, scope_note)
}

fn chord_press_step(
    path: String,
    wid: u32,
    action: &str,
    err: crate::ax::bindings::AXError,
    validation: crate::input::skylight::ValidationPost,
    details: (Option<crate::ax::bindings::AXError>, Option<String>),
    scope_note: String,
) -> ChordStep {
    let (initial_error, disabled_note) = details;
    let mut notes = vec![
        initial_error
            .map(|err| {
                format!("An earlier {action} returned AX error {err}; no menu action was sent.")
            })
            .unwrap_or_default(),
        validation.disclosure(),
        disabled_note.clone().unwrap_or_default(),
        scope_note,
    ];
    notes.retain(|note| !note.is_empty());
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!(" {}", notes.join(" "))
    };
    if err == crate::ax::bindings::kAXErrorSuccess && disabled_note.is_some() {
        ChordStep::Pressed {
            text: format!(
                "Sent {action} to {path} for window {wid}; the requested chord was not sent as keys. Its effect is unverified.{notes}"
            ),
            may_have_run: false,
            validation,
        }
    } else if err == crate::ax::bindings::kAXErrorSuccess {
        ChordStep::Pressed {
            text: format!(
                "Pressed {path} through accessibility for window {wid}; the requested chord was \
                 not sent as keys. Its effect is not read back: check the window.{notes}"
            ),
            may_have_run: false,
            validation,
        }
    } else if super::click::perform_error_proves_nothing_was_sent(err) {
        chord_refusal_after_hand_back(path, err, validation, notes)
    } else {
        ChordStep::Pressed {
            text: format!(
                "Sent {action} to {path} for window {wid}; the app returned AX error {err}, so \
                 the menu action may have run. Read the window before repeating it.{notes}"
            ),
            may_have_run: true,
            validation,
        }
    }
}

fn chord_refusal_after_hand_back(
    path: String,
    err: crate::ax::bindings::AXError,
    validation: crate::input::skylight::ValidationPost,
    notes: String,
) -> ChordStep {
    ChordStep::Refused {
        code: "background_unavailable",
        text: format!(
            "press_key: accessibility refused {path} (AX error {err}); no menu action was \
             sent.{notes}"
        ),
        validation,
    }
}

/// The tool result for a chord step other than Save.
fn chord_result(step: ChordStep) -> ToolResult {
    match step {
        ChordStep::Pressed {
            text,
            may_have_run,
            validation,
        } => ToolResult::text(text)
            .with_structured(serde_json::json!({
                "path": "menu_ax",
                "verified": false,
                "effect": "unverifiable",
                // Only menu validation's Cmd+F20 can have been sent as keys.
                "keys_sent": validation.key_attempts > 0,
                "code": if may_have_run { Some("ax_action_unverified") } else { None },
            }))
            .with_action_record(
                ActionExecutionRecord::builder(
                    ActionEffect::Unverifiable,
                    ActionTransport::MacosAxAction,
                    RequestedDelivery::Background,
                )
                .actual_delivery(ActualDelivery::Background)
                .build()
                .expect("an unverifiable menu record is valid"),
            ),
        ChordStep::Refused {
            code,
            text,
            validation,
        } => {
            let mut structured = serde_json::json!({
                "code": code,
                "effect": "refused",
                "dispatched": false,
                "keys_sent": validation.key_attempts > 0,
                // Error results get no summary: the validation and key
                // focus disclosure travels in the structured message.
                "message": text.clone(),
            });
            if validation.called {
                structured["dispatch"] = serde_json::json!("validation_posted");
            }
            if code == "background_unavailable" {
                structured["escalation"] =
                    serde_json::json!({ "recommended": "foreground", "requires": ["window_id"] });
            }
            ToolResult::error(text).with_structured(structured)
        }
        ChordStep::Save { .. } => unreachable!("Save continues on invoke_menu's route"),
        ChordStep::NeedsKeys { .. } => unreachable!("keys go through the keyboard gate first"),
        ChordStep::KeysPosted { text, validation } => ToolResult::text(text)
            .with_structured(serde_json::json!({
                "path": "keys_under_scope",
                "verified": false,
                "effect": "suspected_noop",
                "keys_sent": true,
                "validation_keys_sent": validation.key_attempts > 0,
            }))
            .with_action_record(
                ActionExecutionRecord::builder(
                    ActionEffect::SuspectedNoop,
                    ActionTransport::MacosCgEventPid,
                    RequestedDelivery::Background,
                )
                .actual_delivery(ActualDelivery::Background)
                .build()
                .expect("a suspected no-op key record is valid"),
            ),
    }
}

/// The window-only native chord: everything from the key scope to the
/// hand-back runs in a task that owns the lease.
pub(super) async fn native_menu_chord(
    lease: super::BackgroundMutationLease,
    wid: u32,
    combo: super::menu_equivalent::Combo,
    display_key: String,
    chord: (String, Vec<String>),
) -> ToolResult {
    let pid = lease.pid();
    let outcome = run_detached(async move {
        let lease = lease;
        let step =
            match latency::spawn_action_blocking(move || chord_in_scope(pid, wid, combo)).await {
                Ok(step) => step,
                Err(error) => {
                    return task_failure(format!(
                    "press_key: the menu task failed ({error}); it may have run. Read the window \
                     before repeating it."
                ))
                }
            };
        match step {
            ChordStep::Save { scope, path } => {
                let entry = scope.entry();
                let result = super::invoke_menu::run_in_background_with_lease(
                    &lease,
                    wid,
                    path,
                    super::invoke_menu::MenuRequest::KeyEquivalent(display_key),
                    super::invoke_menu::KeyOwner::Caller,
                )
                .await;
                let hand_back = latency::spawn_action_blocking(move || scope.finish())
                    .await
                    .unwrap_or(crate::input::skylight::HandBack::NotConfirmed);
                let mut result = super::hotkey::menu_refusal_without_keys(result);
                let disclosure = crate::input::skylight::scope_disclosure(entry, Some(hand_back));
                append_to_summary(&mut result, &disclosure);
                if let Some(serde_json::Value::Object(fields)) = result.structured_content.as_mut()
                {
                    if !disclosure.is_empty() {
                        fields.insert("hand_back_note".into(), serde_json::json!(disclosure));
                    }
                }
                result
            }
            ChordStep::NeedsKeys {
                scope,
                validation,
                path,
                note,
            } => {
                // Raw keys need the keyboard class's facts (a visible, not
                // minimized window of a visible app), on the same lease.
                let gate = lease
                    .gate_again(
                        wid,
                        None,
                        cua_driver_core::background_input::BackgroundAction::ExactWindowKeys,
                    )
                    .await
                    .map_err(|refusal| refusal_text(&refusal));
                let gate_refused = gate.is_err();
                let (key, modifiers) = chord;
                let step = latency::spawn_action_blocking(move || {
                    let mut step =
                        keys_under_scope(pid, wid, scope, gate, validation, path, &key, &modifiers);
                    if !note.is_empty() {
                        match &mut step {
                            ChordStep::KeysPosted { text, .. }
                            | ChordStep::Refused { text, .. } => {
                                text.push(' ');
                                text.push_str(&note);
                            }
                            _ => {}
                        }
                    }
                    step
                })
                .await;
                match step {
                    Ok(step) => chord_result(step),
                    Err(error) if gate_refused => chord_task_failure(
                        format!(
                            "press_key: the key hand-back task failed ({error}); the requested \
                             chord and its menu action were not sent, and key focus inside the \
                             app may remain on the target window."
                        ),
                        validation,
                        false,
                    ),
                    Err(error) => chord_task_failure(
                        format!(
                            "press_key: the key task failed ({error}); the requested chord may \
                             have been sent. Read the window before repeating it."
                        ),
                        validation,
                        true,
                    ),
                }
            }
            step => chord_result(step),
        }
    })
    .await;
    outcome.unwrap_or_else(|error| {
        task_failure(format!(
            "press_key: the menu task failed ({error}); it may have run. Read the window before \
             repeating it."
        ))
    })
}

#[cfg(test)]
pub(super) mod tests {
    struct FakeChord {
        save: bool,
        initial: Option<bool>,
        after: Option<bool>,
        front: bool,
        bound: bool,
        flip_after_first: bool,
        bound_reads: usize,
        error: crate::ax::bindings::AXError,
        presses: usize,
        validations: usize,
        reads: usize,
        disabled_presses: usize,
    }
    impl super::ChordEnv for FakeChord {
        fn is_save(&mut self) -> bool {
            self.save
        }
        fn enabled(&mut self) -> Option<bool> {
            self.reads += 1;
            if self.reads == 1 {
                self.initial
            } else {
                self.after
            }
        }
        fn frontmost(&mut self) -> bool {
            self.front
        }
        fn bound(&mut self) -> bool {
            self.bound_reads += 1;
            self.bound && !(self.flip_after_first && self.bound_reads > 1)
        }
        fn validate(&mut self) -> Option<bool> {
            self.validations += 1;
            self.after
        }
        fn prepare_press(&mut self, _: bool) -> bool {
            true
        }
        fn press(&mut self, disabled: bool) -> crate::ax::bindings::AXError {
            self.presses += 1;
            self.disabled_presses += usize::from(disabled);
            self.error
        }
    }

    pub(crate) fn frontmost_disabled_decision() -> bool {
        let mut env = FakeChord {
            save: false,
            initial: Some(false),
            after: Some(false),
            front: true,
            bound: true,
            flip_after_first: false,
            bound_reads: 0,
            error: crate::ax::bindings::kAXErrorSuccess,
            presses: 0,
            validations: 0,
            reads: 0,
            disabled_presses: 0,
        };
        matches!(
            super::decide_chord(&mut env),
            super::ChordDecision::Pressed { .. }
        ) && env.presses == 1
            && env.disabled_presses == 1
            && env.validations == 0
    }

    #[test]
    fn binding_flip_during_evidence_refuses_without_press() {
        struct Flip {
            bound: bool,
            presses: usize,
        }
        impl super::ChordEnv for Flip {
            fn is_save(&mut self) -> bool {
                false
            }
            fn enabled(&mut self) -> Option<bool> {
                Some(true)
            }
            fn frontmost(&mut self) -> bool {
                true
            }
            fn bound(&mut self) -> bool {
                self.bound
            }
            fn validate(&mut self) -> Option<bool> {
                Some(true)
            }
            fn prepare_press(&mut self, _: bool) -> bool {
                self.bound = false; // Evidence gathering changes the binding.
                true
            }
            fn press(&mut self, _: bool) -> crate::ax::bindings::AXError {
                self.presses += 1;
                crate::ax::bindings::kAXErrorSuccess
            }
        }
        let mut env = Flip {
            bound: true,
            presses: 0,
        };
        assert_eq!(super::decide_chord(&mut env), super::ChordDecision::Unbound);
        assert_eq!(env.presses, 0);
    }

    #[test]
    fn unreadable_frontmost_enablement_is_refused_without_press() {
        let mut env = FakeChord {
            save: false,
            initial: None,
            after: None,
            front: true,
            bound: true,
            flip_after_first: false,
            bound_reads: 0,
            error: crate::ax::bindings::kAXErrorSuccess,
            presses: 0,
            validations: 0,
            reads: 0,
            disabled_presses: 0,
        };
        assert_eq!(
            super::decide_chord(&mut env),
            super::ChordDecision::Unreadable
        );
        assert_eq!(env.presses, 0);
    }

    #[test]
    fn live_disabled_note_flows_into_unverified_chord_result() {
        let mut env = super::LiveChordEnv {
            pid: 1,
            wid: 42,
            element: std::ptr::null_mut(),
            validation: crate::input::skylight::ValidationPost::default(),
            action: Some("AXPress"),
            disabled_note: None,
            evidence: None,
        };
        env.record_disabled(
            super::PressEvidence {
                is_close: false,
                title: Some("Undo".into()),
                sheet: None,
                windows: None,
            },
            Some("Redo".into()),
            None,
            None,
            Some(false),
        );
        let step = super::chord_press_step(
            "Edit > Undo".into(),
            42,
            "AXPress",
            crate::ax::bindings::kAXErrorSuccess,
            env.validation,
            (None, env.disabled_note.take()),
            String::new(),
        );
        match &step {
            super::ChordStep::Pressed { text, .. } => {
                assert!(text.contains("item read disabled before the press"));
                assert!(text.contains("requested chord was not sent as keys"));
                assert!(!text.starts_with("Pressed"));
                assert!(!text.contains("Some("));
            }
            _ => panic!("expected unverified press"),
        }
        assert_eq!(
            super::chord_result(step).structured_content.unwrap()["effect"],
            "unverifiable"
        );
    }

    #[test]
    fn disabled_frontmost_success_reports_observation_without_confirming() {
        for changed in [false, true] {
            let mut env = FakeChord {
                save: false,
                initial: Some(false),
                after: Some(false),
                front: true,
                bound: true,
                flip_after_first: false,
                bound_reads: 0,
                error: crate::ax::bindings::kAXErrorSuccess,
                presses: 0,
                validations: 0,
                reads: 0,
                disabled_presses: 0,
            };
            assert!(matches!(
                super::decide_chord(&mut env),
                super::ChordDecision::Pressed { .. }
            ));
            assert_eq!(
                (env.presses, env.validations, env.disabled_presses),
                (1, 0, 1)
            );
            let note = super::disabled_observation(
                42,
                (
                    Some("Undo".into()),
                    Some(if changed { "Redo" } else { "Undo" }.into()),
                ),
                (None, None),
                (None, None),
                Some(false),
            );
            assert!(note.contains("item read disabled before the press"));
            assert_eq!(note.contains("title changed"), changed);
            let step = super::chord_press_step(
                "Edit > Undo".into(),
                42,
                "AXPress",
                crate::ax::bindings::kAXErrorSuccess,
                crate::input::skylight::ValidationPost::default(),
                (None, Some(note)),
                String::new(),
            );
            if let super::ChordStep::Pressed { text, .. } = &step {
                assert!(text.contains("item read disabled before the press"));
                assert_eq!(text.contains("title changed"), changed);
                assert!(!text.starts_with("Pressed"));
            } else {
                panic!("expected unverified action");
            }
            let result = super::chord_result(step);
            let fields = result.structured_content.expect("structured result");
            assert_eq!(fields["effect"], "unverifiable");
            assert_eq!(fields["code"], serde_json::Value::Null);
            assert_eq!(fields["keys_sent"], false);
        }
    }

    #[test]
    fn chord_rechecks_binding_after_a_proven_no_dispatch_error() {
        let mut env = FakeChord {
            save: false,
            initial: Some(true),
            after: Some(true),
            front: false,
            bound: true,
            flip_after_first: true,
            bound_reads: 0,
            error: crate::ax::bindings::kAXErrorInvalidUIElement,
            presses: 0,
            validations: 0,
            reads: 0,
            disabled_presses: 0,
        };
        assert_eq!(super::decide_chord(&mut env), super::ChordDecision::Unbound);
        assert_eq!((env.presses, env.validations, env.bound_reads), (1, 0, 2));
    }

    fn assert_chord_projection(decision: &super::ChordDecision, bound: bool, label: &str) {
        use super::{ChordDecision, ChordStep};
        let validation = crate::input::skylight::ValidationPost::default();
        let keys = std::cell::Cell::new(0);
        let step = match *decision {
            ChordDecision::Save => return,
            ChordDecision::Pressed { err, initial_error } => super::chord_press_step(
                "Edit > Item".into(),
                42,
                "AXPress",
                err,
                validation,
                (initial_error, None),
                String::new(),
            ),
            ChordDecision::Keys(_) => {
                let (outcome, note) = super::run_keys_under_scope(
                    (),
                    Ok(()),
                    || bound,
                    || {
                        keys.set(keys.get() + 1);
                        crate::input::keyboard::ChordPost::Posted
                    },
                    |_| String::new(),
                );
                super::keys_step(outcome, "Edit > Item", 42, validation, &note)
            }
            ChordDecision::Unbound => ChordStep::Refused {
                code: "window_not_key",
                text: "unbound".into(),
                validation,
            },
            ChordDecision::NoAction | ChordDecision::Unreadable => ChordStep::Refused {
                code: "background_unavailable",
                text: "unavailable".into(),
                validation,
            },
        };
        let fields = super::chord_result(step)
            .structured_content
            .expect("structured result");
        let (effect, code, sent) = match *decision {
            ChordDecision::Pressed { err, .. }
                if err == crate::ax::bindings::kAXErrorInvalidUIElement =>
            {
                ("refused", Some("background_unavailable"), false)
            }
            ChordDecision::Pressed { err, .. } if err == crate::ax::bindings::kAXErrorSuccess => {
                ("unverifiable", None, false)
            }
            ChordDecision::Pressed { .. } => ("unverifiable", Some("ax_action_unverified"), false),
            ChordDecision::Keys(_) if bound => ("suspected_noop", None, true),
            ChordDecision::Keys(_) | ChordDecision::Unbound => {
                ("refused", Some("window_not_key"), false)
            }
            ChordDecision::NoAction | ChordDecision::Unreadable => {
                ("refused", Some("background_unavailable"), false)
            }
            ChordDecision::Save => unreachable!(),
        };
        assert_eq!(fields["effect"], effect, "{label}");
        assert_eq!(
            fields.get("code").and_then(serde_json::Value::as_str),
            code,
            "{label}"
        );
        assert_eq!(fields["keys_sent"], sent, "{label}");
        assert_eq!(keys.get(), usize::from(sent), "{label}");
    }

    #[test]
    fn chord_decisions_cover_ax_results_enabled_save_front_and_binding() {
        use super::ChordDecision as D;
        use crate::ax::bindings::{
            kAXErrorCannotComplete as ambiguous, kAXErrorInvalidUIElement as invalid,
            kAXErrorSuccess as ok,
        };
        let cases = [
            (
                true,
                Some(false),
                Some(false),
                false,
                true,
                ok,
                D::Save,
                0,
                0,
            ),
            (false, None, None, true, true, ok, D::Unreadable, 0, 0),
            (
                false,
                None,
                Some(true),
                false,
                true,
                ok,
                D::Pressed {
                    err: ok,
                    initial_error: None,
                },
                1,
                1,
            ),
            (
                false,
                Some(true),
                Some(true),
                false,
                true,
                ok,
                D::Pressed {
                    err: ok,
                    initial_error: None,
                },
                1,
                0,
            ),
            (
                false,
                Some(false),
                Some(false),
                true,
                true,
                ok,
                D::Pressed {
                    err: ok,
                    initial_error: None,
                },
                1,
                0,
            ),
            (
                false,
                Some(false),
                Some(false),
                false,
                true,
                ok,
                D::Keys(None),
                0,
                1,
            ),
            (
                false,
                Some(false),
                None,
                false,
                true,
                ok,
                D::Unreadable,
                0,
                1,
            ),
            (
                false,
                Some(false),
                Some(true),
                false,
                true,
                ok,
                D::Pressed {
                    err: ok,
                    initial_error: None,
                },
                1,
                1,
            ),
            (
                false,
                Some(true),
                Some(true),
                true,
                false,
                ok,
                D::Unbound,
                0,
                0,
            ),
            (
                false,
                Some(false),
                Some(true),
                false,
                false,
                ok,
                D::Unbound,
                0,
                1,
            ),
            (
                false,
                Some(true),
                Some(true),
                true,
                true,
                ambiguous,
                D::Pressed {
                    err: ambiguous,
                    initial_error: None,
                },
                1,
                0,
            ),
            (
                false,
                Some(true),
                Some(false),
                true,
                true,
                invalid,
                D::Keys(Some(invalid)),
                1,
                1,
            ),
            (
                false,
                Some(true),
                Some(true),
                true,
                true,
                invalid,
                D::Pressed {
                    err: invalid,
                    initial_error: Some(invalid),
                },
                2,
                0,
            ),
        ];
        for (save, initial, after, front, bound, error, expected, presses, validations) in cases {
            let mut env = FakeChord {
                save,
                initial,
                after,
                front,
                bound,
                flip_after_first: false,
                bound_reads: 0,
                error,
                presses: 0,
                validations: 0,
                reads: 0,
                disabled_presses: 0,
            };
            let decision = super::decide_chord(&mut env);
            assert_eq!(decision, expected);
            assert_eq!((env.presses, env.validations), (presses, validations));
            assert_chord_projection(&decision, bound, "explicit case");
        }
    }
    use super::*;

    /// A key is posted once and returns: no read before it, no wait after it
    /// (the old oracle slept 60 ms to read the focused element back), and the
    /// result is unverifiable.
    #[test]
    fn a_key_is_posted_once_without_a_read_back_wait() {
        let posts = std::cell::Cell::new(0);
        let started = std::time::Instant::now();
        let confirmed = dispatch_with_ax_oracle(1, Some(2), Some(3), || {
            posts.set(posts.get() + 1);
            Ok(())
        })
        .expect("posted");
        assert!(!confirmed, "nothing is read back, so nothing is confirmed");
        assert_eq!(posts.get(), 1);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(30),
            "returned after {:?}",
            started.elapsed()
        );
        let failed = dispatch_with_ax_oracle(1, None, None, || anyhow::bail!("not posted"));
        assert!(failed.is_err());
    }

    /// Coordinator decision keys_under_scope: only a confirmed disabled read
    /// sends the chord as keys; an unreadable one sends nothing.
    #[test]
    fn only_a_confirmed_disabled_item_sends_the_chord_as_keys() {
        assert_eq!(not_enabled_route(Some(false)), NotEnabledRoute::Keys);
        assert_eq!(not_enabled_route(None), NotEnabledRoute::Unreadable);
        assert_eq!(not_enabled_route(Some(true)), NotEnabledRoute::NowEnabled);
    }

    #[test]
    fn a_disabled_chord_sent_as_keys_is_a_suspected_noop() {
        let validation = crate::input::skylight::ValidationPost {
            records: 2,
            keys: 2,
            key_attempts: 2,
            uncertain: false,
            called: true,
        };
        let step = keys_posted_step(
            crate::input::keyboard::ChordPost::Posted,
            "Edit > Clear to Start",
            7,
            validation,
            " Key focus went back to the app's previous window.",
        );
        let result = chord_result(step);
        assert_ne!(result.is_error, Some(true));
        let structured = result.structured_content.clone().expect("structured");
        assert_eq!(structured["effect"], "suspected_noop");
        assert_eq!(structured["keys_sent"], true);
        let message = result
            .content
            .iter()
            .map(|v| format!("{v:?}"))
            .collect::<String>();
        assert!(message.contains("likely a no-op"), "{message}");
        assert_eq!(structured["path"], "keys_under_scope");
        let record = result.action_record.expect("record");
        assert!(matches!(record.effect, ActionEffect::SuspectedNoop));
    }

    /// The chord fallback posts only when the keyboard gate passed and the
    /// window and front app still hold right before; key is handed back
    /// exactly once on every path.
    #[test]
    fn the_chord_fallback_posts_only_through_the_gate_and_always_hands_back_once() {
        use crate::input::keyboard::ChordPost;
        let run = |gate: Result<(), String>, targeted: bool| {
            let posts = std::cell::Cell::new(0);
            let hand_backs = std::cell::Cell::new(0);
            let (outcome, note) = run_keys_under_scope(
                "scope",
                gate,
                || targeted,
                || {
                    posts.set(posts.get() + 1);
                    ChordPost::Posted
                },
                |scope| {
                    assert_eq!(scope, "scope");
                    hand_backs.set(hand_backs.get() + 1);
                    "Key focus went back to the app's previous window.".to_owned()
                },
            );
            assert_eq!(hand_backs.get(), 1);
            assert!(!note.is_empty());
            (outcome, posts.get())
        };
        // Hidden, minimized or unknown visibility: the gate refuses.
        let (outcome, posts) = run(Err("window 7 is minimized.".into()), true);
        assert_eq!(posts, 0);
        assert_eq!(
            outcome,
            Err(KeysHeld::GateRefused("window 7 is minimized.".into()))
        );
        // Binding lost.
        let (outcome, posts) = run(Ok(()), false);
        assert_eq!(posts, 0);
        assert_eq!(outcome, Err(KeysHeld::NoLongerTargeted));
        let (outcome, posts) = run(Ok(()), true);
        assert_eq!(posts, 1);
        assert_eq!(outcome, Ok(ChordPost::Posted));

        for (outcome, code) in [
            (
                Err(KeysHeld::GateRefused("hidden.".into())),
                "background_unavailable",
            ),
            (Err(KeysHeld::NoLongerTargeted), "window_not_key"),
        ] {
            let result = chord_result(keys_step(
                outcome,
                "Edit > Clear to Start",
                7,
                crate::input::skylight::ValidationPost::default(),
                "Key focus went back.",
            ));
            let structured = result.structured_content.expect("structured");
            assert_eq!(structured["code"], code);
            assert_eq!(structured["keys_sent"], false);
            assert!(structured["message"]
                .as_str()
                .unwrap()
                .contains("requested chord was not sent"));
        }
    }

    #[test]
    fn a_failed_chord_task_keeps_the_validation_keys_apart_from_the_chord() {
        let validation = crate::input::skylight::ValidationPost {
            records: 2,
            keys: 2,
            key_attempts: 2,
            uncertain: false,
            called: true,
        };
        let not_sent = chord_task_failure("hand-back failed.".into(), validation, false);
        let structured = not_sent.structured_content.clone().expect("structured");
        assert_eq!(structured["chord_sent"], false);
        assert_eq!(structured["keys_sent"], true);
        assert_eq!(structured["validation_keys_sent"], true);
        assert_eq!(structured["dispatch"], "validation_posted");
        assert!(structured["message"].as_str().unwrap().contains("Cmd+F20"));

        let maybe = chord_task_failure("key task failed.".into(), validation, true);
        let structured = maybe.structured_content.expect("structured");
        assert_eq!(structured["chord_delivery"], "possible");
        assert_eq!(structured["effect"], "unverifiable");
        assert!(structured.get("chord_sent").is_none());
    }

    #[test]
    fn a_chord_that_could_not_be_built_is_refused_with_only_validation_keys() {
        let step = keys_posted_step(
            crate::input::keyboard::ChordPost::NotPosted("unknown key".into()),
            "File > Close",
            7,
            crate::input::skylight::ValidationPost::default(),
            "",
        );
        let result = chord_result(step);
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["keys_sent"], false);
        assert!(structured["message"]
            .as_str()
            .unwrap()
            .contains("not sent as keys and no menu action was sent"));
    }
    #[test]
    fn semantic_actions_require_unmodified_return_or_escape_and_live_advertising() {
        let confirm = vec!["AXConfirm".to_owned()];
        let cancel = vec!["AXCancel".to_owned()];
        assert_eq!(
            advertised_semantic_action("return", &[], &confirm),
            Some("AXConfirm")
        );
        assert_eq!(
            advertised_semantic_action("enter", &[], &confirm),
            Some("AXConfirm")
        );
        assert_eq!(
            advertised_semantic_action("escape", &[], &cancel),
            Some("AXCancel")
        );
        assert_eq!(advertised_semantic_action("return", &[], &cancel), None);
        assert_eq!(advertised_semantic_action("escape", &[], &confirm), None);
        assert_eq!(
            advertised_semantic_action("return", &["command".to_owned()], &confirm),
            None
        );
        assert_eq!(advertised_semantic_action("a", &[], &confirm), None);
    }

    #[test]
    fn printable_guard_eligibility_covers_names_and_chords() {
        let shift = |m: &[&str]| m.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // A plain character and a Shift chord are printable.
        assert!(printable_request("x", &[]));
        assert!(printable_request("x", &shift(&["shift"])));
        assert!(printable_request(" ", &[]), "space is printable");
        assert!(
            printable_request("8", &shift(&["shift"])),
            "asterisk resolves to shift+8"
        );
        // Named non-printable keys and other combos are not guarded.
        assert!(!printable_request("\n", &[]), "return is not printable");
        assert!(!printable_request("x", &shift(&["cmd"])));
        assert!(!printable_request("return", &[]));
        // The resolved shape of real requests.
        let (key, mods) = crate::input::keyboard::resolve_key_press("shift+x", &[]).unwrap();
        assert!(printable_request(&key, &mods));
        let (key, mods) = crate::input::keyboard::resolve_key_press("space", &[]).unwrap();
        assert!(printable_request(&key, &mods));
        let (key, mods) = crate::input::keyboard::resolve_key_press("asterisk", &[]).unwrap();
        assert!(printable_request(&key, &mods));
        let (key, mods) = crate::input::keyboard::resolve_key_press("return", &[]).unwrap();
        assert!(!printable_request(&key, &mods));
        let (key, mods) = crate::input::keyboard::resolve_key_press("escape", &[]).unwrap();
        assert!(!printable_request(&key, &mods));
    }

    #[test]
    fn the_summary_names_the_chord_that_was_sent() {
        let shown = |key: &str, modifiers: &[&str]| {
            let modifiers: Vec<String> = modifiers.iter().map(|m| (*m).to_owned()).collect();
            let (key_raw, key, modifiers) =
                requested_key(&serde_json::json!({ "key": key, "modifiers": modifiers }))
                    .expect("a pressable key");
            pressed_key_display(&key_raw, &key, &modifiers)
        };
        assert_eq!(shown("*", &[]), "shift+8 (*)");
        // GOAT's proxy resolves `*` before the driver sees it.
        assert_eq!(shown("8", &["shift"]), "shift+8");
        assert_eq!(shown("return", &[]), "return");
        assert_eq!(shown("a", &["cmd"]), "cmd+a");
    }

    #[tokio::test]
    async fn an_unknown_key_is_refused_before_the_target_is_resolved() {
        let tool = PressKeyTool::new(std::sync::Arc::new(ToolState::new(false, false, None)));
        for args in [
            serde_json::json!({"key": "notakey", "element_token": "s7fffffff:0"}),
            serde_json::json!({"key": "notakey"}),
        ] {
            let result = tool.invoke(args.clone()).await;
            let structured = result.structured_content.expect("structured refusal");
            assert_eq!(
                structured["code"], "invalid_arguments",
                "{args}: {structured}"
            );
            assert!(
                structured["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("notakey")),
                "{args}: {structured}"
            );
        }
    }

    #[test]
    fn targeted_key_requires_focus_readback_before_dispatch() {
        use std::cell::RefCell;
        let events = RefCell::new(Vec::new());
        let result = with_confirmed_key_focus(
            Some(42),
            |target| {
                events.borrow_mut().push(("focus", target));
                Ok(())
            },
            |target| {
                events.borrow_mut().push(("verify", target));
                false
            },
            || {
                events.borrow_mut().push(("dispatch", 0));
                Ok(())
            },
        );
        let failure = delivery_failed(result.unwrap_err());
        assert_eq!(
            failure.structured_content.as_ref().unwrap()["code"],
            "target_focus_unconfirmed"
        );
        assert_eq!(
            failure.structured_content.as_ref().unwrap()["effect"],
            "refused"
        );
        let events = events.borrow();
        assert!(events.len() >= 4);
        for pair in events.chunks(2) {
            assert_eq!(pair, &[("focus", 42), ("verify", 42)]);
        }
    }

    #[test]
    fn targeted_key_retries_focus_before_dispatch() {
        use std::cell::RefCell;
        let events = RefCell::new(Vec::new());
        let mut checks = 0;
        with_confirmed_key_focus(
            Some(42),
            |target| {
                events.borrow_mut().push(("focus", target));
                Ok(())
            },
            |target| {
                events.borrow_mut().push(("verify", target));
                checks += 1;
                checks == 2
            },
            || {
                events.borrow_mut().push(("dispatch", 0));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            *events.borrow(),
            vec![
                ("focus", 42),
                ("verify", 42),
                ("focus", 42),
                ("verify", 42),
                ("dispatch", 0)
            ]
        );
    }

    #[test]
    fn confirmed_target_dispatches_once_and_window_keys_do_not_focus() {
        use std::cell::Cell;
        let sent = Cell::new(0);
        for target in [Some(42), None] {
            with_confirmed_key_focus(
                target,
                |_| Ok(()),
                |_| true,
                || {
                    sent.set(sent.get() + 1);
                    Ok(())
                },
            )
            .unwrap();
        }
        assert_eq!(sent.get(), 2);
        let failed = with_confirmed_key_focus(
            Some(42),
            |_| anyhow::bail!("focus unavailable"),
            |_| panic!("must not confirm failed focus"),
            || -> anyhow::Result<()> { panic!("must not dispatch") },
        );
        assert_eq!(failed.unwrap_err().to_string(), "focus unavailable");
        with_confirmed_key_focus(
            None,
            |_| panic!("window key has no element"),
            |_| panic!("window key has no element"),
            || Ok(()),
        )
        .unwrap();
    }

    #[test]
    fn delivery_outcome_mapper_distinguishes_confirmed_unverifiable_and_failed() {
        assert!(matches!(
            map_delivery_outcome(Ok(true)),
            PressKeyDeliveryOutcome::Confirmed
        ));
        assert!(matches!(
            map_delivery_outcome(Ok(false)),
            PressKeyDeliveryOutcome::Unverifiable
        ));
        let failed = map_delivery_outcome(Err(anyhow::anyhow!("post rejected")));
        assert!(matches!(failed, PressKeyDeliveryOutcome::Failed(_)));
    }

    #[test]
    fn accepted_without_oracle_has_no_escalation_but_ax_change_confirms() {
        let unverifiable = action_record(false, false).public_result().unwrap();
        assert_eq!(
            unverifiable.effect,
            cua_driver_contract::ActionEffect::Unverifiable
        );
        assert_eq!(
            unverifiable.delivery.unwrap().mode,
            cua_driver_contract::ActionDeliveryMode::Background
        );
        assert!(unverifiable.escalation.is_none());

        let confirmed = action_record(true, false).public_result().unwrap();
        assert_eq!(
            confirmed.effect,
            cua_driver_contract::ActionEffect::Confirmed
        );
        assert_eq!(confirmed.evidence.unwrap().len(), 1);
        assert!(confirmed.escalation.is_none());
    }

    #[test]
    fn definitely_dead_pid_is_a_typed_delivery_failure() {
        let child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn short-lived child");
        let pid = child.id() as i32;
        let mut child = child;
        child.wait().expect("wait for child exit");
        let failure = delivery_failed(validate_post_target(pid).unwrap_err());
        assert_eq!(failure.is_error, Some(true));
        assert_eq!(
            failure.structured_content.unwrap()["code"],
            "delivery_failed"
        );
    }

    #[test]
    fn web_or_safari_fallback_runs_once_when_ax_focus_fails_to_stick() {
        use std::cell::Cell;
        let fallback_calls = Cell::new(0usize);
        let focused = Cell::new(false);
        let dispatched = Cell::new(0usize);
        with_confirmed_key_focus_or_fallback(
            Some(99),
            |_| Ok(()),
            |_| focused.get(),
            |target| {
                assert_eq!(target, 99);
                fallback_calls.set(fallback_calls.get() + 1);
                focused.set(true);
                Ok(true)
            },
            || {
                dispatched.set(dispatched.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(fallback_calls.get(), 1);
        assert_eq!(dispatched.get(), 1);
    }

    /// Returns what press_key reports when its pointer focus fallback clicked
    /// but focus still did not stick, so the test sees the published result.
    struct ClickedButUnfocusedProbe;

    #[async_trait::async_trait]
    impl Tool for ClickedButUnfocusedProbe {
        fn def(&self) -> &ToolDef {
            def()
        }

        async fn invoke(&self, _args: serde_json::Value) -> ToolResult {
            let error = with_confirmed_key_focus_or_fallback(
                Some(99),
                |_| Ok(()),
                |_| false,
                |_| Ok(true),
                || -> anyhow::Result<()> { panic!("no key may be sent without focus") },
            )
            .unwrap_err();
            delivery_failed(error)
        }
    }

    #[tokio::test]
    async fn a_fallback_click_without_focus_is_not_reported_as_nothing_sent() {
        let mut registry = cua_driver_core::tool::ToolRegistry::new();
        registry.register(Box::new(ClickedButUnfocusedProbe));
        let result = registry
            .invoke(
                "press_key",
                serde_json::json!({"pid": 123, "window_id": 7, "key": "return"}),
            )
            .await;
        let structured = result.structured_content.expect("published action result");
        assert_ne!(result.is_error, Some(true), "{structured}");
        assert_eq!(structured["effect"], "unverifiable", "{structured}");
        assert_eq!(structured["route"], "synthetic_events", "{structured}");
        assert!(structured.get("dispatched").is_none(), "{structured}");
        let summary = structured["summary"].as_str().unwrap_or_default();
        assert!(
            summary.contains("no key was dispatched") && summary.contains("click may have acted"),
            "{structured}"
        );
        let hint = structured["hint"]
            .as_str()
            .expect("a read-before-repeat hint");
        assert!(
            hint.contains("Read what changed") && !hint.contains("The key was sent"),
            "{structured}"
        );
        assert_eq!(
            structured["next_call"]["tool"], "get_window_state",
            "{structured}"
        );
    }

    #[test]
    fn window_focus_conflict_maps_to_refused_structured_result() {
        let sibling_err = map_focus_conflict_to_refusal(
            42,
            crate::ax::exact_target::WindowFocusConflict::SiblingWindowFocused {
                target_window_id: 700,
                sibling_window_id: 701,
            },
        )
        .unwrap();
        let result = delivery_failed(sibling_err.into());
        let structured = result.structured_content.unwrap();
        assert_eq!(
            structured["code"],
            cua_driver_core::background_input::refusal_codes::SAME_PID_KEYBOARD_AMBIGUITY
        );
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
        assert_eq!(structured["window_id"], 700);
        assert_eq!(structured["sibling_window_id"], 701);
    }

    #[test]
    fn macos_foreground_idle_gate_refuses_active_user_and_allows_idle() {
        let err = super::super::gate_foreground_physical_idle_with_sample(
            42,
            Some(700),
            Some(120),
            1_000,
        )
        .unwrap_err();
        let structured = err.structured_content.unwrap();
        assert_eq!(structured["code"], "foreground_user_active");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
        assert_eq!(structured["pid"], 42);
        assert_eq!(structured["window_id"], 700);

        assert!(super::super::gate_foreground_physical_idle_with_sample(
            42,
            Some(700),
            Some(1_500),
            1_000,
        )
        .is_ok());
    }
}

#[cfg(test)]
mod menu_chord_tests {
    use super::*;

    #[tokio::test]
    async fn a_detached_menu_task_keeps_the_lease_after_the_caller_is_dropped() {
        let pid = -91_777;
        let lease = crate::background_mutation::acquire(pid).await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let caller = tokio::spawn(run_detached(async move {
            let _lease = lease;
            let _ = started_tx.send(());
            let _ = release_rx.await;
            let _ = done_tx.send(());
        }));
        started_rx.await.expect("the menu task started");
        caller.abort();
        assert!(caller.await.is_err(), "the caller was cancelled");
        assert!(
            crate::background_mutation::try_acquire(pid).is_none(),
            "the lease stays with the detached task"
        );
        release_tx.send(()).expect("the menu task is still running");
        done_rx.await.expect("the menu task finished");
        // Released once the task ends.
        drop(crate::background_mutation::acquire(pid).await);
    }

    #[test]
    fn a_refused_chord_says_no_menu_action_and_flags_posted_validation() {
        let refused = chord_result(ChordStep::Refused {
            code: "background_unavailable",
            text: "press_key: Format > Bold is disabled for this window. No menu action was sent."
                .into(),
            validation: crate::input::skylight::ValidationPost {
                records: 2,
                keys: 2,
                key_attempts: 2,
                uncertain: false,
                called: true,
            },
        });
        assert_eq!(refused.is_error, Some(true));
        let structured = refused.structured_content.as_ref().expect("structured");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
        assert_eq!(structured["dispatch"], "validation_posted");
        assert_eq!(structured["keys_sent"], true, "the Cmd+F20 pair went out");
        assert_eq!(structured["escalation"]["recommended"], "foreground");

        let not_key = chord_result(ChordStep::Refused {
            code: "window_not_key",
            text: "press_key: x. No menu action was sent.".into(),
            validation: crate::input::skylight::ValidationPost::default(),
        });
        let structured = not_key.structured_content.as_ref().expect("structured");
        assert!(structured.get("dispatch").is_none());
        assert!(structured.get("escalation").is_none());
    }

    #[test]
    fn a_pressed_chord_is_unverifiable_and_an_ax_error_may_have_run() {
        let pressed = chord_result(ChordStep::Pressed {
            text: "Pressed Format > Font > Bold".into(),
            validation: crate::input::skylight::ValidationPost::default(),
            may_have_run: false,
        });
        assert_ne!(pressed.is_error, Some(true));
        let structured = pressed.structured_content.as_ref().expect("structured");
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["keys_sent"], false);
        assert!(structured["code"].is_null());
        let maybe = chord_result(ChordStep::Pressed {
            text: "Sent AXPress".into(),
            validation: crate::input::skylight::ValidationPost::default(),
            may_have_run: true,
        });
        let structured = maybe.structured_content.as_ref().expect("structured");
        assert_eq!(structured["code"], "ax_action_unverified");
    }
}

#[cfg(test)]
mod minimized_key_receipt_tests {
    use super::*;

    #[test]
    fn a_failed_minimized_key_after_focus_writes_is_not_a_nothing_sent_refusal() {
        let step = MinimizedKeyStep {
            outcome: Some(Err(KeyFocusUnconfirmed.into())),
            refusal: None,
            notes: String::new(),
            focus_writes: 2,
        };
        let result = minimized_key_result(step, 1, "return");
        let structured = result.structured_content.as_ref().expect("structured");
        assert_eq!(structured["effect"], "unverifiable");
        assert!(structured.get("dispatched").is_none());
        assert_eq!(structured["keys_sent"], false);
        assert_eq!(structured["focus_writes_sent"], 2);

        let untouched = MinimizedKeyStep {
            outcome: Some(Err(KeyFocusUnconfirmed.into())),
            refusal: None,
            notes: String::new(),
            focus_writes: 0,
        };
        let result = minimized_key_result(untouched, 1, "return");
        let structured = result.structured_content.as_ref().expect("structured");
        assert!(structured.get("focus_writes_sent").is_none());

        // A failure that does not prove no key event went out.
        let maybe_key = MinimizedKeyStep {
            outcome: Some(Err(anyhow::anyhow!("key-up event could not be created"))),
            refusal: None,
            notes: String::new(),
            focus_writes: 1,
        };
        let result = minimized_key_result(maybe_key, 1, "return");
        let structured = result.structured_content.as_ref().expect("structured");
        assert!(structured.get("keys_sent").is_none());
        assert_eq!(structured["focus_writes_sent"], 1);

        // Window-only (no focus write) with a possibly sent key.
        let window_only = MinimizedKeyStep {
            outcome: Some(Err(anyhow::anyhow!("key-up event could not be created"))),
            refusal: None,
            notes: String::new(),
            focus_writes: 0,
        };
        let result = minimized_key_result(window_only, 1, "return");
        let structured = result.structured_content.as_ref().expect("structured");
        assert_eq!(structured["effect"], "unverifiable");
        assert_ne!(result.is_error, Some(true));
        assert_eq!(
            result.action_record.as_ref().map(|r| r.effect),
            Some(ActionEffect::Unverifiable)
        );
        assert!(structured.get("keys_sent").is_none());
        // In the first text block: the published summary.
        assert!(matches!(
            result.content.first(),
            Some(cua_driver_core::protocol::Content::Text { text, .. }) if text.contains("may have been sent")
        ));
    }
}
