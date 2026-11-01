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
