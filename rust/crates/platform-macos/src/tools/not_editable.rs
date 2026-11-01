//! The not_editable guard (handoff 030 section 3.7, plan 040 U4a).
//!
//! Typing into a non-editing cell of a row/outline/table changes selection
//! or triggers type-ahead instead of editing. These guards refuse before
//! any focus action or key reaches such a cell, and report focus actions
//! truthfully when one preceded the refusal.

use cua_driver_core::action_record::{
    ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
};
use cua_driver_core::protocol::ToolResult;

use crate::ax::bindings::{
    ax_get_window_id, copy_element_attr, copy_string_attr, element_at_screen_position_bounded,
    AXUIElementRef, AXUIElementSetMessagingTimeout,
};
use crate::ax::editable::{
    cell_verdict, facts_for_element_with_deadline, is_text_role, token_refused, CellVerdict,
    ElementFacts,
};
use crate::goat::action_latency as latency;
use core_foundation::base::{CFRelease, CFTypeRef};

/// What happened before a not_editable refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusDispatch {
    /// No focus action was attempted: proven pre-dispatch.
    None,
    /// An AX focus action was sent (background accessibility rung).
    SentAx,
    /// A real foreground click was sent.
    SentClick,
    /// A focus action may have been sent; its outcome is unknown.
    Indeterminate,
}

/// The not_editable result. A refusal after a focus attempt never claims
/// nothing was sent: the focus action is reported as delivered (partial)
/// or possibly delivered (unverifiable, delivery unknown).
/// Attach a focus receipt to a later result. A refusal or an unverifiable
/// outcome that follows a delivered focus action must not read as though
/// nothing happened: it becomes partial, with the actual focus route in the
/// public record. An error whose message starts with "not_editable" and has
/// no receipt becomes the structured not_editable refusal.
pub(crate) fn account_focus_receipt(
    mut result: ToolResult,
    receipt: Option<FocusDispatch>,
) -> ToolResult {
    let focus = receipt.unwrap_or(FocusDispatch::None);
    // No focus action (no receipt, or an already-focused point): a late
    // not_editable error still becomes the structured refusal.
    if matches!(focus, FocusDispatch::None) {
        if result.is_error == Some(true) {
            let message = text_of_result(&result);
            if message.contains("not_editable") {
                return not_editable_result(message, FocusDispatch::None);
            }
        }
        return result;
    }
    if !matches!(focus, FocusDispatch::SentAx | FocusDispatch::SentClick) {
        return result;
    }
    if result.is_error != Some(true) || result.action_record.is_some() {
        return result;
    }
    let effect = result
        .structured_content
        .as_ref()
        .and_then(|v| v["effect"].as_str())
        .unwrap_or("")
        .to_owned();
    let code = result
        .structured_content
        .as_ref()
        .and_then(|v| v["code"].as_str())
        .unwrap_or("")
        .to_owned();
    // Only a proven pre-key refusal (or the focus-unconfirmed outcome,
    // where the focus is known to be sent and no keys went out) becomes
    // partial. An error after text or keys may have been sent keeps its
    // uncertainty — a paste that may have landed is NOT a partial focus.
    // An uncertain REAL click after a delivered AX focus is not a partial
    // AX result: name both facts and keep the uncertainty.
    if code == "focus_dispatch_unverified" {
        if let Some(serde_json::Value::Object(fields)) = result.structured_content.as_mut() {
            fields.insert("ax_focus_delivered".into(), serde_json::json!(true));
        }
        let original = text_of_result(&result);
        let summary = format!(
            "The accessibility focus action was delivered, and a real click may also have been sent. {original}"
        );
        for content in result.content.iter_mut() {
            if let cua_driver_core::protocol::Content::Text { text, .. } = content {
                *text = summary.clone();
            }
        }
        return result;
    }
    let pre_key_refusal = effect == "refused"
        || code == "pixel_focus_unconfirmed"
        || (effect.is_empty() && text_of_result(&result).contains("not_editable"));
    if !pre_key_refusal {
        return result;
    }
    // The registry publishes action records only for non-error results:
    // a partial outcome is not a hard error.
    result.is_error = None;
    let (transport, delivery) = if matches!(focus, FocusDispatch::SentClick) {
        (ActionTransport::MacosCgEventHid, ActualDelivery::Foreground)
    } else {
        (ActionTransport::MacosAxAction, ActualDelivery::Background)
    };
    if let Some(serde_json::Value::Object(fields)) = result.structured_content.as_mut() {
        fields.insert("effect".into(), serde_json::json!("partial"));
        fields.insert("focus_action".into(), serde_json::json!("sent"));
    } else {
        let message = text_of_result(&result);
        result.structured_content = Some(serde_json::json!({
            "effect": "partial",
            "focus_action": "sent",
            "message": message,
        }));
    }
    result.action_record = Some(
        ActionExecutionRecord::builder(
            ActionEffect::Partial,
            transport,
            RequestedDelivery::Background,
        )
        .actual_delivery(delivery)
        .delivered_count(1)
        .build()
        .expect("focus receipt record is valid"),
    );
    // The public summary keeps the actionable reason, without any
    // "nothing was sent" claim that a delivered focus contradicts.
    let original = text_of_result(&result);
    let reason = original
        .split(". ")
        .filter(|part| !part.to_lowercase().contains("nothing was sent"))
        .collect::<Vec<_>>()
        .join("; ");
    let summary = format!(
        "The focus action was delivered; the key or text was refused ({}): {} Read fresh state          before retrying.",
        if code.is_empty() || original.contains("not_editable") {
            "not_editable".to_owned()
        } else {
            code
        },
        reason
    );
    for content in result.content.iter_mut() {
        if let cua_driver_core::protocol::Content::Text { text, .. } = content {
            *text = summary.clone();
        }
    }
    result
}

