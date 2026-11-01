use super::*;
use std::cell::Cell;

const NOT_SUPPORTED: i32 = 0x8004_0204_u32 as i32;
const TIMEOUT: i32 = 0x8013_1505_u32 as i32;
const DENIED: i32 = 0x8007_0005_u32 as i32;

#[test]
fn classification_table() {
    use SentVerdict::*;
    use UncertainKind::*;
    for (hr, expected) in [
        (0, Delivered),
        (
            NOT_SUPPORTED,
            Uncertain(Provider(ProviderCode::NotSupported)),
        ),
        (DENIED, Uncertain(Provider(ProviderCode::AccessDenied))),
        (
            0x8004_0201_u32 as i32,
            Uncertain(Provider(ProviderCode::ElementUnavailable)),
        ),
        (
            0x8007_0057_u32 as i32,
            Uncertain(Provider(ProviderCode::InvalidArgument)),
        ),
        (
            0x8004_0200_u32 as i32,
            Uncertain(Provider(ProviderCode::NotEnabled)),
        ),
        (0x8001_0105_u32 as i32, Uncertain(Transport)),
        (0x8001_0108_u32 as i32, Uncertain(Transport)),
        (0x8007_06ba_u32 as i32, Uncertain(Transport)),
        (0x8007_06be_u32 as i32, Uncertain(Transport)),
        (TIMEOUT, Uncertain(Timeout)),
        (0x8000_4005_u32 as i32, Uncertain(Unclassified)),
        (0x8013_1509_u32 as i32, Uncertain(Unclassified)),
        (-7, Uncertain(Unclassified)),
    ] {
        assert_eq!(classify_sent_call(hr), expected, "{hr:#x}");
    }
}

#[test]
fn chain_stops_after_refusal_or_uncertainty() {
    for hr in [DENIED, TIMEOUT, NOT_SUPPORTED, 1] {
        let calls = Cell::new(0);
        let result = run_rungs(vec![
            Box::new(|| RungOutcome::Called(hr)),
            Box::new(|| {
                calls.set(1);
                RungOutcome::Called(0)
            }),
        ]);
        assert_eq!(calls.get(), 0);
        assert!(matches!(result, ChainOutcome::Uncertain { rung: 0, .. }));
    }
}

#[test]
fn chain_stops_at_called_not_supported_without_next_rung() {
    let calls = Cell::new(0);
    let result = run_rungs(vec![
        Box::new(|| RungOutcome::Absent),
        Box::new(|| RungOutcome::Called(NOT_SUPPORTED)),
        Box::new(|| {
            calls.set(1);
            RungOutcome::Called(0)
        }),
    ]);
    assert_eq!(calls.get(), 0);
    assert!(matches!(result, ChainOutcome::Uncertain { rung: 1, .. }));
}

#[test]
fn value_route_never_calls_range_after_uncertain_or_refused() {
    for hr in [DENIED, TIMEOUT, NOT_SUPPORTED, 1] {
        let calls = Cell::new(0);
        let decision = set_value_route(RungOutcome::Called(hr), || {
            calls.set(1);
            RungOutcome::Called(0)
        });
        assert_eq!(calls.get(), 0);
        assert!(matches!(
            decision,
            SetValueDecision::Uncertain { rung: 0, .. }
        ));
    }
    assert!(matches!(
        set_value_route(RungOutcome::Called(NOT_SUPPORTED), || RungOutcome::Called(
            0
        )),
        SetValueDecision::Uncertain { rung: 0, .. }
    ));
    assert!(matches!(
        set_value_route(RungOutcome::Absent, || RungOutcome::Called(NOT_SUPPORTED)),
        SetValueDecision::Uncertain { rung: 1, .. }
    ));
}

#[test]
fn click_mapping_covers_every_outcome() {
    assert_eq!(
        click_plan_from_chain(ChainOutcome::Delivered { rung: 1 }),
        ClickPlan::Semantic {
            rung: 1,
            failed: vec![]
        }
    );
    assert_eq!(
        click_plan_from_chain(ChainOutcome::NoAffordance),
        ClickPlan::Physical { failed: vec![] }
    );
    assert_eq!(
        click_plan_from_chain(ChainOutcome::Refused {
            rung: 1,
            kind: RefusalKind::PermissionDenied,
            hresult: DENIED
        }),
        ClickPlan::Refused {
            rung: 1,
            kind: RefusalKind::PermissionDenied,
            hresult: DENIED,
            failed: vec![]
        }
    );
    assert_eq!(
        click_plan_from_chain(ChainOutcome::Uncertain {
            rung: 1,
            kind: UncertainKind::Timeout,
            hresult: TIMEOUT
        }),
        ClickPlan::Uncertain {
            rung: 1,
            kind: UncertainKind::Timeout,
            hresult: TIMEOUT,
            failed: vec![]
        }
    );
}

