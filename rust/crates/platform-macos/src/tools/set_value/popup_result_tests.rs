use super::popup_ownership::{tool_result, CloseStatus, PopupOutcome};

#[test]
fn refusal_has_no_dispatch() {
    let result = tool_result(
        &PopupOutcome::Refused {
            reason: "ambiguous".into(),
            preexisting: false,
        },
        "",
        false,
    );
    assert_eq!(result.is_error, Some(true));
    let data = result.structured_content.unwrap();
    assert_eq!(data["dispatched"], false);
    assert_eq!(data["effect"], "refused");
    assert!(data.get("path").is_none());
}

#[test]
fn stopped_after_open_preserves_uncertainty_and_close_note() {
    let result = tool_result(
        &PopupOutcome::StoppedAfterOpen {
            reason: "ambiguous".into(),
            close: CloseStatus::Unconfirmed,
        },
        "",
        false,
    );
    assert_eq!(result.is_error, Some(true));
    let data = result.structured_content.unwrap();
    assert_eq!(data["dispatched"], true);
    assert_eq!(data["effect"], "unverifiable");
    assert!(data["note"].as_str().unwrap().contains("may still be open"));
}

#[test]
fn selected_preexisting_menu_keeps_result_fields_and_note() {
    let result = tool_result(
        &PopupOutcome::Selected {
            title: "Choice".into(),
            preexisting: true,
            close: None,
        },
        "",
        false,
    );
    let data = result.structured_content.unwrap();
    assert_eq!(data["path"], "ax");
    assert_eq!(data["verified"], false);
    assert_eq!(data["effect"], "unverifiable");
    assert!(data["note"].as_str().unwrap().contains("previously open"));
    assert!(data.get("escalation").is_none());
}

#[test]
fn selected_web_surface_retains_escalation() {
    let result = tool_result(
        &PopupOutcome::Selected {
            title: "Choice".into(),
            preexisting: false,
            close: Some(CloseStatus::Confirmed),
        },
        "",
        true,
    );
    let data = result.structured_content.unwrap();
    assert_eq!(data["escalation"]["recommended"], "px");
    assert!(data["note"].as_str().unwrap().contains("confirmed closed"));
}

#[test]
fn sent_unverified_never_claims_refusal() {
    let result = tool_result(
        &PopupOutcome::SentUnverified {
            reason: "press failed".into(),
            preexisting: true,
            close: None,
        },
        "",
        false,
    );
    let data = result.structured_content.unwrap();
    assert_eq!(data["effect"], "unverifiable");
    assert_eq!(data["dispatched"], true);
    assert!(data["note"].as_str().unwrap().contains("previously open"));
}
