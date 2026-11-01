use super::*;
use cua_driver_core::{
    action_record::{ActionEffect, ActionExecutionRecord, ActionTransport, RequestedDelivery},
    actionability::Rect,
};
use std::{
    pin::Pin,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

#[derive(Default)]
struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            self.0
                .fetch_add(duration.as_millis() as u64, Ordering::SeqCst);
        })
    }
}
fn facts() -> ElementFacts {
    ElementFacts {
        visible: Evidence::Known(true),
        offscreen: Evidence::Known(false),
        enabled: Evidence::Known(true),
        editable: Evidence::Absent,
        stable: Evidence::Known(true),
        receives_events: Evidence::Absent,
        bounds: Evidence::Known(Rect {
            x: 0.0,
            y: 0.0,
            width: 20.0,
            height: 20.0,
        }),
        affordances: Evidence::Known(vec![Affordance::Press]),
        advertised_actions: Evidence::Known(vec!["AXPress".into()]),
        selectable_text: Evidence::Absent,
        role: Evidence::Absent,
        nonspatial_visible: false,
    }
}
fn sample(facts: &ElementFacts) -> ActionabilityReport {
    evaluate(Intent::Click, Policy::headless(), facts)
}
fn success() -> ToolResult {
    ToolResult::text("existing").with_structured(serde_json::json!({"effect":"unverifiable","dispatched":true,"note":"keep","warning":"keep"}))
        .with_action_record(ActionExecutionRecord::new(ActionEffect::Unverifiable, ActionTransport::MacosAxAction, RequestedDelivery::Background))
}
fn assert_same(a: &ToolResult, b: &ToolResult) {
    assert_eq!(
        serde_json::to_value(a).unwrap(),
        serde_json::to_value(b).unwrap()
    );
    assert_eq!(a.action_record, b.action_record);
    assert_eq!(a.disposition, b.disposition);
}