#[test]
fn last_range_write_is_classified_and_all_absent_is_terminal() {
    assert_eq!(
        run_rungs(vec![
            Box::new(|| RungOutcome::Absent),
            Box::new(|| RungOutcome::Absent)
        ]),
        ChainOutcome::NoAffordance
    );
    assert_eq!(
        set_value_route(RungOutcome::Absent, || RungOutcome::Called(TIMEOUT)),
        SetValueDecision::Uncertain {
            rung: 1,
            kind: UncertainKind::Timeout,
            hresult: TIMEOUT
        }
    );
    assert_eq!(
        set_value_route(RungOutcome::Absent, || RungOutcome::Called(DENIED)),
        SetValueDecision::Uncertain {
            rung: 1,
            kind: UncertainKind::Provider(ProviderCode::AccessDenied),
            hresult: DENIED
        }
    );
}

#[test]
fn pattern_absence_is_only_null_no_interface_or_not_supported() {
    assert!(pattern_absent(0));
    assert!(pattern_absent(1));
    assert!(pattern_absent(0x8000_4002_u32 as i32));
    assert!(pattern_absent(NOT_SUPPORTED));
    for hr in [
        TIMEOUT,
        DENIED,
        0x8004_0201_u32 as i32,
        0x8000_4005_u32 as i32,
        0x8001_0108_u32 as i32,
    ] {
        assert!(!pattern_absent(hr), "{hr:#x}");
    }
}

#[test]
fn failed_pattern_acquisition_stops_the_chain_without_delivery() {
    let later = Cell::new(0);
    let result = run_rungs(vec![
        Box::new(|| RungOutcome::AcquireFailed(TIMEOUT)),
        Box::new(|| {
            later.set(later.get() + 1);
            RungOutcome::Called(0)
        }),
    ]);
    assert_eq!(later.get(), 0);
    assert_eq!(
        result,
        ChainOutcome::Refused {
            rung: 0,
            kind: RefusalKind::UnconfirmedAlive,
            hresult: TIMEOUT,
        }
    );
    let range_calls = Cell::new(0);
    let decision = set_value_route(RungOutcome::AcquireFailed(DENIED), || {
        range_calls.set(range_calls.get() + 1);
        RungOutcome::Called(0)
    });
    assert_eq!(range_calls.get(), 0);
    assert_eq!(
        decision,
        SetValueDecision::Refused {
            rung: 0,
            kind: RefusalKind::PermissionDenied,
            hresult: DENIED,
        }
    );
}

#[test]
fn scroll_route_stops_on_uncertain_or_partial_and_worker_failure() {
    use MutationRoute::*;
    use MutationStop::*;
    assert_eq!(scroll_route(0, Some(Write(TIMEOUT))), Uncertain);
    assert_eq!(scroll_route(1, Some(Write(DENIED))), Uncertain);
    assert_eq!(scroll_route(0, Some(WorkerFailed)), Uncertain);
    assert_eq!(scroll_route(0, Some(AcquireFailed(TIMEOUT))), Refused);
    assert_eq!(scroll_route(0, Some(Write(NOT_SUPPORTED))), Uncertain);
    assert_eq!(scroll_route(0, Some(Absent)), FallThrough);
    assert_eq!(scroll_route(2, None), Done);
}
#[test]
fn multi_invoke_route_preserves_partial_and_worker_failure() {
    use MutationRoute::*;
    use MutationStop::*;
    assert_eq!(multi_invoke_route(1, Some(Write(DENIED))), Uncertain);
    assert_eq!(multi_invoke_route(0, Some(WorkerFailed)), Uncertain);
    assert_eq!(multi_invoke_route(0, Some(Write(TIMEOUT))), Uncertain);
    assert_eq!(multi_invoke_route(0, Some(AcquireFailed(TIMEOUT))), Refused);
    assert_eq!(multi_invoke_route(0, Some(Write(DENIED))), Uncertain);
    assert_eq!(multi_invoke_route(0, Some(Write(NOT_SUPPORTED))), Uncertain);
    assert_eq!(multi_invoke_route(2, None), Done);
}
#[test]
fn accelerator_scan_stops_before_a_second_match_after_uncertainty() {
    let calls = Cell::new(0);
    let result = accelerator_scan(
        [
            AccelOutcome::Stop(AccelStop::Uncertain(UncertainKind::Timeout), TIMEOUT),
            AccelOutcome::Delivered,
        ]
        .into_iter()
        .inspect(|_| calls.set(calls.get() + 1)),
    );
    assert_eq!(calls.get(), 1);
    assert_eq!(
        result,
        AccelOutcome::Stop(AccelStop::Uncertain(UncertainKind::Timeout), TIMEOUT)
    );
}
#[test]
fn menu_outcome_preserves_completed_hops_and_uncertainty() {
    assert_eq!(
        menu_outcome(0, Some(MenuFailure::Refused)),
        MenuResult::Refused
    );
    assert_eq!(
        menu_outcome(1, Some(MenuFailure::Refused)),
        MenuResult::Partial { completed_hops: 1 }
    );
    assert_eq!(
        menu_outcome(0, Some(MenuFailure::Uncertain)),
        MenuResult::Uncertain { completed_hops: 0 }
    );
    assert_eq!(menu_outcome(2, None), MenuResult::Done);
}
#[test]
fn scroll_direction_rejects_unknown_values() {
    assert_eq!(scroll_direction("left"), Some((true, false)));
    assert_eq!(scroll_direction("down"), Some((false, true)));
    assert_eq!(scroll_direction("sideways"), None);
}

