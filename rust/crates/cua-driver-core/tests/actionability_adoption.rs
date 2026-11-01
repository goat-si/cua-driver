use cua_driver_contract::ErrorDisposition;
use cua_driver_core::actionability::*;

fn facts() -> ElementFacts {
    let bounds = Rect {
        x: 0.,
        y: 0.,
        width: 30.,
        height: 30.,
    };
    ElementFacts {
        visible: Evidence::Known(true),
        offscreen: Evidence::Known(false),
        enabled: Evidence::Known(true),
        editable: Evidence::Known(false),
        stable: Evidence::Known(true),
        receives_events: Evidence::Known(collect_window_hits(bounds, |_| Hit::ReachesTarget)),
        bounds: Evidence::Known(bounds),
        affordances: Evidence::Known(vec![Affordance::Open, Affordance::Select]),
        advertised_actions: Evidence::Known(vec!["AXCustom".into()]),
        selectable_text: Evidence::Known(true),
        role: Evidence::Known("text".into()),
        nonspatial_visible: false,
    }
}

#[test]
fn new_semantic_intents_require_positive_advertisement_and_live_state() {
    for intent in [
        Intent::Open,
        Intent::SelectText,
        Intent::PerformAction { name: "AXCustom" },
    ] {
        let mut f = facts();
        f.stable = Evidence::Known(false);
        f.receives_events = Evidence::Unknown("unavailable".into());
        assert_eq!(
            evaluate(intent, &f, Policy::headless()).verdict,
            Verdict::Pass,
            "{intent:?}"
        );
        f.enabled = Evidence::Known(false);
        assert!(matches!(
            evaluate(intent, &f, Policy::headless()).verdict,
            Verdict::Transient { .. }
        ));
        f.enabled = Evidence::Known(true);
        f.visible = Evidence::Unknown("visibility".into());
        assert!(matches!(
            evaluate(intent, &f, Policy::headless()).verdict,
            Verdict::Transient { unknown: true, .. }
        ));
        f.visible = Evidence::Known(true);
        match intent {
            Intent::Open => f.affordances = Evidence::Absent,
            Intent::SelectText => f.selectable_text = Evidence::Known(false),
            Intent::PerformAction { .. } => {
                f.advertised_actions = Evidence::Known(vec!["other".into()])
            }
            _ => unreachable!(),
        }
        assert!(matches!(
            evaluate(intent, &f, Policy::headless()).verdict,
            Verdict::Terminal { .. }
        ));
        match intent {
            Intent::Open => f.affordances = Evidence::Unknown("actions".into()),
            Intent::SelectText => f.selectable_text = Evidence::Unknown("text".into()),
            Intent::PerformAction { .. } => {
                f.advertised_actions = Evidence::Unknown("actions".into())
            }
            _ => unreachable!(),
        }
        assert!(matches!(
            evaluate(intent, &f, Policy::headless()).verdict,
            Verdict::Transient { unknown: true, .. }
        ));
    }
}

#[test]
fn background_pointer_uses_window_hit_evidence_and_all_gates() {
    let mut f = facts();
    assert_eq!(
        evaluate(Intent::Click, &f, Policy::background_window()).verdict,
        Verdict::Pass
    );
    assert!(matches!(
        evaluate(Intent::TypeText, &f, Policy::background_window()).verdict,
        Verdict::Terminal { .. }
    ));
    f.receives_events = Evidence::Known(collect_hits(
        match f.bounds {
            Evidence::Known(r) => r,
            _ => unreachable!(),
        },
        |_| Hit::ReachesTarget,
    ));
    assert_ne!(
        evaluate(Intent::Click, &f, Policy::background_window()).verdict,
        Verdict::Pass
    );
    f = facts();
    f.stable = Evidence::Known(false);
    assert!(matches!(
        evaluate(Intent::Click, &f, Policy::background_window()).verdict,
        Verdict::Transient { .. }
    ));
    f.stable = Evidence::Known(true);
    f.receives_events = Evidence::Unknown("hit".into());
    assert!(matches!(
        evaluate(Intent::Click, &f, Policy::background_window()).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
    f = facts();
    f.visible = Evidence::Known(false);
    assert!(matches!(
        evaluate(Intent::Click, &f, Policy::background_window()).verdict,
        Verdict::Transient { .. }
    ));
    f.visible = Evidence::Known(true);
    f.enabled = Evidence::Known(false);
    assert!(matches!(
        evaluate(Intent::Click, &f, Policy::background_window()).verdict,
        Verdict::Transient { .. }
    ));
}

#[test]
fn target_window_hit_does_not_prove_foreground_delivery() {
    let f = facts();
    assert!(matches!(
        evaluate(Intent::Click, &f, Policy::headed()).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
}

#[test]
fn coordinate_only_is_not_evaluated() {
    assert_eq!(coordinate_only(), CoordinateOutcome::NotEvaluated);
}

#[test]
fn public_refusal_has_stable_shape_and_typed_disposition() {
    let mut report = ActionabilityReport {
        verdict: Verdict::Terminal {
            code: "timeout",
            reason: "visible unavailable".into(),
        },
        checks: vec![],
        waited_ms: 10,
        samples: 1,
        last_facts: None,
        disposition: Some(DispositionIntent::NotDeliveredSafe),
    };
    let result = report.refusal_result().unwrap();
    assert_eq!(result.disposition, Some(ErrorDisposition::NOT_DELIVERED));
    let encoded = serde_json::to_value(&result).unwrap();
    let body = &encoded["structuredContent"];
    assert_eq!(body["code"], "element_not_actionable");
    assert_eq!(body["refusal"]["code"], "element_not_actionable");
    assert_eq!(body["effect"], "refused");
    assert_eq!(
        body["disposition"],
        serde_json::json!({"delivery":"not_delivered", "retry":"safe"})
    );
    assert!(body.get("actionability").is_none());
    assert!(body["refusal"]["message"]
        .as_str()
        .unwrap()
        .contains("visible unavailable"));
    report.disposition = None;
    let result = report.refusal_result().unwrap();
    assert_eq!(result.disposition, None);
    assert!(serde_json::to_value(result).unwrap()["structuredContent"]
        .get("disposition")
        .is_none());
    report.verdict = Verdict::Pass;
    assert!(report.refusal_result().is_none());
}
