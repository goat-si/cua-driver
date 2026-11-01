use super::*;
use cua_driver_core::action_record::{ActionEffect, ActualDelivery, RequestedDelivery};

#[test]
fn w2b_refusal_and_uncertain_records_validate() {
    let menu = menu_refusal("not found".into());
    for result in [
        menu,
        mutation_refusal(
            "refused".into(),
            ActionTransport::WindowsUiaExpandCollapse,
            RequestedDelivery::Background,
            false,
            cua_driver_core::mutation::Reason::StaleElement,
        ),
        mutation_refusal(
            "refused".into(),
            ActionTransport::WindowsUiaScroll,
            RequestedDelivery::Foreground,
            false,
            cua_driver_core::mutation::Reason::PermissionDenied,
        ),
    ] {
        let record = result.action_record.unwrap();
        assert_eq!(record.effect, ActionEffect::Refused);
        assert_eq!(record.actual_delivery, None);
        assert!(record.evidence.is_empty());
        assert!(record.validate().is_ok());
    }
    // A pre-send stop after the no-activate guard ran is not a safe refusal.
    let after_prologue = mutation_refusal(
        "refused after guard".into(),
        ActionTransport::WindowsUiaScroll,
        RequestedDelivery::Background,
        true,
        cua_driver_core::mutation::Reason::StaleElement,
    );
    let record = after_prologue.action_record.unwrap();
    assert_eq!(record.effect, ActionEffect::Unverifiable);
    assert!(record.validate().is_ok());
    for result in [
        menu_uncertain("unknown".into(), ActionTransport::WindowsUiaInvoke),
        mutation_uncertain(
            "unknown".into(),
            "uia_expand_collapse",
            ActionTransport::WindowsUiaExpandCollapse,
            RequestedDelivery::Background,
            ActualDelivery::Background,
        ),
        mutation_uncertain(
            "unknown".into(),
            "uia",
            ActionTransport::WindowsUiaScroll,
            RequestedDelivery::Foreground,
            ActualDelivery::Foreground,
        ),
    ] {
        let record = result.action_record.unwrap();
        assert_eq!(record.effect, ActionEffect::Unverifiable);
        assert!(record.actual_delivery.is_some());
        assert!(record.validate().is_ok());
    }
}