#[test]
fn scroll_preflight_rejects_bad_direction_before_com_acquisition() {
    let calls = Cell::new(0);
    assert_eq!(
        scroll_preflight("sideways", || {
            calls.set(calls.get() + 1);
            42
        }),
        None
    );
    assert_eq!(calls.get(), 0);
    assert_eq!(
        scroll_preflight("left", || {
            calls.set(calls.get() + 1);
            42
        }),
        Some((true, false, 42))
    );
    assert_eq!(calls.get(), 1);
}

#[test]
fn expand_route_classifies_all_terminal_results() {
    use MutationRoute::*;
    assert_eq!(expand_route(RungOutcome::Called(TIMEOUT)), Uncertain);
    assert_eq!(expand_route(RungOutcome::Called(DENIED)), Uncertain);
    assert_eq!(expand_route(RungOutcome::AcquireFailed(TIMEOUT)), Refused);
    assert_eq!(expand_route(RungOutcome::Called(NOT_SUPPORTED)), Uncertain);
    assert_eq!(expand_route(RungOutcome::Absent), FallThrough);
    assert_eq!(expand_route(RungOutcome::Called(0)), Done);
}

#[test]
fn sent_and_acquisition_map_to_core_outcomes() {
    use cua_driver_core::mutation::{Reason, WriteOutcome};
    assert_eq!(classify_sent_call(0).to_core(), WriteOutcome::Delivered);
    for hr in [
        1,
        NOT_SUPPORTED,
        DENIED,
        0x8004_0201_u32 as i32,
        0x8007_0057_u32 as i32,
        0x8004_0200_u32 as i32,
        0x8007_06ba_u32 as i32,
        TIMEOUT,
        0x8000_4005_u32 as i32,
    ] {
        assert!(
            matches!(
                classify_sent_call(hr).to_core(),
                WriteOutcome::StopUncertain(_)
            ),
            "{hr:#x}"
        );
    }
    for hr in [0, 0x8000_4002_u32 as i32, NOT_SUPPORTED] {
        assert_eq!(
            classify_acquisition(hr).to_core(),
            WriteOutcome::FallThrough(Reason::Unsupported)
        );
    }
    for (hr, reason) in [
        (0x8004_0201_u32 as i32, Reason::StaleElement),
        (DENIED, Reason::PermissionDenied),
        (TIMEOUT, Reason::UnconfirmedAlive),
        (0x8000_4005_u32 as i32, Reason::UnconfirmedAlive),
    ] {
        assert_eq!(
            classify_acquisition(hr).to_core(),
            WriteOutcome::StopNotDelivered(reason)
        );
    }
}

#[test]
fn producer_errors_keep_disposition_and_legacy_fields_after_finalization() {
    use cua_driver_contract::ErrorDisposition;
    use cua_driver_core::mutation::{Reason, WriteOutcome};
    for (outcomes, prologue, expected, effect) in [
        (
            vec![WriteOutcome::StopUncertain(Reason::Unknown)],
            false,
            ErrorDisposition::SUSPECTED_NOOP,
            "unverifiable",
        ),
        (
            vec![WriteOutcome::StopNotDelivered(Reason::PermissionDenied)],
            false,
            ErrorDisposition::NOT_DELIVERED,
            "refused",
        ),
        (
            vec![WriteOutcome::StopNotDelivered(Reason::PermissionDenied)],
            true,
            ErrorDisposition::SUSPECTED_NOOP,
            "unverifiable",
        ),
    ] {
        let result = chain_error(
            "write failed",
            serde_json::json!({
                "path":"ax", "verified":false, "note":"note", "warning":"warning",
                "code":"element_not_actionable", "focus_restoration":"preserved"
            }),
            outcomes,
            prologue,
        );
        let wire = cua_driver_core::mcp_result::conforming_tool_result(
            "set_value",
            serde_json::to_value(result).unwrap(),
        );
        let data = &wire["structuredContent"];
        assert_eq!(data["effect"], effect);
        assert_eq!(
            serde_json::from_value::<ErrorDisposition>(data["disposition"].clone()).unwrap(),
            expected
        );
        for (key, value) in [
            ("note", "note"),
            ("warning", "warning"),
            ("code", "element_not_actionable"),
            ("focus_restoration", "preserved"),
        ] {
            assert_eq!(data[key], value, "{key}");
        }
    }
}

