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
use crate::ax::bindings::{
    copy_bool_attr, copy_string_attr, focused_element_of_pid, AXUIElementRef,
};
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct AxKeyState {
    value: Option<String>,
    selected: Option<bool>,
}

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

fn read_ax_key_state(pid: i32, window_id: Option<u32>, element_ptr: usize) -> Option<AxKeyState> {
    if super::type_text::target_in_web_area(pid, Some((element_ptr, None)), window_id) {
        return None;
    }
    let element = element_ptr as AXUIElementRef;
    let state = AxKeyState {
        value: unsafe { copy_string_attr(element, "AXValue") },
        selected: unsafe { copy_bool_attr(element, "AXSelected") },
    };
    (state.value.is_some() || state.selected.is_some()).then_some(state)
}

fn dispatch_with_ax_oracle(
    pid: i32,
    window_id: Option<u32>,
    explicit_element_ptr: Option<usize>,
    dispatch: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    let (element_ptr, owns_element) = match explicit_element_ptr {
        Some(ptr) => (Some(ptr), false),
        None => unsafe {
            match window_id {
                Some(wid) => crate::ax::exact_target::focused_element_in_window(pid, wid),
                None => focused_element_of_pid(pid),
            }
        }
        .map(|element| (Some(element as usize), true))
        .unwrap_or((None, false)),
    };
    let before = element_ptr.and_then(|ptr| read_ax_key_state(pid, window_id, ptr));
    let result = dispatch();
    // Native controls normally publish their new value/selection on the next
    // run-loop turn. Keep this bounded and reuse the exact retained element so
    // a focus move cannot become false confirmation from a different control.
    if before.is_some() {
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    let after = element_ptr.and_then(|ptr| read_ax_key_state(pid, window_id, ptr));
    if owns_element {
        if let Some(ptr) = element_ptr {
            unsafe { CFRelease(ptr as _) };
        }
    }
    result?;
    Ok(matches!((before, after), (Some(before), Some(after)) if ax_state_changed(&before, &after)))
}

fn ax_state_changed(before: &AxKeyState, after: &AxKeyState) -> bool {
    matches!((&before.value, &after.value), (Some(before), Some(after)) if before != after)
        || matches!((before.selected, after.selected), (Some(before), Some(after)) if before != after)
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
    #[error("addressed element does not belong to target window {target_window_id} (resolved: {actual_window_id:?})")]
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
                        dispatch_with_ax_oracle(pid, Some(wid), Some(ptr), || {
                            crate::input::ax_actions::perform_ax_action(ptr, action)
                        })
                    })
                    .await;
                    return match result {
                        Ok(Ok(confirmed)) => {
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

        // ── Menu key equivalents in a background app ──
        // Background key events reach a window's views but never its app's
        // menu key equivalents: a background cmd+a to TextEdit was reported
        // "Pressed" and selected nothing. After the exact-target gate, the key
        // scope and the focus-conflict check above, and under the same
        // per-PID lease, a Command combo that is a menu item's key equivalent
        // runs that item through accessibility, or is refused with no keys
        // sent, as hotkey does. Browsers, Electron, a frontmost app and the
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
                    let in_front = apps::frontmost_pid() == Some(pid);
                    if native && !in_front {
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
                    return native_menu_chord(lease, wid, combo, display_key.clone()).await;
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
                            crate::input::keyboard::press_key(pid, &key, &m)
                        },
                        )
                    })
                })
                .await
            },
        )
        .await;

        let changes = super::finish_window_observation(snapshot).await;

        let delivery_outcome = match result {
            Ok(result) => map_delivery_outcome(result),
            Err(error) => {
                PressKeyDeliveryOutcome::Failed(anyhow::anyhow!("posting task failed: {error}"))
            }
        };

        let result = match delivery_outcome {
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
            // Every focus write, retries included, reads the front app first.
            |target| {
                if !restore.front_still_ok() {
                    let earlier = match focus_writes.get() {
                        0 => "no focus write was sent".to_owned(),
                        n => format!("{n} earlier focus write(s) may have reached the element"),
                    };
                    anyhow::bail!(
                        "the app came to the front, or which app is in front could not be read; {earlier} and no key was sent"
                    );
                }
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
                // The last read before the post.
                if !restore.front_still_ok() {
                    anyhow::bail!(
                        "the app came to the front, or which app is in front could not be read; no key was sent"
                    );
                }
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
            return ToolResult::error(format!(
                "press_key: the minimized-window task failed ({error}); a key may have been sent \
                 and the window may be left restored. Read the window before repeating it."
            ))
            .with_structured(serde_json::json!({
                "effect": "unverifiable",
                "code": "ax_action_unverified",
            }))
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
        return ToolResult::error(format!("press_key: {text}{notes}")).with_structured(
            serde_json::json!({
                "code": code,
                "effect": "refused",
                "dispatched": false,
                "keys_sent": false,
            }),
        );
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
            // Focus writes went out: never a nothing-sent refusal.
            if step.focus_writes > 0 {
                if let Some(serde_json::Value::Object(fields)) = result.structured_content.as_mut()
                {
                    if fields.get("effect").and_then(|v| v.as_str()) == Some("refused") {
                        fields.insert("effect".into(), serde_json::json!("unverifiable"));
                        fields.remove("dispatched");
                    }
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
                result
                    .content
                    .push(cua_driver_core::protocol::Content::text(format!(
                        "{} focus write(s) were sent to the element; {}",
                        step.focus_writes,
                        if before_any_key {
                            "no key was sent."
                        } else {
                            "a key event may also have been sent. Read the window before repeating it."
                        }
                    )));
            }
            if !notes.is_empty() {
                result
                    .content
                    .push(cua_driver_core::protocol::Content::text(
                        notes.trim_start().to_owned(),
                    ));
            }
            result
        }
        None => ToolResult::error(format!("press_key: no key was sent.{notes}")),
    }
}

