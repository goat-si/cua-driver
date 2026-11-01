use super::*;
use std::{future::Future, pin::Pin, sync::Mutex, time::Duration};
fn rect(x: f64) -> Rect {
    Rect {
        x,
        y: 0.,
        width: 100.,
        height: 50.,
    }
}
fn occ() -> Occluder {
    Occluder {
        role: Some("dialog".into()),
        name: None,
        bounds: None,
    }
}
fn facts() -> ElementFacts {
    ElementFacts {
        visible: Evidence::Known(true),
        offscreen: Evidence::Known(false),
        enabled: Evidence::Known(true),
        editable: Evidence::Known(true),
        stable: Evidence::Known(true),
        receives_events: Evidence::Known(collect_hits(rect(0.), |_| Hit::ReachesTarget)),
        bounds: Evidence::Known(rect(0.)),
        affordances: Evidence::Known(vec![
            Affordance::Press,
            Affordance::ShowMenu,
            Affordance::SetValue,
            Affordance::TypeText,
            Affordance::Select,
            Affordance::Toggle,
            Affordance::Expand,
            Affordance::Collapse,
            Affordance::Scroll,
            Affordance::ScrollIntoView,
            Affordance::Focus,
            Affordance::PressKey,
            Affordance::KeyDown,
            Affordance::KeyUp,
        ]),
        role: Evidence::Known("textfield".into()),
        nonspatial_visible: false,
    }
}
fn headed() -> Policy {
    Policy::headed()
}
fn headless() -> Policy {
    Policy::headless()
}
fn tuple(r: Requirements) -> (bool, bool, bool, bool, bool) {
    (
        r.visible,
        r.stable,
        r.enabled,
        r.editable,
        r.receives_events,
    )
}
#[test]
fn each_intent_requirement_row() {
    use Intent::*;
    let p = (true, true, true, false, true);
    let c = (true, true, false, false, true);
    let t = (true, false, true, true, false);
    let s = (true, true, true, false, false);
    let n = (false, false, false, false, false);
    for (i, e) in [
        (Click, p),
        (DoubleClick, p),
        (TripleClick, p),
        (RightClick, p),
        (Hover, c),
        (Drag, c),
        (SetValue, t),
        (TypeText, t),
        (Clear, t),
        (Select, (true, false, true, false, false)),
        (Toggle, s),
        (Check, s),
        (Uncheck, s),
        (Expand, s),
        (Collapse, s),
        (Scroll, s),
        (ScrollTo, (false, true, false, false, false)),
        (Focus, n),
        (PressKey, n),
        (KeyDown, n),
        (KeyUp, n),
    ] {
        assert_eq!(tuple(i.requirements()), e, "{i:?}");
        assert_eq!(
            evaluate(i, &facts(), headed()).verdict,
            Verdict::Pass,
            "{i:?}"
        );
    }
}
#[test]
fn unknown_absent_and_terminal_precedence() {
    let mut f = facts();
    f.enabled = Evidence::Unknown("read".into());
    assert!(matches!(
        evaluate(Intent::Click, &f, headed()).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
    f.enabled = Evidence::Absent;
    assert!(matches!(
        evaluate(Intent::Click, &f, headed()).verdict,
        Verdict::Transient { unknown: false, .. }
    ));
    f.enabled = Evidence::Unknown("read".into());
    assert_eq!(evaluate(Intent::Hover, &f, headed()).verdict, Verdict::Pass);
    f.visible = Evidence::Known(false);
    f.role = Evidence::Known("button".into());
    f.editable = Evidence::Known(false);
    f.affordances = Evidence::Known(vec![Affordance::TypeText]);
    assert!(matches!(
        evaluate(Intent::TypeText, &f, headed()).verdict,
        Verdict::Terminal {
            code: "unsupported_action",
            ..
        }
    ));
}
#[test]
fn hit_test_five_points() {
    let points = candidate_points(rect(0.));
    assert_eq!(points[0], Point { x: 50., y: 25. });
    assert_eq!(points[4], Point { x: 75., y: 37.5 });
    let mut n = 0;
    let mixed = collect_hits(rect(0.), |_| {
        n += 1;
        if n == 1 {
            Hit::Occluded(occ())
        } else {
            Hit::Unknown
        }
    });
    assert_eq!((mixed.attempted, mixed.unknown, mixed.occluded), (5, 4, 1));
    assert_eq!(mixed.occluder, Some(occ()));
    for hit in [
        mixed,
        collect_hits(rect(0.), |_| Hit::Unknown),
        collect_hits(rect(0.), |_| Hit::Occluded(occ())),
    ] {
        let mut f = facts();
        f.receives_events = Evidence::Known(hit);
        assert!(matches!(
            evaluate(Intent::Click, &f, headed()).verdict,
            Verdict::Transient { .. }
        ));
    }
    let mut n = 0;
    let e = collect_hits(rect(0.), |_| {
        n += 1;
        if n == 2 {
            Hit::ReachesTarget
        } else {
            Hit::Unknown
        }
    });
    assert_eq!(e.attempted, 2);
    assert_eq!(e.verified_point, Some(points[1]));
}
#[derive(Default)]
struct TestClock {
    time: TestTime,
    sleeps: Mutex<Vec<Duration>>,
}
#[derive(Default)]
struct TestTime(Mutex<Duration>);
impl TestTime {
    fn get(&self) -> Duration {
        *self.0.lock().unwrap()
    }
    fn set(&self, value: Duration) {
        *self.0.lock().unwrap() = value;
    }
}
impl Clock for TestClock {
    fn now(&self) -> Duration {
        self.time.get()
    }
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            self.sleeps.lock().unwrap().push(d);
            self.time.set(self.time.get() + d);
        })
    }
}
#[test]
fn stability_spacing_tolerance_reset_and_revalidation() {
    let c = TestClock::default();
    let mut s = StabilitySampler::default();
    assert_eq!(
        s.observe(&c, Evidence::Known(rect(0.))),
        Evidence::Known(false)
    );
    assert_eq!(
        s.observe(&c, Evidence::Known(rect(0.1))),
        Evidence::Known(false)
    );
    assert_eq!(s.samples(), 1);
    c.time.set(Duration::from_millis(17));
    assert_eq!(
        s.observe(&c, Evidence::Known(rect(0.5))),
        Evidence::Known(false)
    );
    c.time.set(Duration::from_millis(34));
    assert_eq!(
        s.observe(&c, Evidence::Known(rect(1.))),
        Evidence::Known(true)
    );
    assert_eq!(
        s.revalidate(Evidence::Known(rect(1.5))),
        Evidence::Known(true)
    );
    assert_eq!(
        s.revalidate(Evidence::Known(rect(1.51))),
        Evidence::Known(false)
    );
    c.time.set(Duration::from_millis(51));
    assert_eq!(
        s.observe(&c, Evidence::Known(rect(2.))),
        Evidence::Known(false)
    );
    assert_eq!(s.samples(), 1);
    assert_eq!(
        s.observe(&c, Evidence::Unknown("failed".into())),
        Evidence::Unknown("failed".into())
    );
    assert_eq!(s.samples(), 0);
}
#[test]
fn vocabulary_alternatives_and_physical_only() {
    let mut f = facts();
    f.affordances = Evidence::Known(vec![Affordance::Press]);
    assert!(is_primary(Affordance::Press));
    for a in [
        Affordance::Focus,
        Affordance::ShowMenu,
        Affordance::ScrollIntoView,
    ] {
        assert!(!is_primary(a));
    }
    assert_eq!(
        evaluate(Intent::Select, &f, headless()).verdict,
        Verdict::Pass
    );
    assert_eq!(
        evaluate(Intent::Toggle, &f, headless()).verdict,
        Verdict::Pass
    );
    for i in [Intent::DoubleClick, Intent::TripleClick] {
        assert!(matches!(
            evaluate(i, &f, headless()).verdict,
            Verdict::Terminal {
                code: "policy_denied",
                ..
            }
        ));
    }
    f.stable = Evidence::Known(false);
    f.bounds = Evidence::Absent;
    assert_eq!(
        evaluate(Intent::Click, &f, headless()).verdict,
        Verdict::Pass
    );
    f.bounds = Evidence::Unknown("read".into());
    assert!(matches!(
        evaluate(Intent::Click, &f, headless()).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
    f = facts();
    f.role = Evidence::Known("combobox".into());
    f.affordances = Evidence::Known(vec![Affordance::Expand]);
    assert_eq!(
        evaluate(Intent::Select, &f, headless()).verdict,
        Verdict::Pass
    );
}
#[tokio::test]
async fn wait_zero_clamp_transitions_disposition_json() {
    let c = TestClock::default();
    let mut calls = 0;
    let report = auto_wait(&c, Duration::ZERO, false, |_| {
        calls += 1;
        async {
            let mut f = facts();
            f.enabled = Evidence::Known(false);
            evaluate(Intent::Click, &f, headed())
        }
    })
    .await;
    assert_eq!(calls, 1);
    assert!(matches!(
        report.verdict,
        Verdict::Terminal {
            code: "timeout",
            ..
        }
    ));
    assert_eq!(
        report.disposition,
        Some(DispositionIntent::NotDeliveredSafe)
    );
    let c = TestClock::default();
    let mut calls = 0;
    let report = auto_wait(&c, Duration::from_millis(150), true, |_| {
        calls += 1;
        async {
            let mut f = facts();
            f.enabled = Evidence::Known(false);
            evaluate(Intent::Click, &f, headed())
        }
    })
    .await;
    assert_eq!(calls, 2);
    assert_eq!(
        *c.sleeps.lock().unwrap(),
        vec![Duration::from_millis(100), Duration::from_millis(50)]
    );
    assert_eq!(report.waited_ms, 150);
    assert_eq!(report.disposition, None);
    let c = TestClock::default();
    let mut calls = 0;
    let report = auto_wait(&c, Duration::from_secs(1), false, |_| {
        calls += 1;
        let pass = calls == 2;
        async move {
            let mut f = facts();
            f.enabled = Evidence::Known(pass);
            evaluate(Intent::Click, &f, headed())
        }
    })
    .await;
    assert_eq!(report.verdict, Verdict::Pass);
    assert_eq!(report.samples, 2);
    assert_eq!(report.refusal_json(), None);
    let c = TestClock::default();
    let mut calls = 0;
    let report = auto_wait(&c, Duration::from_secs(1), true, |_| {
        calls += 1;
        let terminal = calls == 2;
        async move {
            let mut f = facts();
            f.enabled = Evidence::Known(false);
            evaluate(
                Intent::DoubleClick,
                &f,
                if terminal { headless() } else { headed() },
            )
        }
    })
    .await;
    assert!(matches!(
        report.verdict,
        Verdict::Terminal {
            code: "policy_denied",
            ..
        }
    ));
    assert_eq!(report.disposition, None);
    assert_eq!(calls, 2);
    assert_eq!(
        report.refusal_json().unwrap()["actionability"]["code"],
        "policy_denied"
    );
}

#[test]
fn each_required_unknown_blocks_even_when_other_evidence_is_positive() {
    for field in ["visible", "enabled", "stable", "hit", "bounds"] {
        let mut f = facts();
        match field {
            "visible" => f.visible = Evidence::Unknown(field.into()),
            "enabled" => f.enabled = Evidence::Unknown(field.into()),
            "stable" => f.stable = Evidence::Unknown(field.into()),
            "hit" => f.receives_events = Evidence::Unknown(field.into()),
            "bounds" => f.bounds = Evidence::Unknown(field.into()),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                evaluate(Intent::Click, &f, headed()).verdict,
                Verdict::Transient { unknown: true, .. }
            ),
            "{field}"
        );
    }
    let mut f = facts();
    f.editable = Evidence::Unknown("editability".into());
    assert!(matches!(
        evaluate(Intent::TypeText, &f, headed()).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
    f = facts();
    f.affordances = Evidence::Unknown("capabilities".into());
    assert!(matches!(
        evaluate(Intent::Select, &f, headless()).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
    f.affordances = Evidence::Absent;
    assert!(matches!(
        evaluate(Intent::Select, &f, headless()).verdict,
        Verdict::Terminal {
            code: "unsupported_action",
            ..
        }
    ));
}

#[test]
fn policy_is_required_for_cursor_and_physical_fallback() {
    let mut f = facts();
    for intent in [Intent::Hover, Intent::Drag] {
        assert!(matches!(
            evaluate(intent, &f, headless()).verdict,
            Verdict::Terminal {
                code: "policy_denied",
                ..
            }
        ));
    }
    let invalid = Policy::focus_fallback();
    f.affordances = Evidence::Known(vec![]);
    assert!(matches!(
        evaluate(Intent::Click, &f, invalid).verdict,
        Verdict::Terminal {
            code: "policy_denied",
            ..
        }
    ));
    f.affordances = Evidence::Known(vec![
        Affordance::Focus,
        Affordance::ShowMenu,
        Affordance::ScrollIntoView,
    ]);
    assert!(matches!(
        evaluate(Intent::Click, &f, headless()).verdict,
        Verdict::Terminal { .. }
    ));
    assert!(matches!(
        evaluate(Intent::SetValue, &f, headless()).verdict,
        Verdict::Terminal { .. }
    ));
}

#[tokio::test]
async fn pending_probe_is_bounded_and_preserves_last_report() {
    let c = TestClock::default();
    let mut calls = 0;
    let report = auto_wait(&c, Duration::from_millis(150), false, |_| {
        calls += 1;
        let initial = calls == 1;
        async move {
            if !initial {
                std::future::pending::<()>().await;
            }
            let mut f = facts();
            f.enabled = Evidence::Known(false);
            evaluate(Intent::Click, &f, headed())
        }
    })
    .await;
    assert_eq!(calls, 2);
    assert_eq!(report.samples, 1);
    assert_eq!(report.waited_ms, 150);
    assert!(matches!(
        report.last_facts.as_ref().unwrap().enabled,
        Evidence::Known(false)
    ));
    assert_eq!(
        report.disposition,
        Some(DispositionIntent::NotDeliveredSafe)
    );
    assert_eq!(
        report.refusal_json(),
        Some(
            serde_json::json!({"actionability":{"verdict":"terminal","code":"timeout","reason":"enabled is false or absent","waited_ms":150}})
        )
    );
}

#[test]
fn early_geometry_change_and_missing_sample_reset_the_window() {
    let c = TestClock::default();
    let mut s = StabilitySampler::default();
    s.observe(&c, Evidence::Known(rect(0.)));
    c.time.set(Duration::from_millis(10));
    s.observe(&c, Evidence::Known(rect(5.)));
    c.time.set(Duration::from_millis(17));
    assert_eq!(
        s.observe(&c, Evidence::Known(rect(5.))),
        Evidence::Known(false)
    );
    assert_eq!(s.samples(), 1);
    c.time.set(Duration::from_millis(27));
    s.observe(&c, Evidence::Known(rect(5.)));
    c.time.set(Duration::from_millis(44));
    assert_eq!(
        s.observe(&c, Evidence::Known(rect(5.))),
        Evidence::Known(true)
    );
    assert_eq!(s.observe(&c, Evidence::Absent), Evidence::Absent);
    assert_eq!(s.samples(), 0);
    for dimension in 0..4 {
        let mut changed = rect(0.);
        match dimension {
            0 => changed.x = 0.51,
            1 => changed.y = 0.51,
            2 => changed.width += 0.51,
            _ => changed.height += 0.51,
        }
        assert!(!geometry_matches(rect(0.), changed));
    }
}

#[tokio::test]
async fn wait_does_not_inherit_probe_disposition() {
    let c = TestClock::default();
    let report = auto_wait(&c, Duration::ZERO, true, |_| async {
        let mut report = evaluate(Intent::Click, &facts(), headed());
        report.verdict = Verdict::Terminal {
            code: "policy_denied",
            reason: "denied".into(),
        };
        report.disposition = Some(DispositionIntent::NotDeliveredSafe);
        report
    })
    .await;
    assert_eq!(report.disposition, None);
}

#[test]
fn role_and_focus_fallback_are_independent_of_unknown_evidence() {
    let mut f = facts();
    f.editable = Evidence::Absent;
    f.affordances = Evidence::Known(vec![Affordance::TypeText]);
    assert_eq!(
        evaluate(Intent::TypeText, &f, headless()).verdict,
        Verdict::Pass
    );
    f.role = Evidence::Known("button".into());
    assert!(matches!(
        evaluate(Intent::TypeText, &f, headless()).verdict,
        Verdict::Terminal { .. }
    ));
    f.role = Evidence::Known("textfield".into());
    f.affordances = Evidence::Known(vec![]);
    assert!(matches!(
        evaluate(Intent::TypeText, &f, headless()).verdict,
        Verdict::Terminal {
            code: "policy_denied",
            ..
        }
    ));
    let focus_only = Policy::focus_fallback();
    assert_eq!(
        evaluate(Intent::TypeText, &f, focus_only).verdict,
        Verdict::Pass
    );
    assert_eq!(default_type_budget("가a"), Duration::from_millis(5_060));
    assert_eq!(default_type_budget(""), DEFAULT_BUDGET);
}

#[test]
fn review_blocker_boundless_without_onscreen_proof_blocks() {
    let mut f = facts();
    f.bounds = Evidence::Absent;
    f.offscreen = Evidence::Absent;
    f.nonspatial_visible = false;
    assert!(matches!(
        evaluate(Intent::Click, &f, headless()).verdict,
        Verdict::Transient { .. }
    ));
}

#[test]
fn review_blocker_collapsed_and_invalid_geometry_blocks() {
    let mut f = facts();
    for (w, h) in [(0., 0.), (-1., 10.), (10., -1.), (f64::NAN, 10.)] {
        f.bounds = Evidence::Known(Rect {
            width: w,
            height: h,
            ..rect(0.)
        });
        assert!(
            matches!(
                evaluate(Intent::Click, &f, headless()).verdict,
                Verdict::Transient { .. }
            ),
            "{w:?} {h:?}"
        );
    }
}

#[test]
fn review_blocker_focus_fallback_has_no_headless_exemptions() {
    let mut f = facts();
    f.stable = Evidence::Known(false);
    f.receives_events = Evidence::Unknown("no hit".into());
    f.bounds = Evidence::Absent;
    f.nonspatial_visible = true;
    let policy = Policy::focus_fallback();
    assert!(matches!(
        evaluate(Intent::Click, &f, policy).verdict,
        Verdict::Transient { .. }
    ));
}

#[test]
fn review_blocker_headless_nonpointer_boundless_stability() {
    let mut f = facts();
    f.bounds = Evidence::Absent;
    f.nonspatial_visible = true;
    f.stable = Evidence::Absent;
    for intent in [
        Intent::Toggle,
        Intent::Check,
        Intent::Uncheck,
        Intent::Expand,
        Intent::Collapse,
        Intent::Scroll,
    ] {
        assert_eq!(
            evaluate(intent, &f, headless()).verdict,
            Verdict::Pass,
            "{intent:?}"
        );
    }
}

#[test]
fn review_blocker_invalid_rectangles_are_unknown_for_stability() {
    let c = TestClock::default();
    let mut sampler = StabilitySampler::default();
    for size in [-1., f64::NAN] {
        let bad = Rect {
            width: size,
            ..rect(0.)
        };
        assert!(matches!(
            sampler.observe(&c, Evidence::Known(bad)),
            Evidence::Unknown(_)
        ));
        assert!(matches!(
            sampler.revalidate(Evidence::Known(bad)),
            Evidence::Unknown(_)
        ));
    }
}

#[test]
fn review_blocker_unknown_editability_inputs_remain_transient() {
    let mut f = facts();
    f.editable = Evidence::Absent;
    f.role = Evidence::Unknown("role".into());
    f.affordances = Evidence::Known(vec![Affordance::TypeText]);
    assert!(matches!(
        evaluate(Intent::TypeText, &f, headless()).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
    f.role = Evidence::Known("button".into());
    f.affordances = Evidence::Unknown("actions".into());
    assert!(matches!(
        evaluate(Intent::TypeText, &f, headless()).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
}

#[test]
fn review_blocker_unknown_combobox_role_is_transient() {
    let mut f = facts();
    f.role = Evidence::Unknown("role".into());
    f.affordances = Evidence::Known(vec![Affordance::Expand]);
    assert!(matches!(
        evaluate(Intent::Select, &f, headless()).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
}

#[test]
fn review_blocker_known_noneditable_overrides_role_and_capability() {
    let mut f = facts();
    f.editable = Evidence::Known(false);
    assert!(matches!(
        evaluate(Intent::TypeText, &f, headless()).verdict,
        Verdict::Terminal { .. }
    ));
}

#[test]
fn review_blocker_open_is_not_press() {
    let mut f = facts();
    f.affordances = Evidence::Known(vec![Affordance::Open]);
    for intent in [Intent::Click, Intent::Select, Intent::Toggle] {
        assert!(
            matches!(
                evaluate(intent, &f, headless()).verdict,
                Verdict::Terminal { .. }
            ),
            "{intent:?}"
        );
    }
    assert!(!supported_affordances(Intent::DoubleClick).contains(&Affordance::Open));
}

#[test]
fn review_visibility_evidence_distinguishes_onscreen_windowless_and_unknown() {
    let mut f = facts();
    f.bounds = Evidence::Absent;
    assert_eq!(
        evaluate(Intent::Click, &f, headless()).verdict,
        Verdict::Pass
    );
    f.offscreen = Evidence::Unknown("offscreen read failed".into());
    assert!(matches!(
        evaluate(Intent::Click, &f, headless()).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
    f.offscreen = Evidence::Known(true);
    assert!(matches!(
        evaluate(Intent::Click, &f, headless()).verdict,
        Verdict::Transient { .. }
    ));
    f.offscreen = Evidence::Absent;
    f.bounds = Evidence::Known(Rect {
        width: 0.,
        height: 0.,
        ..rect(0.)
    });
    assert_eq!(
        evaluate(Intent::Click, &f, headless()).verdict,
        Verdict::Pass
    );
    f.offscreen = Evidence::Known(false);
    assert!(matches!(
        evaluate(Intent::Click, &f, headless()).verdict,
        Verdict::Transient { .. }
    ));
    f.nonspatial_visible = true;
    assert!(matches!(
        evaluate(Intent::Click, &f, headless()).verdict,
        Verdict::Transient { .. }
    ));
}

#[test]
fn review_unknown_stability_is_not_exempted_for_boundless_target() {
    let mut f = facts();
    f.bounds = Evidence::Absent;
    f.stable = Evidence::Unknown("sample failed".into());
    assert!(matches!(
        evaluate(Intent::Toggle, &f, headless()).verdict,
        Verdict::Transient { unknown: true, .. }
    ));
}

#[test]
fn review_occluder_is_preserved_on_check() {
    let mut f = facts();
    f.receives_events = Evidence::Known(collect_hits(rect(0.), |_| Hit::Occluded(occ())));
    let report = evaluate(Intent::Click, &f, headed());
    assert_eq!(
        report
            .checks
            .iter()
            .find(|c| c.name == "receives_events")
            .unwrap()
            .occluder,
        Some(occ())
    );
}

#[tokio::test]
async fn review_unknown_editability_can_resolve_before_deadline() {
    let c = TestClock::default();
    let mut calls = 0;
    let report = auto_wait(&c, Duration::from_millis(150), false, |_| {
        calls += 1;
        let ready = calls > 1;
        async move {
            let mut f = facts();
            f.editable = Evidence::Absent;
            f.affordances = Evidence::Known(vec![Affordance::TypeText]);
            f.role = if ready {
                Evidence::Known("textfield".into())
            } else {
                Evidence::Unknown("role".into())
            };
            evaluate(Intent::TypeText, &f, headless())
        }
    })
    .await;
    assert_eq!(calls, 2);
    assert_eq!(report.verdict, Verdict::Pass);
}

#[test]
fn review_stability_without_pointer_is_not_measurable_when_bounds_absent() {
    let mut f = facts();
    f.bounds = Evidence::Absent;
    f.stable = Evidence::Known(false);
    for intent in [
        Intent::Toggle,
        Intent::Check,
        Intent::Uncheck,
        Intent::Expand,
        Intent::Collapse,
        Intent::Scroll,
    ] {
        assert_eq!(
            evaluate(intent, &f, headless()).verdict,
            Verdict::Pass,
            "{intent:?}"
        );
    }
}
