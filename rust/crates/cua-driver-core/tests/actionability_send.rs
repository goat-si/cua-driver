use std::{future::Future, pin::Pin, time::Duration};

use cua_driver_core::actionability::{auto_wait, ActionabilityReport, Clock, Verdict};

struct SyncClock;
impl Clock for SyncClock {
    fn now(&self) -> Duration {
        Duration::ZERO
    }
    fn sleep(&self, _: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}

#[test]
fn auto_wait_future_is_send() {
    fn assert_send<T: Send>(_: T) {}
    let clock = SyncClock;
    assert_send(auto_wait(&clock, Duration::ZERO, false, |_| async {
        ActionabilityReport {
            verdict: Verdict::Pass,
            checks: vec![],
            waited_ms: 0,
            samples: 1,
            last_facts: None,
            disposition: None,
        }
    }));
}