fn text_of_result(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            cua_driver_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

pub(crate) fn not_editable_result(message: String, focus: FocusDispatch) -> ToolResult {
    match focus {
        FocusDispatch::None => {
            ToolResult::error(message.clone()).with_structured(serde_json::json!({
                "code": "not_editable",
                "effect": "refused",
                "dispatched": false,
                "keys_sent": false,
                "message": message,
            }))
        }
        FocusDispatch::SentAx | FocusDispatch::SentClick => {
            let text =
                format!("{message} The focus action was delivered; no text or keys were sent.");
            let (transport, delivery) = if matches!(focus, FocusDispatch::SentClick) {
                (ActionTransport::MacosCgEventHid, ActualDelivery::Foreground)
            } else {
                (ActionTransport::MacosAxAction, ActualDelivery::Background)
            };
            ToolResult::text(text.clone())
                .with_structured(serde_json::json!({
                    "code": "not_editable",
                    "effect": "partial",
                    "dispatched": true,
                    "keys_sent": false,
                    "message": text,
                }))
                .with_action_record(
                    ActionExecutionRecord::builder(
                        ActionEffect::Partial,
                        transport,
                        RequestedDelivery::Background,
                    )
                    .actual_delivery(delivery)
                    .delivered_count(1)
                    .build()
                    .expect("not_editable partial record is valid"),
                )
        }
        FocusDispatch::Indeterminate => {
            let text = format!(
                "{message} A focus action may have been sent; no text or keys were sent. Read fresh state before retrying."
            );
            ToolResult::text(text.clone())
                .with_structured(serde_json::json!({
                    "code": "not_editable",
                    "effect": "unverifiable",
                    "keys_sent": false,
                    "message": text,
                }))
                .with_action_record(
                    ActionExecutionRecord::builder(
                        ActionEffect::Unverifiable,
                        ActionTransport::MacosAxAction,
                        RequestedDelivery::Background,
                    )
                    .actual_delivery(ActualDelivery::Unknown)
                    .build()
                    .expect("not_editable unverifiable record is valid"),
                )
        }
    }
}

const CELL_MESSAGE: &str =
    "the target is a cell in a row, outline or table that is not being edited";
const UNREADABLE_MESSAGE: &str = "the element at the point could not be classified as editable";
const CELL_MESSAGE_TOKEN: &str = "the target element is not being edited";

fn verdict_message(verdict: CellVerdict, point: bool) -> &'static str {
    match (verdict, point) {
        (CellVerdict::NotEditing, _) => CELL_MESSAGE,
        (CellVerdict::Unreadable, true) => UNREADABLE_MESSAGE,
        (CellVerdict::Unreadable, false) => {
            "the focused element could not be read, so editability is unknown"
        }
        _ => "the target is not editable",
    }
}

