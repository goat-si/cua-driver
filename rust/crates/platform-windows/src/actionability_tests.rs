use super::*;
use cua_driver_core::actionability::{Affordance, Evidence, Rect};
use std::{
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
};

struct FakeClock;
impl Clock for FakeClock {
    fn now(&self) -> Duration {
        Duration::ZERO
    }
    fn sleep(&self, _: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn disabled_facts_never_write() {
    let writes = AtomicUsize::new(0);
    let facts = ElementFacts {
        visible: Evidence::Known(true),
        offscreen: Evidence::Known(false),
        enabled: Evidence::Known(false),
        editable: Evidence::Known(true),
        stable: Evidence::Absent,
        receives_events: Evidence::Absent,
        bounds: Evidence::Known(Rect {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        }),
        affordances: Evidence::Known(vec![Affordance::SetValue]),
        advertised_actions: Evidence::Absent,
        selectable_text: Evidence::Absent,
        role: Evidence::Known("textfield".into()),
        nonspatial_visible: false,
    };
    gate_then_write(
        &FakeClock,
        Duration::ZERO,
        |_| std::future::ready(facts.clone()),
        || async {
            writes.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await;
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}
