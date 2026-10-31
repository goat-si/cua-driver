use super::{background_element_click_result, refused_element_click_result, ActionTransport};

#[test]
fn raw_fallback_reports_its_physical_transport() {
    for (transport, path) in [
        (ActionTransport::WindowsTargetedInjection, "pixel"),
        (ActionTransport::WindowsPostMessage, "post_message"),
    ] {
        let result = background_element_click_result(
            "physical fallback".into(),
            transport,
            vec![ActionTransport::WindowsUiaInvoke],
        );
        assert_eq!(result.structured_content.as_ref().unwrap()["path"], path);
        let record = result.action_record.unwrap();
        assert_eq!(record.transport, transport);
        let truth = record.debug_json();
        assert_ne!(truth["route"], "accessibility");
        assert_eq!(
            record.attempts[0].transport,
            ActionTransport::WindowsUiaInvoke
        );
        assert_eq!(record.fallbacks[0].to, transport);
    }
}

#[test]
fn failed_provider_calls_disqualify_single_action_marker_exemption() {
    let result = background_element_click_result(
        "semantic success after provider failure".into(),
        ActionTransport::WindowsUiaToggle,
        vec![ActionTransport::WindowsUiaInvoke],
    );
    let record = result.action_record.unwrap();
    assert_eq!(record.transport, ActionTransport::WindowsUiaToggle);
    assert_eq!(record.attempts.len(), 1);
    assert_eq!(record.fallbacks.len(), 1);
    assert_eq!(record.fallbacks[0].from, ActionTransport::WindowsUiaInvoke);
    assert_eq!(record.fallbacks[0].to, ActionTransport::WindowsUiaToggle);
    // Point-free recording requires an empty fallback journal.
    assert!(!record.debug_json()["fallbacks"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn single_provider_success_has_no_invented_attempts() {
    for transport in [
        ActionTransport::WindowsUiaInvoke,
        ActionTransport::WindowsUiaToggle,
        ActionTransport::WindowsUiaSelection,
        ActionTransport::WindowsUiaExpandCollapse,
    ] {
        let result = background_element_click_result("semantic success".into(), transport, vec![]);
        assert_eq!(result.structured_content.as_ref().unwrap()["path"], "ax");
        let record = result.action_record.unwrap();
        assert_eq!(record.transport, transport);
        assert!(record.attempts.is_empty());
        assert!(record.fallbacks.is_empty());
    }
}

#[test]
fn write_failure_records_the_failing_value_transport_and_request() {
    use super::set_value_write_failure_result;
    use crate::uia_mutation::{RefusalKind, SetValueDecision, UncertainKind};
    use cua_driver_core::action_record::{ActionEffect, RequestedDelivery};
    let cases = [
        (
            SetValueDecision::Refused {
                rung: 0,
                kind: RefusalKind::AccessDenied,
                hresult: 0x8007_0005_u32 as i32,
            },
            ActionTransport::WindowsUiaValue,
            ActionEffect::Refused,
        ),
        (
            SetValueDecision::Uncertain {
                rung: 1,
                kind: UncertainKind::Timeout,
                hresult: 0x8013_1505_u32 as i32,
            },
            ActionTransport::WindowsUiaRangeValue,
            ActionEffect::Unverifiable,
        ),
    ];
    for (decision, transport, effect) in cases {
        let result = set_value_write_failure_result(
            decision,
            &serde_json::json!({"delivery_mode": "foreground"}),
        );
        let record = result.action_record.unwrap();
        assert_eq!(record.effect, effect);
        assert!(record.validate().is_ok());
        assert_eq!(record.transport, transport);
        assert_eq!(record.requested_delivery, RequestedDelivery::Foreground);
    }
}

#[test]
fn refused_click_records_the_refusing_rung_and_delivers_nothing() {
    use cua_driver_core::action_record::ActionEffect;
    let result = refused_element_click_result(
        "UIA Toggle on [3]: refused".into(),
        ActionTransport::WindowsUiaToggle,
        vec![ActionTransport::WindowsUiaInvoke],
    );
    assert!(result.is_error.unwrap_or(false));
    let record = result.action_record.unwrap();
    assert_eq!(record.effect, ActionEffect::Refused);
    assert_eq!(record.transport, ActionTransport::WindowsUiaToggle);
    assert!(record.actual_delivery.is_none());
    assert_eq!(record.attempts.len(), 1);
    assert_eq!(
        record.attempts[0].transport,
        ActionTransport::WindowsUiaInvoke
    );
    assert!(record.validate().is_ok());
}
