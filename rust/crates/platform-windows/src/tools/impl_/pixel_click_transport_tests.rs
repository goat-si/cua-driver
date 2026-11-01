use super::{finish_pixel_uia_attempt, posted_pixel_click_result};
use crate::uia::windows_enum::PointInvokeOutcome;
use cua_driver_core::action_record::{
    ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
};

#[test]
fn no_answer_refuses_without_fallback_or_uncertain_effect() {
    let result = finish_pixel_uia_attempt(PointInvokeOutcome::NotResponding, 7, 3, 4)
        .expect("precheck must stop the route");
    assert!(result.is_error.unwrap_or(false));
    let data = result.structured_content.expect("structured refusal");
    assert_eq!(data["effect"], "refused");
    assert_eq!(data["nothing_sent"], true);
    assert_eq!(data["uia_status"], "not_responding");
}

#[test]
fn gone_window_refuses_without_claiming_an_unknown_effect() {
    let result = finish_pixel_uia_attempt(PointInvokeOutcome::WindowGone, 7, 3, 4)
        .expect("a gone window must stop the route");
    assert!(result.is_error.unwrap_or(false));
    let data = result.structured_content.expect("structured refusal");
    assert_eq!(data["effect"], "refused");
    assert_eq!(data["nothing_sent"], true);
    assert_eq!(data["uia_status"], "window_gone");
}

#[test]
fn point_failures_finalize_with_truthful_delivery_and_no_replay() {
    use cua_driver_contract::ErrorDisposition;
    for (outcome, status, effect, disposition) in [
        (
            PointInvokeOutcome::Busy,
            "busy",
            "refused",
            ErrorDisposition::NOT_DELIVERED,
        ),
        (
            PointInvokeOutcome::Unavailable,
            "unavailable",
            "refused",
            ErrorDisposition::NOT_DELIVERED,
        ),
        (
            PointInvokeOutcome::Timeout,
            "timeout",
            "unverifiable",
            ErrorDisposition::SUSPECTED_NOOP,
        ),
        (
            PointInvokeOutcome::SentFailure,
            "sent_failure",
            "unverifiable",
            ErrorDisposition::SUSPECTED_NOOP,
        ),
    ] {
        let result =
            finish_pixel_uia_attempt(outcome, 7, 3, 4).expect("failure must not fall through");
        let wire = cua_driver_core::mcp_result::conforming_tool_result(
            "click",
            serde_json::to_value(result).unwrap(),
        );
        let data = &wire["structuredContent"];
        assert_eq!(data["uia_status"], status);
        assert_eq!(data["effect"], effect);
        assert_eq!(
            serde_json::from_value::<ErrorDisposition>(data["disposition"].clone()).unwrap(),
            disposition
        );
    }
    assert!(finish_pixel_uia_attempt(PointInvokeOutcome::Miss, 7, 3, 4).is_none());
}

#[test]
fn post_message_pixel_click_reports_synthetic_transport() {
    let result = posted_pixel_click_result(42, "click");
    let structured = result
        .structured_content
        .as_ref()
        .expect("posted click should expose legacy transport metadata");
    assert_eq!(structured["path"], "post_message");

    let record = ActionExecutionRecord::from_legacy(
        "click",
        &serde_json::json!({ "delivery_mode": "background" }),
        structured,
    )
    .expect("posted click should normalize into the public action contract");
    assert_eq!(record.transport, ActionTransport::WindowsPostMessage);
    assert_eq!(record.requested_delivery, RequestedDelivery::Background);
    assert_eq!(record.actual_delivery, Some(ActualDelivery::Background));

    let public = serde_json::to_value(record.public_result().expect("valid ActionResult"))
        .expect("serialize ActionResult");
    assert_eq!(public["route"], "synthetic_events");
    assert_eq!(public["delivery"]["mode"], "background");
}

#[test]
fn sent_point_failure_is_uncertain_and_never_replays() {
    let result = finish_pixel_uia_attempt(PointInvokeOutcome::SentFailure, 7, 3, 4)
        .expect("sent failure is terminal");
    assert!(result.is_error.unwrap_or(false));
    assert_eq!(
        result.disposition,
        Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
    );
    let data = result.structured_content.unwrap();
    assert_eq!(data["effect"], "unverifiable");
    assert!(data.get("nothing_sent").is_none());
}