async fn open_action_read_failure_blocks_fallback(route: &'static str) {
    for actions in [
        Evidence::Unknown("AX error -25204".into()),
        Evidence::Unknown("wrong CF type in action names".into()),
        Evidence::Absent,
    ] {
        let clock = FakeClock::default();
        let pixel = AtomicUsize::new(0);
        let ax = AtomicUsize::new(0);
        let advertised = advertised_action_status(actions, "AXOpen");
        let chosen = may_use_semantic_route(&advertised);
        let result = super::super::double_click::dispatch_open(
            &clock,
            Duration::from_millis(200),
            chosen,
            route,
            |_, _| {
                route_semantic_probe(
                    &advertised,
                    None,
                    unsafe { crate::ax::snapshot::RetainedElement::retain(0) },
                    0,
                    0,
                    semantic::SemanticIntent::Open,
                )
            },
            |_| async {
                if chosen {
                    ax.fetch_add(1, Ordering::SeqCst);
                } else {
                    pixel.fetch_add(1, Ordering::SeqCst);
                }
                success()
            },
        )
        .await;
        assert!(
            result.is_err(),
            "{route}: unknown action names must not dispatch"
        );
        assert_eq!(pixel.load(Ordering::SeqCst), 0);
        assert_eq!(ax.load(Ordering::SeqCst), 0);
        assert!(clock.now() > Duration::ZERO);
    }
    let clock = FakeClock::default();
    let pixel = AtomicUsize::new(0);
    let chosen =
        may_use_semantic_route(&advertised_action_status(Evidence::Known(vec![]), "AXOpen"));
    let result = super::super::double_click::dispatch_open(
        &clock,
        Duration::ZERO,
        chosen,
        route,
        |_, _| Box::pin(async { panic!("proven absent should use pixel fallback") }),
        |_| async {
            pixel.fetch_add(1, Ordering::SeqCst);
            success()
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(pixel.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn double_click_unknown_action_names_block_pixel_and_ax() {
    open_action_read_failure_blocks_fallback("double_click").await;
}

#[tokio::test]
async fn click_count_two_unknown_action_names_block_pixel_and_ax() {
    open_action_read_failure_blocks_fallback("click_count_2").await;
}

#[tokio::test]
async fn right_click_unknown_action_names_block_pixel_and_ax() {
    for actions in [
        Evidence::Unknown("AX error -25204".into()),
        Evidence::Unknown("wrong CF type in action names".into()),
        Evidence::Absent,
    ] {
        let clock = FakeClock::default();
        let pixel = AtomicUsize::new(0);
        let ax = AtomicUsize::new(0);
        let advertised = advertised_action_status(actions, "AXShowMenu");
        let chosen = may_use_semantic_route(&advertised);
        let result = super::super::right_click::dispatch_show_menu(
            &clock,
            Duration::from_millis(200),
            chosen,
            (),
            |_, _| {
                route_semantic_probe(
                    &advertised,
                    None,
                    unsafe { crate::ax::snapshot::RetainedElement::retain(0) },
                    0,
                    0,
                    semantic::SemanticIntent::RightClick,
                )
            },
            |_| async {
                if chosen {
                    ax.fetch_add(1, Ordering::SeqCst);
                } else {
                    pixel.fetch_add(1, Ordering::SeqCst);
                }
                success()
            },
        )
        .await;
        assert!(result.is_err(), "unknown action names must not dispatch");
        assert_eq!(pixel.load(Ordering::SeqCst), 0);
        assert_eq!(ax.load(Ordering::SeqCst), 0);
        assert!(clock.now() > Duration::ZERO);
    }
    let clock = FakeClock::default();
    let pixel = AtomicUsize::new(0);
    let chosen = may_use_semantic_route(&advertised_action_status(
        Evidence::Known(vec![]),
        "AXShowMenu",
    ));
    let result = super::super::right_click::dispatch_show_menu(
        &clock,
        Duration::ZERO,
        chosen,
        (),
        |_, _| Box::pin(async { panic!("proven absent should use pixel fallback") }),
        |_| async {
            pixel.fetch_add(1, Ordering::SeqCst);
            success()
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(pixel.load(Ordering::SeqCst), 1);
}

#[test]
fn click_right_uses_semantic_right_click_intent() {
    use semantic::SemanticIntent;
    let request = super::super::click::effective_click_action("press", "right");
    let advertised = vec!["AXShowMenu".into()];
    let resolved =
        crate::input::ax_actions::resolve_element_action(&request, &advertised, false).unwrap();
    assert!(matches!(
        super::super::click::click_semantic_intent(resolved),
        SemanticIntent::RightClick
    ));
}

#[test]
fn click_open_uses_semantic_open_intent() {
    use semantic::SemanticIntent;
    let request = super::super::click::effective_click_action("open", "left");
    let advertised = vec!["AXOpen".into()];
    let resolved =
        crate::input::ax_actions::resolve_element_action(&request, &advertised, false).unwrap();
    assert!(matches!(
        super::super::click::click_semantic_intent(resolved),
        SemanticIntent::Open
    ));
    assert!(matches!(
        super::super::click::click_semantic_intent("AXPress"),
        SemanticIntent::Click
    ));
    assert!(
        matches!(super::super::click::click_semantic_intent("Name:exact"), SemanticIntent::PerformAction(name) if name == "Name:exact")
    );
}
#[tokio::test]
async fn click_axpress_refusal_prevents_first_effect() {
    let clock = FakeClock::default();
    for disabled in [true, false] {
        let mut f = facts();
        if disabled {
            f.enabled = Evidence::Known(false);
        } else {
            f.visible = Evidence::Known(false);
        }
        let count = AtomicUsize::new(0);
        let outcome = dispatch_semantic(
            &clock,
            Duration::ZERO,
            (),
            |_, _| {
                let report = sample(&f);
                Box::pin(async move { report })
            },
            |_| async {
                count.fetch_add(1, Ordering::SeqCst);
                success()
            },
        )
        .await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(outcome.is_err());
    }
    let count = AtomicUsize::new(0);
    let good = facts();
    let result = dispatch_semantic(
        &clock,
        Duration::ZERO,
        (),
        |_, _| {
            let report = sample(&good);
            Box::pin(async move { report })
        },
        |_| async {
            count.fetch_add(1, Ordering::SeqCst);
            success()
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn click_axpress_transient_waits_without_effect() {
    let clock = FakeClock::default();
    let count = AtomicUsize::new(0);
    let samples = AtomicUsize::new(0);
    let result = dispatch_semantic(
        &clock,
        Duration::from_millis(500),
        (),
        |_, _| {
            assert_eq!(count.load(Ordering::SeqCst), 0);
            let mut f = facts();
            if samples.fetch_add(1, Ordering::SeqCst) == 0 {
                f.enabled = Evidence::Unknown("pending".into());
            }
            let report = sample(&f);
            Box::pin(async move { report })
        },
        |_| async {
            count.fetch_add(1, Ordering::SeqCst);
            success()
        },
    )
    .await;
    assert!(samples.load(Ordering::SeqCst) >= 2);
    assert!(clock.now() >= Duration::from_millis(100));
    assert!(result.is_ok());
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn click_axpress_refusal_forwards_core_result_unchanged() {
    let clock = FakeClock::default();
    let mut disabled = facts();
    disabled.enabled = Evidence::Known(false);
    let expected = wait(&clock, Duration::ZERO, false, |_| async {
        sample(&disabled)
    })
    .await
    .refusal_result()
    .unwrap();
    let result = dispatch_semantic(
        &clock,
        Duration::ZERO,
        (),
        |_, _| {
            let report = sample(&disabled);
            Box::pin(async move { report })
        },
        |_| async { success() },
    )
    .await
    .unwrap_err();
    assert_same(&result, &expected);
    let good = facts();
    let result = dispatch_semantic(
        &clock,
        Duration::ZERO,
        (),
        |_, _| {
            let report = sample(&good);
            Box::pin(async move { report })
        },
        |_| async { success() },
    )
    .await
    .unwrap();
    assert_same(&result, &success());
}

#[tokio::test]
async fn right_click_owned_cancel_checks_recorded_menu() {
    let clock = FakeClock::default();
    for allowed in [false, true] {
        let sent = AtomicUsize::new(0);
        let sampled = AtomicUsize::new(0);
        let sent_ref = &sent;
        let result = super::super::right_click::dispatch_owned_cancel(
            &clock,
            Duration::ZERO,
            41usize, // recorded menu; the original opener is a different object
            |menu, _| {
                assert_eq!(*menu, 41);
                sampled.fetch_add(1, Ordering::SeqCst);
                let mut f = facts();
                f.advertised_actions = Evidence::Known(if allowed {
                    vec!["AXCancel".into()]
                } else {
                    vec![]
                });
                let report = evaluate(
                    Intent::PerformAction { name: "AXCancel" },
                    Policy::headless(),
                    &f,
                );
                Box::pin(async move { report })
            },
            |menu| async move {
                assert_eq!(menu, 41);
                sent_ref.fetch_add(1, Ordering::SeqCst);
                success()
            },
        )
        .await;
        assert!(sampled.load(Ordering::SeqCst) > 0);
        assert_eq!(sent.load(Ordering::SeqCst), usize::from(allowed));
        assert_eq!(result.is_ok(), allowed);
    }
}

#[tokio::test]
async fn click_custom_unknown_blocks_action() {
    let clock = FakeClock::default();
    let exact = "Name:열기\nTarget:object\nSelector:perform:";
    for known in [false, true] {
        let count = AtomicUsize::new(0);
        let count_ref = &count;
        let result = super::super::click::dispatch_custom_action(
            &clock,
            Duration::ZERO,
            exact.to_owned(),
            |name, _| {
                assert_eq!(name, exact);
                let mut f = facts();
                f.advertised_actions = if known {
                    Evidence::Known(vec![exact.into()])
                } else {
                    Evidence::Unknown("actions unavailable".into())
                };
                let report = evaluate(Intent::PerformAction { name }, Policy::headless(), &f);
                Box::pin(async move { report })
            },
            |name| async move {
                assert_eq!(name, exact);
                count_ref.fetch_add(1, Ordering::SeqCst);
                success()
            },
        )
        .await;
        assert_eq!(count.load(Ordering::SeqCst), usize::from(known));
        assert_eq!(result.is_ok(), known);
    }
}

#[tokio::test]
async fn double_click_axopen_gates_before_activation() {
    let clock = FakeClock::default();
    for route in ["double_click", "click_count_2"] {
        for allowed in [false, true] {
            let activated = AtomicUsize::new(0);
            let activated_ref = &activated;
            let result = super::super::double_click::dispatch_open(
                &clock,
                Duration::ZERO,
                true,
                route,
                |_, _| {
                    let mut f = facts();
                    f.enabled = Evidence::Known(allowed);
                    f.affordances = Evidence::Known(vec![Affordance::Open]);
                    f.advertised_actions = Evidence::Known(vec!["AXOpen".into()]);
                    let report = evaluate(Intent::Open, Policy::headless(), &f);
                    Box::pin(async move { report })
                },
                |actual| async move {
                    assert_eq!(actual, route);
                    activated_ref.fetch_add(1, Ordering::SeqCst);
                    success()
                },
            )
            .await;
            assert_eq!(activated.load(Ordering::SeqCst), usize::from(allowed));
            assert_eq!(result.is_ok(), allowed);
        }
    }
}

#[tokio::test]
async fn right_click_axshowmenu_refusal_blocks_dispatch() {
    let clock = FakeClock::default();
    for allowed in [false, true] {
        let sent = AtomicUsize::new(0);
        let sent_ref = &sent;
        let result = super::super::right_click::dispatch_show_menu(
            &clock,
            Duration::ZERO,
            true,
            (),
            |_, _| {
                let mut f = facts();
                f.enabled = Evidence::Known(allowed);
                f.affordances = Evidence::Known(vec![Affordance::ShowMenu]);
                f.advertised_actions = Evidence::Known(vec!["AXShowMenu".into()]);
                let report = evaluate(Intent::RightClick, Policy::headless(), &f);
                Box::pin(async move { report })
            },
            |_| async move {
                sent_ref.fetch_add(1, Ordering::SeqCst);
                success()
            },
        )
        .await;
        assert_eq!(sent.load(Ordering::SeqCst), usize::from(allowed));
        assert_eq!(result.is_ok(), allowed);
    }
}

#[tokio::test]
async fn coordinate_pointer_routes_remain_not_evaluated() {
    assert_eq!(
        select(Route::Coordinate),
        Selection::Coordinate(CoordinateOutcome::NotEvaluated)
    );
    let clock = FakeClock::default();
    let sent = AtomicUsize::new(0);
    let sent_ref = &sent;
    let result = super::super::right_click::dispatch_show_menu(
        &clock,
        Duration::ZERO,
        false,
        (),
        |_, _| Box::pin(async { panic!("pixel fallback sampled as AXShowMenu") }),
        |_| async move {
            sent_ref.fetch_add(1, Ordering::SeqCst);
            success()
        },
    )
    .await;
    assert!(result.is_ok());
    let sent_ref = &sent;
    let result = super::super::double_click::dispatch_open(
        &clock,
        Duration::ZERO,
        false,
        (),
        |_, _| Box::pin(async { panic!("pixel fallback sampled as AXOpen") }),
        |_| async move {
            sent_ref.fetch_add(1, Ordering::SeqCst);
            success()
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(sent.load(Ordering::SeqCst), 2);
}