// ── Window-only native menu chord (key scope + own menu dispatch) ───────────

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

/// Another app is frontmost (the person is not using the target app).
fn front_is_other_app(pid: i32, wid: u32) -> bool {
    crate::input::skylight::front_process_matches(pid as libc::pid_t, wid) == Some(false)
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

/// The chord's menu route on one blocking thread: hold the target window as
/// the app's key window, find and validate the item for that window, and
/// press it only while the window is still the app's focused and main
/// window and another app is still in front.
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
    if menu_item_is_save(element) {
        return ChordStep::Save {
            scope,
            path: found.path,
        };
    }
    let enabled = || unsafe { copy_bool_attr(element, "AXEnabled") } == Some(true);
    let mut validation = ValidationPost::default();
    if !enabled() {
        validation =
            crate::input::skylight::prime_menu_validation_guarded(pid as libc::pid_t, wid, || {
                front_is_other_app(pid, wid)
            });
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        while !enabled() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !enabled() {
            return chord_refusal(
                "background_unavailable",
                &format!("{} is disabled for this window.", found.path.join(" > ")),
                scope,
                validation,
            );
        }
    }
    let actions = unsafe { crate::ax::bindings::copy_action_names(element) };
    let Some(action) = ["AXPress", "AXPick"]
        .into_iter()
        .find(|name| actions.iter().any(|action| action == name))
    else {
        return chord_refusal(
            "background_unavailable",
            "the menu item has no press action.",
            scope,
            validation,
        );
    };
    // The last checks, immediately before the one dispatch.
    if !(menu_bound_to(pid, wid) && front_is_other_app(pid, wid)) {
        return chord_refusal(
            "window_not_key",
            "before the press, the window stopped being the app's focused and main window, or \
             the app came to the front or which app is in front could not be read.",
            scope,
            validation,
        );
    }
    let err = unsafe { crate::ax::bindings::perform_action(element, action) };
    let entry = scope.entry();
    let hand_back = scope.finish();
    let path = found.path.join(" > ");
    let mut notes = vec![
        validation.disclosure(),
        crate::input::skylight::scope_disclosure(entry, Some(hand_back)),
    ];
    notes.retain(|note| !note.is_empty());
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!(" {}", notes.join(" "))
    };
    if err == crate::ax::bindings::kAXErrorSuccess {
        ChordStep::Pressed {
            text: format!(
                "Pressed {path} through accessibility for window {wid}, with the app in the \
                 background (not activated); the requested chord was not sent as keys. Its \
                 effect is not read back: check the window.{notes}"
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
    }
}

/// The window-only native chord: everything from the key scope to the
/// hand-back runs in a task that owns the lease.
async fn native_menu_chord(
    lease: super::BackgroundMutationLease,
    wid: u32,
    combo: super::menu_equivalent::Combo,
    display_key: String,
) -> ToolResult {
    let pid = lease.pid();
    let outcome = run_detached(async move {
        let lease = lease;
        let step = match latency::spawn_action_blocking(move || chord_in_scope(pid, wid, combo)).await {
            Ok(step) => step,
            Err(error) => {
                return ToolResult::error(format!(
                    "press_key: the menu task failed ({error}); it may have run. Read the window \
                     before repeating it."
                ))
                .with_structured(serde_json::json!({
                    "effect": "unverifiable",
                    "code": "ax_action_unverified",
                }))
            }
        };
        match step {
            ChordStep::Save { scope, path } => {
                if !front_is_other_app(pid, wid) {
                    let step = latency::spawn_action_blocking(move || {
                        chord_refusal(
                            "window_not_key",
                            "the app came to the front, or which app is in front could not be read.",
                            scope,
                            crate::input::skylight::ValidationPost::default(),
                        )
                    })
                    .await;
                    return match step {
                        Ok(step) => chord_result(step),
                        Err(error) => ToolResult::error(format!("press_key: {error}")),
                    };
                }
                let result = super::invoke_menu::run_in_background_with_lease(
                    &lease,
                    wid,
                    path,
                    super::invoke_menu::MenuRequest::KeyEquivalent(display_key),
                )
                .await;
                let _ = latency::spawn_action_blocking(move || scope.finish()).await;
                super::hotkey::menu_refusal_without_keys(result)
            }
            step => chord_result(step),
        }
    })
    .await;
    outcome.unwrap_or_else(|error| {
        ToolResult::error(format!(
            "press_key: the menu task failed ({error}); it may have run. Read the window before \
             repeating it."
        ))
        .with_structured(serde_json::json!({
            "effect": "unverifiable",
            "code": "ax_action_unverified",
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_ne!(structured["effect"], "refused");
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
    }
}
