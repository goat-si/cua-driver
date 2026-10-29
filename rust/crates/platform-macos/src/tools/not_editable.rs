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
    ax_get_window_id, copy_element_attr, copy_string_attr, element_at_screen_position,
    AXUIElementRef, AXUIElementSetMessagingTimeout,
};
use crate::ax::editable::{
    cell_verdict, facts_for_element, is_text_role, token_refused, CellVerdict,
};
use crate::goat::action_latency as latency;
use core_foundation::base::{CFRelease, CFTypeRef};

/// What happened before a not_editable refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusDispatch {
    /// No focus action was attempted: proven pre-dispatch.
    None,
    /// A focus action (AXFocused or a click) was sent.
    Sent,
    /// A focus action may have been sent; its outcome is unknown.
    Indeterminate,
}

/// The not_editable result. A refusal after a focus attempt never claims
/// nothing was sent: the focus action is reported as delivered (partial)
/// or possibly delivered (unverifiable, delivery unknown).
pub(crate) fn not_editable_result_with(
    message: String,
    focus: FocusDispatch,
    foreground: bool,
) -> ToolResult {
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
        FocusDispatch::Sent => {
            let text =
                format!("{message} The focus action was delivered; no text or keys were sent.");
            let (transport, delivery) = if foreground {
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

pub(crate) fn not_editable_result(message: String, focus: FocusDispatch) -> ToolResult {
    not_editable_result_with(message, focus, false)
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
    let message = format!(
        "{}; no focus action and no keys were sent.",
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
    let element = match window_id {
        Some(wid) => crate::ax::exact_target::focused_element_in_window(pid, wid),
        None => crate::ax::bindings::focused_element_of_pid(pid),
    };
    let Some(element) = element else {
        return CellVerdict::Unreadable;
    };
    let verdict = facts_for_element(element)
        .map(|facts| cell_verdict(&facts))
        .unwrap_or(CellVerdict::Unreadable);
    CFRelease(element as CFTypeRef);
    verdict
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
/// window-local native pixel point is translated to screen coordinates with
/// the same frame the click uses; the hit must belong to the delivery
/// window. A missing hit, a foreign-window hit, unreadable ancestry or a
/// task failure all fail closed pre-dispatch.
pub(crate) async fn guard_point(
    pid: i32,
    window_id: Option<u32>,
    native_x: f64,
    native_y: f64,
) -> Option<ToolResult> {
    let Some(wid) = window_id else {
        return Some(cell_refusal(CellVerdict::Unreadable, true));
    };
    let frame = match super::px_frame::resolve_or_refuse(wid).await {
        Ok(frame) => frame,
        Err(_) => return Some(cell_refusal(CellVerdict::Unreadable, true)),
    };
    let (screen_x, screen_y, _, _) = frame.to_screen(native_x, native_y);
    match latency::spawn_action_blocking(move || unsafe {
        let verdict = classify_point_screen(pid, screen_x, screen_y, wid);
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

/// Hit-test at a screen point and classify, requiring the hit to belong to
/// the delivery window.
unsafe fn classify_point_screen(pid: i32, x: f64, y: f64, window_id: u32) -> CellVerdict {
    let Some(element) = element_at_screen_position(pid, x, y) else {
        return CellVerdict::Unreadable;
    };
    // Prove the hit belongs to the delivery window: an overlapping
    // same-process sibling must not be classified in its place.
    let mut current = element;
    let mut owner: Option<u32> = None;
    for _ in 0..12 {
        let Some(parent) = copy_element_attr(current, "AXParent") else {
            break;
        };
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
        return CellVerdict::Unreadable;
    }
    let verdict = facts_for_element(element)
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

/// TOKEN rule for element_token type_text/set_value. The role is read
/// first: non-text targets are never focused (their behavior is unchanged).
/// A text-role target is focused when needed (the handoff's order), then
/// evaluated. focus_element masks AX errors, so the focus read-back decides
/// whether the focus action was delivered or is indeterminate.
pub(crate) async fn token_not_editable_refusal(
    pid: i32,
    element: usize,
    window_id: u32,
) -> Option<ToolResult> {
    match latency::spawn_action_blocking(move || unsafe {
        let element = element as AXUIElementRef;
        let Some(initial) = facts_for_element(element) else {
            return Some(not_editable_result(
                "the target element could not be read; no text or keys were sent.".into(),
                FocusDispatch::None,
            ));
        };
        if !initial.role.as_deref().is_some_and(is_text_role) {
            return None;
        }
        let mut focus = FocusDispatch::None;
        if initial.focused != Some(true) {
            let _ = crate::input::ax_actions::focus_element(element as usize);
            focus = FocusDispatch::Indeterminate;
        }
        let Some(facts) = facts_for_element(element) else {
            return Some(not_editable_result(
                "the target element could not be read; no text or keys were sent.".into(),
                focus,
            ));
        };
        if matches!(focus, FocusDispatch::Indeterminate) && facts.focused == Some(true) {
            // The read-back proves the focus action landed.
            focus = FocusDispatch::Sent;
        }
        if token_refused(&facts) {
            return Some(not_editable_result(
                format!("{CELL_MESSAGE_TOKEN}; no text or keys were sent."),
                focus,
            ));
        }
        let _ = (pid, window_id);
        None
    })
    .await
    {
        Ok(result) => result,
        // The task may have failed after the focus attempt: never treat a
        // classification failure as permission to write.
        Err(_) => Some(not_editable_result(
            "the target's editability could not be classified; no text or keys were sent.".into(),
            FocusDispatch::Indeterminate,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = not_editable_result("cell".into(), FocusDispatch::Sent);
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
        let result = not_editable_result_with("cell".into(), FocusDispatch::Sent, true);
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