fn cell_refusal(verdict: CellVerdict, point: bool) -> ToolResult {
    let advice = if matches!((verdict, point), (CellVerdict::Unreadable, false)) {
        " Pass the text field's element_token (from get_window_state) to type into it."
    } else {
        ""
    };
    let message = format!(
        "{}; no focus action and no keys were sent.{advice}",
        verdict_message(verdict, point)
    );
    not_editable_result(message, FocusDispatch::None)
}

fn verdict_allows(verdict: CellVerdict) -> bool {
    matches!(verdict, CellVerdict::NotCell | CellVerdict::Editing)
}

/// Classify the focused element (untargeted route). Blocking, unsafe AX
/// context; Unreadable on any failure.
pub(crate) unsafe fn classify_untargeted(pid: i32, window_id: Option<u32>) -> CellVerdict {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
    let element = match window_id {
        Some(wid) => {
            crate::ax::exact_target::focused_element_in_window_with_deadline(pid, wid, deadline)
        }
        None => crate::ax::bindings::focused_element_of_pid(pid),
    };
    let Some(element) = element else {
        return CellVerdict::Unreadable;
    };
    let verdict = facts_for_element_with_deadline(element, deadline)
        .map(|facts| cell_verdict(&facts))
        .unwrap_or(CellVerdict::Unreadable);
    CFRelease(element as CFTypeRef);
    verdict
}

/// Classify one element the caller already holds. Blocking, unsafe AX
/// context; Unreadable on any failure.
pub(crate) unsafe fn classify_element(element: AXUIElementRef) -> CellVerdict {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
    facts_for_element_with_deadline(element, deadline)
        .map(|facts| cell_verdict(&facts))
        .unwrap_or(CellVerdict::Unreadable)
}

/// Late recheck inside a delivery context: the focused element may have
/// changed since the early guard. False on any unreadable state.
pub(crate) unsafe fn sync_focused_cell_allows(pid: i32, window_id: Option<u32>) -> bool {
    verdict_allows(classify_untargeted(pid, window_id))
}

