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

use crate::ax::bindings::{element_at_screen_position, AXUIElementRef};
use crate::ax::editable::{cell_verdict, facts_for_element, token_refused, CellVerdict};
use crate::goat::action_latency as latency;

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
        FocusDispatch::Sent => {
            let text =
                format!("{message} The focus action was delivered; no text or keys were sent.");
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
                        ActionTransport::MacosAxAction,
                        RequestedDelivery::Background,
                    )
                    .actual_delivery(ActualDelivery::Background)
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

/// CELL rule for untargeted typing: classify the window's focused element.
/// Returns a refusal for a non-editing cell or unreadable state.
pub(crate) async fn guard_untargeted(pid: i32, window_id: Option<u32>) -> Option<ToolResult> {
    latency::spawn_action_blocking(move || unsafe {
        let element = match window_id {
            Some(wid) => crate::ax::exact_target::focused_element_in_window(pid, wid),
            None => crate::ax::bindings::focused_element_of_pid(pid),
        };
        let verdict = match element {
            None => CellVerdict::Unreadable,
            Some(element) => {
                let verdict = facts_for_element(element)
                    .map(|facts| cell_verdict(&facts))
                    .unwrap_or(CellVerdict::Unreadable);
                core_foundation::base::CFRelease(element as core_foundation::base::CFTypeRef);
                verdict
            }
        };
        match verdict {
            CellVerdict::NotEditing | CellVerdict::Unreadable => Some(cell_refusal(verdict, false)),
            _ => None,
        }
    })
    .await
    .ok()
    .flatten()
}

/// CELL rule for point-targeted typing, before any focus action. A missing
/// hit or unreadable ancestry fails closed with a pre-dispatch refusal.
pub(crate) async fn guard_point(pid: i32, x: f64, y: f64) -> Option<ToolResult> {
    latency::spawn_action_blocking(move || unsafe {
        let element = element_at_screen_position(pid, x, y);
        let verdict = match element {
            None => CellVerdict::Unreadable,
            Some(element) => {
                let verdict = facts_for_element(element)
                    .map(|facts| cell_verdict(&facts))
                    .unwrap_or(CellVerdict::Unreadable);
                core_foundation::base::CFRelease(element as core_foundation::base::CFTypeRef);
                verdict
            }
        };
        match verdict {
            CellVerdict::NotEditing | CellVerdict::Unreadable => Some(cell_refusal(verdict, true)),
            _ => None,
        }
    })
    .await
    .ok()
    .flatten()
}

/// After the focus step of a point-targeted route, verify the resulting
/// focused element still allows typing before keys are sent.
pub(crate) async fn focused_cell_allows_typing(pid: i32, window_id: Option<u32>) -> bool {
    latency::spawn_action_blocking(move || unsafe {
        let element = match window_id {
            Some(wid) => crate::ax::exact_target::focused_element_in_window(pid, wid),
            None => crate::ax::bindings::focused_element_of_pid(pid),
        };
        let Some(element) = element else {
            return false;
        };
        let verdict = facts_for_element(element)
            .map(|facts| cell_verdict(&facts))
            .unwrap_or(CellVerdict::Unreadable);
        core_foundation::base::CFRelease(element as core_foundation::base::CFTypeRef);
        !matches!(verdict, CellVerdict::NotEditing | CellVerdict::Unreadable)
    })
    .await
    .unwrap_or(false)
}

/// TOKEN rule for element_token type_text/set_value: attempt AX focus when
/// the element is not focused (the handoff's order), then evaluate. The
/// refusal accounts for that focus attempt.
pub(crate) async fn token_not_editable_refusal(
    pid: i32,
    element: usize,
    window_id: u32,
) -> Option<ToolResult> {
    latency::spawn_action_blocking(move || unsafe {
        let element = element as AXUIElementRef;
        let mut focus = FocusDispatch::None;
        if facts_for_element(element).map(|f| f.focused) != Some(Some(true)) {
            match crate::input::ax_actions::focus_element(element as usize) {
                Ok(()) => focus = FocusDispatch::Sent,
                Err(_) => focus = FocusDispatch::Indeterminate,
            }
        }
        let Some(facts) = facts_for_element(element) else {
            return Some(not_editable_result(
                "the target element could not be read; no text or keys were sent.".into(),
                focus,
            ));
        };
        let _ = (pid, window_id);
        if token_refused(&facts) {
            return Some(not_editable_result(
                format!("{}; no text or keys were sent.", CELL_MESSAGE_TOKEN),
                focus,
            ));
        }
        None
    })
    .await
    .ok()
    .flatten()
}

const CELL_MESSAGE_TOKEN: &str = "the target element is not being edited";

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
        assert_eq!(public["delivery"]["delivered_count"], 1);
        assert!(text_of(&result).contains("focus action was delivered"));
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