#[test]
fn sent_reveal_and_focus_failures_stop_before_followup() {
    for hr in [1, NOT_SUPPORTED, DENIED, TIMEOUT, 0x80004005u32 as i32] {
        for _site in [
            "scroll_into_view",
            "ancestor_scroll",
            "set_focus",
            "text_range_select",
        ] {
            let calls = Cell::new(0);
            assert_eq!(
                after_sent_call(hr, || {
                    calls.set(calls.get() + 1);
                }),
                Err(hr)
            );
            assert_eq!(calls.get(), 0);
        }
    }
    let calls = Cell::new(0);
    assert_eq!(after_sent_call(0, || calls.set(calls.get() + 1)), Ok(()));
    assert_eq!(calls.get(), 1);
}

#[test]
fn focus_failure_never_clicks_or_types() {
    for hr in [1, NOT_SUPPORTED, DENIED, TIMEOUT] {
        let confirms = Cell::new(0);
        let clicks = Cell::new(0);
        let types = Cell::new(0);
        match focus_route(Some(hr), || {
            confirms.set(confirms.get() + 1);
            true
        }) {
            FocusRoute::Confirmed => types.set(types.get() + 1),
            FocusRoute::ClickBeforeSend => clicks.set(clicks.get() + 1),
            FocusRoute::Uncertain => {}
        }
        assert_eq!((confirms.get(), clicks.get(), types.get()), (0, 0, 0));
    }
    assert_eq!(
        focus_route(None, || panic!("no focus was sent")),
        FocusRoute::ClickBeforeSend
    );
    assert_eq!(focus_route(Some(0), || false), FocusRoute::Uncertain);
    assert_eq!(focus_route(Some(0), || true), FocusRoute::Confirmed);
}

#[test]
fn set_value_failure_producer_finalizes_with_uncertain_delivery() {
    use cua_driver_contract::ErrorDisposition;
    use cua_driver_core::action_record::{ActionEffect, ActualDelivery};
    for (decision, sent) in [
        (
            SetValueDecision::Uncertain {
                rung: 0,
                kind: UncertainKind::Provider(ProviderCode::NotSupported),
                hresult: NOT_SUPPORTED,
            },
            true,
        ),
        (
            SetValueDecision::Refused {
                rung: 1,
                kind: RefusalKind::PermissionDenied,
                hresult: DENIED,
            },
            false,
        ),
    ] {
        let producer = set_value_write_failure_result(
            decision,
            &serde_json::json!({"delivery_mode":"foreground"}),
        );
        let record = producer.action_record.as_ref().unwrap();
        assert_eq!(record.effect, ActionEffect::Unverifiable);
        assert_eq!(
            record.actual_delivery,
            if sent {
                Some(ActualDelivery::Unknown)
            } else {
                None
            }
        );
        assert!(record.validate().is_ok());
        let wire = cua_driver_core::mcp_result::conforming_tool_result(
            "set_value",
            serde_json::to_value(producer).unwrap(),
        );
        assert_eq!(wire["isError"], true);
        assert_eq!(wire["structuredContent"]["path"], "ax");
        assert_eq!(wire["structuredContent"]["verified"], false);
        assert_eq!(wire["structuredContent"]["effect"], "unverifiable");
        assert_eq!(
            serde_json::from_value::<ErrorDisposition>(
                wire["structuredContent"]["disposition"].clone()
            )
            .unwrap(),
            ErrorDisposition::SUSPECTED_NOOP
        );
    }
}

#[test]
fn prior_prologue_preserves_refusal_fields_and_makes_delivery_uncertain() {
    let result = cua_driver_core::protocol::ToolResult::error("offscreen")
        .with_structured(
            serde_json::json!({"code":"element_not_visible", "note":"kept", "effect":"refused"}),
        )
        .with_disposition(cua_driver_contract::ErrorDisposition::NOT_DELIVERED);
    let result = after_prologue(result);
    let wire = cua_driver_core::mcp_result::conforming_tool_result(
        "click",
        serde_json::to_value(result).unwrap(),
    );
    assert_eq!(wire["structuredContent"]["code"], "element_not_visible");
    assert_eq!(wire["structuredContent"]["note"], "kept");
    assert_eq!(wire["structuredContent"]["effect"], "unverifiable");
    assert_eq!(
        serde_json::from_value::<cua_driver_contract::ErrorDisposition>(
            wire["structuredContent"]["disposition"].clone()
        )
        .unwrap(),
        cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP
    );
}