/// CELL rule for untargeted typing. A blocking-task failure is a fail-closed
/// refusal, never a pass.
pub(crate) async fn guard_untargeted(pid: i32, window_id: Option<u32>) -> Option<ToolResult> {
    match latency::spawn_action_blocking(move || unsafe {
        let verdict = classify_untargeted(pid, window_id);
        match verdict {
            CellVerdict::NotEditing | CellVerdict::Unreadable => Some(cell_refusal(verdict, false)),
            _ => None,
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Some(cell_refusal(CellVerdict::Unreadable, false)),
    }
}

/// CELL rule for point-targeted typing, before any focus action. The
/// caller supplies the same frame the click will use; the hit must belong
/// to the delivery window. Everything unclassifiable fails closed.
pub(crate) async fn guard_point_with_frame(
    pid: i32,
    window_id: u32,
    frame: &super::px_frame::WindowPxFrame,
    native_x: f64,
    native_y: f64,
) -> Option<ToolResult> {
    let (screen_x, screen_y, _, _) = frame.to_screen(native_x, native_y);
    match latency::spawn_action_blocking(move || unsafe {
        let verdict = classify_point_screen(pid, screen_x, screen_y, window_id);
        match verdict {
            CellVerdict::NotEditing | CellVerdict::Unreadable => Some(cell_refusal(verdict, true)),
            _ => None,
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Some(cell_refusal(CellVerdict::Unreadable, true)),
    }
}

/// A point guard that cannot resolve its window's frame: pre-dispatch
/// refusal, nothing sent.
pub(crate) fn unresolved_point_refusal() -> ToolResult {
    cell_refusal(CellVerdict::Unreadable, true)
}

/// Hit-test at a screen point and classify, requiring the hit to belong to
/// the delivery window.
unsafe fn classify_point_screen(pid: i32, x: f64, y: f64, window_id: u32) -> CellVerdict {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
    if std::time::Instant::now() >= deadline {
        return CellVerdict::Unreadable;
    }
    let Some(element) = element_at_screen_position_bounded(pid, x, y, 0.5) else {
        return CellVerdict::Unreadable;
    };
    let _ = AXUIElementSetMessagingTimeout(element, 0.5);
    // Prove the hit belongs to the delivery window: an overlapping
    // same-process sibling must not be classified in its place.
    let mut current = element;
    let mut owner: Option<u32> = None;
    for _ in 0..12 {
        if std::time::Instant::now() >= deadline {
            if current != element {
                CFRelease(current as CFTypeRef);
            }
            CFRelease(element as CFTypeRef);
            return CellVerdict::Unreadable;
        }
        let Some(parent) = copy_element_attr(current, "AXParent") else {
            break;
        };
        if std::time::Instant::now() >= deadline {
            if current != element {
                CFRelease(current as CFTypeRef);
            }
            // The newly copied parent is released too on this exit.
            CFRelease(parent as CFTypeRef);
            CFRelease(element as CFTypeRef);
            return CellVerdict::Unreadable;
        }
        let _ = AXUIElementSetMessagingTimeout(parent, 0.5);
        let role = copy_string_attr(parent, "AXRole");
        if role.as_deref() == Some("AXWindow") {
            owner = ax_get_window_id(parent);
        }
        if current != element {
            CFRelease(current as CFTypeRef);
        }
        current = parent;
        if owner.is_some() || role.as_deref() == Some("AXApplication") {
            break;
        }
    }
    if current != element {
        CFRelease(current as CFTypeRef);
    }
    if owner != Some(window_id) {
        CFRelease(element as CFTypeRef);
        return CellVerdict::Unreadable;
    }
    let verdict = facts_for_element_with_deadline(element, deadline)
        .map(|facts| cell_verdict(&facts))
        .unwrap_or(CellVerdict::Unreadable);
    CFRelease(element as CFTypeRef);
    verdict
}

/// After the focus step of a point-targeted route, verify the resulting
/// focused element still allows typing before keys are sent.
pub(crate) async fn focused_cell_allows_typing(pid: i32, window_id: Option<u32>) -> bool {
    latency::spawn_action_blocking(move || unsafe { sync_focused_cell_allows(pid, window_id) })
        .await
        .unwrap_or_default()
}

/// TOKEN rule plus the focus action preceding an allowed write.
pub(crate) async fn token_not_editable_receipt(
    pid: i32,
    element: usize,
    window_id: u32,
) -> Result<FocusDispatch, ToolResult> {
    token_not_editable_with_policy(pid, element, window_id, true).await
}

async fn token_not_editable_with_policy(
    pid: i32,
    element: usize,
    window_id: u32,
    strict: bool,
) -> Result<FocusDispatch, ToolResult> {
    match latency::spawn_action_blocking(move || unsafe {
        let element = element as AXUIElementRef;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
        let _ = (pid, window_id);
        token_guard_with(
            || facts_for_element_with_deadline(element, deadline),
            || {
                if !strict {
                    let _ = crate::input::ax_actions::focus_element(element as usize);
                    return Ok(());
                }
                let settable = crate::ax::write_outcome::query_settable(element, "AXFocused");
                let error = crate::input::ax_actions::focus_element_raw(element as usize);
                match crate::ax::write_outcome::native_write_outcome(
                    crate::ax::write_outcome::NativeWriteKind::SetAttribute,
                    true,
                    &settable,
                    error,
                ) {
                    cua_driver_core::mutation::WriteOutcome::Delivered => Ok(()),
                    outcome => Err(focus_write_refusal(
                        outcome,
                        error,
                        cua_driver_core::mutation::PriorInput::None,
                    )),
                }
            },
            strict,
        )
    })
    .await
    {
        Ok(result) => result,
        // The task may have failed after the focus attempt: never treat a
        // classification failure as permission to write.
        Err(_) => Err(not_editable_result(
            "the target's editability could not be classified; no text or keys were sent.".into(),
            FocusDispatch::Indeterminate,
        )),
    }
}

pub(super) fn focus_write_refusal(
    outcome: cua_driver_core::mutation::WriteOutcome,
    error: i32,
    prior: cua_driver_core::mutation::PriorInput,
) -> ToolResult {
    use cua_driver_core::mutation::{ChainResult, Postcondition};
    let chain = ChainResult::from_recorded(vec![outcome], prior)
        .expect("one focus write is a valid recorded chain");
    let effect = if chain.no_input() {
        "refused"
    } else {
        "unverifiable"
    };
    let mut result = ToolResult::error(format!(
        "AXFocused returned error {error}; no value or keys were sent. Read fresh state before retrying."
    )).with_structured(serde_json::json!({
        "code": "not_editable", "effect": effect, "keys_sent": false,
    }));
    if let Some(disposition) = chain.error_disposition(Postcondition::Indeterminate, false) {
        result = result.with_disposition(disposition);
    }
    result
}

fn token_guard_with(
    mut read: impl FnMut() -> Option<ElementFacts>,
    focus_element: impl FnOnce() -> Result<(), ToolResult>,
    require_focus: bool,
) -> Result<FocusDispatch, ToolResult> {
    let Some(initial) = read() else {
        return Err(not_editable_result(
            "the target element could not be read; no text or keys were sent.".into(),
            FocusDispatch::None,
        ));
    };
    if initial.role.is_none() {
        return Err(not_editable_result(
            "the target element's role could not be read; no text or keys were sent.".into(),
            FocusDispatch::None,
        ));
    }
    if !initial.role.as_deref().is_some_and(is_text_role) {
        return Ok(FocusDispatch::None);
    }
    let mut focus = FocusDispatch::None;
    if initial.focused != Some(true) {
        focus_element()?;
        focus = FocusDispatch::Indeterminate;
    }
    let Some(facts) = read() else {
        return Err(not_editable_result(
            "the target element could not be read; no text or keys were sent.".into(),
            focus,
        ));
    };
    if facts.role.is_none() {
        return Err(not_editable_result(
                "the target element's role could not be read after the focus attempt; no text                  or keys were sent."
                    .into(),
                focus,
            ));
    }
    if matches!(focus, FocusDispatch::Indeterminate) && facts.focused == Some(true) {
        // The read-back proves the focus action landed.
        focus = FocusDispatch::SentAx;
    }
    if require_focus && matches!(focus, FocusDispatch::Indeterminate) {
        return Err(not_editable_result(
            "the focus action was not confirmed; no value or keys were sent.".into(),
            focus,
        ));
    }
    if token_refused(&facts) {
        return Err(not_editable_result(
            format!("{CELL_MESSAGE_TOKEN}; no text or keys were sent."),
            focus,
        ));
    }
    Ok(focus)
}

/// Desktop-scope dispatch recheck, inside the posting closure: the
/// frontmost app may have changed since the early guard, so the pid and its
/// focused element are re-read immediately before the global post.
pub(crate) fn desktop_dispatch_guard(expected_pid: i32, printable: bool) -> anyhow::Result<()> {
    let front = crate::apps::frontmost_pid();
    if front != Some(expected_pid) {
        anyhow::bail!(
            "not_editable: the frontmost application changed before the key was posted; no keys              were sent"
        );
    }
    if printable && !unsafe { sync_focused_cell_allows(expected_pid, None) } {
        anyhow::bail!(
            "not_editable: the frontmost focused element is a non-editing cell or is              unreadable; no keys were sent"
        );
    }
    // The classification itself can be slow: the frontmost app is checked
    // once more immediately before the global post.
    if crate::apps::frontmost_pid() != Some(expected_pid) {
        anyhow::bail!(
            "not_editable: the frontmost application changed during classification; no keys              were sent"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_guard_retains_successful_focus_receipts() {
        use std::cell::Cell;
        use std::collections::VecDeque;
        let facts = |focused| ElementFacts {
            role: Some("AXTextField".into()),
            focused,
            value_settable: Some(true),
            ..ElementFacts::default()
        };
        for (before, after, expected, calls) in [
            (Some(true), Some(true), FocusDispatch::None, 0),
            (Some(false), Some(true), FocusDispatch::SentAx, 1),
            (Some(false), Some(false), FocusDispatch::Indeterminate, 1),
        ] {
            let mut reads = VecDeque::from([Some(facts(before)), Some(facts(after))]);
            let focus_calls = Cell::new(0);
            let receipt = token_guard_with(
                || reads.pop_front().expect("unexpected read"),
                || {
                    focus_calls.set(focus_calls.get() + 1);
                    Ok(())
                },
                false,
            )
            .expect("editable");
            assert_eq!(receipt, expected);
            assert_eq!(focus_calls.get(), calls);
        }
    }

    #[test]
    fn token_guard_retains_uncertain_focus_on_failed_read() {
        let mut reads = std::collections::VecDeque::from([
            Some(ElementFacts {
                role: Some("AXTextField".into()),
                focused: Some(false),
                ..ElementFacts::default()
            }),
            None,
        ]);
        let result = token_guard_with(|| reads.pop_front().unwrap(), || Ok(()), false).unwrap_err();
        assert_eq!(result.structured_content.unwrap()["effect"], "unverifiable");
    }

    #[test]
    fn strict_token_guard_stops_after_raw_focus_failure() {
        use crate::ax::bindings::{kAXErrorAPIDisabled, kAXErrorCannotComplete};
        use crate::ax::write_outcome::{native_write_outcome, NativeWriteKind, Settable};
        use cua_driver_contract::ErrorDisposition;
        for (error, effect, disposition) in [
            (
                kAXErrorCannotComplete,
                "unverifiable",
                ErrorDisposition::SUSPECTED_NOOP,
            ),
            (
                kAXErrorAPIDisabled,
                "refused",
                ErrorDisposition::NOT_DELIVERED,
            ),
        ] {
            let mut reads = 0;
            let result = token_guard_with(
                || {
                    reads += 1;
                    Some(ElementFacts {
                        role: Some("AXTextField".into()),
                        focused: Some(false),
                        ..ElementFacts::default()
                    })
                },
                || {
                    let outcome = native_write_outcome(
                        NativeWriteKind::SetAttribute,
                        true,
                        &Settable::Yes,
                        error,
                    );
                    Err(focus_write_refusal(
                        outcome,
                        error,
                        cua_driver_core::mutation::PriorInput::None,
                    ))
                },
                true,
            )
            .unwrap_err();
            assert_eq!(reads, 1);
            assert_eq!(
                result.structured_content.as_ref().unwrap()["effect"],
                effect
            );
            assert_eq!(result.disposition, Some(disposition));
        }
    }

    fn text_of(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                cua_driver_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_later_refusal_after_a_sent_focus_action_becomes_partial() {
        let refusal = ToolResult::error("focus conflict").with_structured(serde_json::json!({
            "code": "window_not_key",
            "effect": "refused",
        }));
        let result = account_focus_receipt(refusal, Some(FocusDispatch::SentAx));
        assert_ne!(
            result.is_error,
            Some(true),
            "the registry publishes partial records"
        );
        let data = result.structured_content.clone().unwrap();
        assert_eq!(data["effect"], "partial");
        assert_eq!(data["focus_action"], "sent");
        assert_eq!(data["code"], "window_not_key");
        let public = result
            .action_record
            .as_ref()
            .unwrap()
            .public_result()
            .expect("valid record");
        let public = serde_json::to_value(public).unwrap();
        assert_eq!(public["effect"], "partial");
        assert_eq!(public["delivery"]["mode"], "background");
    }

    /// An untargeted refusal for an unreadable focus names the next action;
    /// a point refusal and a cell refusal keep their text.
    #[test]
    fn an_unreadable_focus_refusal_asks_for_the_element_token() {
        let untargeted = text_of(&cell_refusal(CellVerdict::Unreadable, false));
        assert!(
            untargeted.contains("editability is unknown"),
            "{untargeted}"
        );
        assert!(untargeted.contains("no focus action and no keys were sent"));
        assert!(untargeted.contains("element_token"), "{untargeted}");
        let point = text_of(&cell_refusal(CellVerdict::Unreadable, true));
        assert!(!point.contains("element_token"), "{point}");
        let cell = text_of(&cell_refusal(CellVerdict::NotEditing, false));
        assert!(!cell.contains("element_token"), "{cell}");
    }

    #[test]
    fn an_unverifiable_focus_outcome_is_projected_as_partial_when_sent() {
        let unconfirmed = ToolResult::error("focus click sent, not confirmed").with_structured(
            serde_json::json!({
                "code": "pixel_focus_unconfirmed",
                "effect": "unverifiable",
            }),
        );
        let result = account_focus_receipt(unconfirmed, Some(FocusDispatch::SentClick));
        assert_eq!(
            result.structured_content.clone().unwrap()["effect"],
            "partial"
        );
        let public = result
            .action_record
            .as_ref()
            .unwrap()
            .public_result()
            .expect("valid record");
        let public = serde_json::to_value(public).unwrap();
        assert_eq!(public["delivery"]["mode"], "foreground");
    }

    #[test]
    fn a_not_editable_error_without_a_receipt_becomes_the_structured_refusal() {
        let error = ToolResult::error("not_editable: cell at delivery");
        let result = account_focus_receipt(error, None);
        let data = result.structured_content.unwrap();
        assert_eq!(data["code"], "not_editable");
        assert_eq!(data["effect"], "refused");
        assert_eq!(data["dispatched"], false);
    }

    #[test]
    fn a_pre_dispatch_refusal_sends_nothing() {
        let result = not_editable_result("cell".into(), FocusDispatch::None);
        assert_eq!(result.is_error, Some(true));
        let data = result.structured_content.unwrap();
        assert_eq!(data["code"], "not_editable");
        assert_eq!(data["effect"], "refused");
        assert_eq!(data["dispatched"], false);
        assert_eq!(data["keys_sent"], false);
        assert!(result.action_record.is_none());
    }

    #[test]
    fn a_refusal_after_a_sent_focus_action_is_partial() {
        let result = not_editable_result("cell".into(), FocusDispatch::SentAx);
        let data = result.structured_content.clone().unwrap();
        assert_eq!(data["effect"], "partial");
        assert_eq!(data["keys_sent"], false);
        let public = result
            .action_record
            .as_ref()
            .unwrap()
            .public_result()
            .expect("valid record");
        let public = serde_json::to_value(public).unwrap();
        assert_eq!(public["effect"], "partial");
        assert_eq!(public["delivery"]["mode"], "background");
        assert_eq!(public["delivery"]["delivered_count"], 1);
        assert!(text_of(&result).contains("focus action was delivered"));
    }

    #[test]
    fn a_foreground_click_receipt_reports_foreground_delivery() {
        let result = not_editable_result("cell".into(), FocusDispatch::SentClick);
        let public = result
            .action_record
            .as_ref()
            .unwrap()
            .public_result()
            .expect("valid record");
        let public = serde_json::to_value(public).unwrap();
        assert_eq!(public["delivery"]["mode"], "foreground");
        assert_eq!(public["effect"], "partial");
    }

    #[test]
    fn a_refusal_after_an_indeterminate_focus_action_is_unverifiable() {
        let result = not_editable_result("cell".into(), FocusDispatch::Indeterminate);
        let data = result.structured_content.clone().unwrap();
        assert_eq!(data["effect"], "unverifiable");
        assert!(data.get("dispatched").is_none());
        let public = result
            .action_record
            .as_ref()
            .unwrap()
            .public_result()
            .expect("valid record");
        let public = serde_json::to_value(public).unwrap();
        assert_eq!(public["effect"], "unverifiable");
        assert_eq!(public["delivery"]["mode"], "unknown");
        assert!(text_of(&result).contains("may have been sent"));
    }
}
