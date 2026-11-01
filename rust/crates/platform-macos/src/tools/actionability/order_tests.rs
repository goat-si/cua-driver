use super::runner::{Effect, Execution, Runner, StepResult};
use super::*;
use cua_driver_core::{
    action_record::{ActionEffect, ActionExecutionRecord, ActionTransport, RequestedDelivery},
    actionability::DispositionIntent,
};
use std::{
    collections::VecDeque,
    future::{pending, Future},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

type Log = Arc<Mutex<Vec<&'static str>>>;
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
fn report(verdict: Verdict) -> ActionabilityReport {
    ActionabilityReport {
        verdict,
        checks: vec![],
        waited_ms: 0,
        samples: 1,
        last_facts: None,
        disposition: None,
    }
}
fn denied() -> Verdict {
    Verdict::Terminal {
        code: "unsupported_action",
        reason: "denied".into(),
    }
}
fn transient() -> Verdict {
    Verdict::Transient {
        reason: "not ready".into(),
        unknown: true,
    }
}
fn sent_result() -> ToolResult {
    ToolResult::text("existing")
        .with_structured(serde_json::json!({"effect":"unverifiable","dispatched":true,"note":"keep note","warning":"keep warning"}))
        .with_action_record(ActionExecutionRecord::new(ActionEffect::Unverifiable, ActionTransport::MacosAxAction, RequestedDelivery::Background))
}
fn assert_sent(result: &ToolResult) {
    let expected = sent_result();
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(result.action_record, expected.action_record);
    assert_eq!(result.disposition, expected.disposition);
}
async fn stage(
    runner: &mut Runner,
    clock: &FakeClock,
    log: &Log,
    verdicts: Vec<Verdict>,
    effect: Effect,
    tag: &'static str,
) -> StepResult {
    let replies = Arc::new(Mutex::new(VecDeque::from(verdicts)));
    let before = log.lock().unwrap().len();
    let probe_log = log.clone();
    let effect_log = log.clone();
    runner
        .step(
            clock,
            Duration::from_millis(500),
            move |_| {
                let replies = replies.clone();
                let probe_log = probe_log.clone();
                async move {
                    assert_eq!(
                        probe_log.lock().unwrap().len(),
                        before,
                        "effect during observation"
                    );
                    report(replies.lock().unwrap().pop_front().expect("scripted probe"))
                }
            },
            effect,
            sent_result(),
            move || async move {
                effect_log.lock().unwrap().push(tag);
                Execution::Continue(sent_result())
            },
        )
        .await
}
fn runner() -> Runner {
    Runner::new(ToolResult::text("old"))
}
fn assert_refusal(result: &ToolResult) {
    let mut expected = report(denied());
    expected.disposition = Some(DispositionIntent::NotDeliveredSafe);
    let expected = expected.refusal_result().unwrap();
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(result.disposition, expected.disposition);
}

#[tokio::test]
async fn terminal_before_prologue_forwards_core_refusal_unchanged() {
    let mut r = runner();
    let clock = FakeClock::default();
    let log = Log::default();
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![denied()],
            Effect::Mutation,
            "click"
        )
        .await,
        StepResult::Stop
    );
    assert_refusal(&r.finish(|result| async { result }).await);
    assert!(log.lock().unwrap().is_empty());
    // An actuator's proven-unsent result permits another route to retain the
    // clean refusal contract; it does not turn an AX attempt into sent input.
    let mut r = runner();
    assert_eq!(
        r.step(
            &clock,
            Duration::ZERO,
            |_| async { report(Verdict::Pass) },
            Effect::Mutation,
            sent_result(),
            || async { Execution::ProvenUnsent(ToolResult::text("unsent")) }
        )
        .await,
        StepResult::Continue
    );
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![denied()],
            Effect::Mutation,
            "fallback"
        )
        .await,
        StepResult::Stop
    );
    assert_refusal(&r.finish(|result| async { result }).await);
    assert!(log.lock().unwrap().is_empty());
}
#[tokio::test]
async fn transient_waits_without_native_effects() {
    let mut r = runner();
    let clock = FakeClock::default();
    let log = Log::default();
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![transient(), transient(), Verdict::Pass],
            Effect::Mutation,
            "click"
        )
        .await,
        StepResult::Continue
    );
    assert_sent(&r.finish(|result| async { result }).await);
    assert_eq!(*log.lock().unwrap(), ["click"]);
    assert_eq!(clock.now(), Duration::from_millis(200));
}
#[tokio::test]
async fn activation_attempt_then_timeout_is_unverifiable() {
    let mut r = runner();
    let clock = FakeClock::default();
    let log = Log::default();
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![Verdict::Pass],
            Effect::Prologue,
            "activate"
        )
        .await,
        StepResult::Continue
    );
    assert_eq!(
        r.step(
            &clock,
            Duration::ZERO,
            |_| async { report(transient()) },
            Effect::Mutation,
            ToolResult::text("must not replace"),
            || async { Execution::Continue(ToolResult::text("bad")) }
        )
        .await,
        StepResult::Stop
    );
    assert_sent(&r.finish(|result| async { result }).await);
    assert_eq!(*log.lock().unwrap(), ["activate"]);
}
#[tokio::test]
async fn reveal_then_wheel_refusal_keeps_sent_effect() {
    let mut r = runner();
    let clock = FakeClock::default();
    let log = Log::default();
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![Verdict::Pass],
            Effect::Mutation,
            "reveal"
        )
        .await,
        StepResult::Continue
    );
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![denied()],
            Effect::Mutation,
            "wheel"
        )
        .await,
        StepResult::Stop
    );
    assert_sent(&r.finish(|result| async { result }).await);
    assert_eq!(*log.lock().unwrap(), ["reveal"]);
}
#[tokio::test]
async fn primer_then_target_change_is_not_unsent() {
    let mut r = runner();
    let clock = FakeClock::default();
    let log = Log::default();
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![Verdict::Pass],
            Effect::Mutation,
            "primer"
        )
        .await,
        StepResult::Continue
    );
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![denied()],
            Effect::Mutation,
            "new target"
        )
        .await,
        StepResult::Stop
    );
    assert_sent(&r.finish(|result| async { result }).await);
    assert_eq!(*log.lock().unwrap(), ["primer"]);
}
#[tokio::test]
async fn post_wheel_fallback_denial_stops_chain_preserving_result() {
    let mut r = runner();
    let clock = FakeClock::default();
    let log = Log::default();
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![Verdict::Pass],
            Effect::Mutation,
            "wheel"
        )
        .await,
        StepResult::Continue
    );
    // A later proven-unsent attempt cannot erase the already sent wheel.
    assert_eq!(
        r.step(
            &clock,
            Duration::ZERO,
            |_| async { report(Verdict::Pass) },
            Effect::Mutation,
            ToolResult::text("bad"),
            || async { Execution::ProvenUnsent(ToolResult::text("bad")) }
        )
        .await,
        StepResult::Continue
    );
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![denied()],
            Effect::Mutation,
            "fallback"
        )
        .await,
        StepResult::Stop
    );
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![Verdict::Pass],
            Effect::Mutation,
            "later"
        )
        .await,
        StepResult::Stop
    );
    assert_sent(&r.finish(|result| async { result }).await);
    assert_eq!(*log.lock().unwrap(), ["wheel"]);
}
struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn cancelled_probe_cannot_dispatch_late() {
    let mut r = runner();
    let clock = FakeClock::default();
    let log = Log::default();
    let dropped = Arc::new(AtomicUsize::new(0));
    let probe_drop = dropped.clone();
    let effect_log = log.clone();
    assert_eq!(
        r.step(
            &clock,
            Duration::ZERO,
            move |_| {
                let probe_drop = probe_drop.clone();
                async move {
                    let _guard = Dropped(probe_drop);
                    pending::<ActionabilityReport>().await
                }
            },
            Effect::Mutation,
            sent_result(),
            move || async move {
                effect_log.lock().unwrap().push("late dispatch");
                Execution::Continue(sent_result())
            }
        )
        .await,
        StepResult::Stop
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(
        r.finish(|result| async { result }).await.disposition,
        Some(cua_driver_contract::ErrorDisposition::NOT_DELIVERED)
    );
    // Cancelling an already started effect also fences all later stages and
    // keeps its preinstalled unverifiable result.
    for effect in [Effect::Mutation, Effect::Prologue] {
        let mut r = runner();
        {
            let future = r.step(
                &clock,
                Duration::ZERO,
                |_| async { report(Verdict::Pass) },
                effect,
                sent_result(),
                || async { pending::<Execution>().await },
            );
            tokio::pin!(future);
            tokio::select! { biased; _ = &mut future => panic!("effect unexpectedly finished"), _ = async {} => {} }
        }
        assert_eq!(
            stage(
                &mut r,
                &clock,
                &log,
                vec![Verdict::Pass],
                Effect::Mutation,
                "later"
            )
            .await,
            StepResult::Stop
        );
        assert_sent(&r.finish(|result| async { result }).await);
    }
    assert!(log.lock().unwrap().is_empty());
}
#[tokio::test]
async fn failure_cleanup_preserves_note_warning_and_action_record() {
    let mut r = runner();
    let clock = FakeClock::default();
    let log = Log::default();
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![Verdict::Pass],
            Effect::Mutation,
            "button down"
        )
        .await,
        StepResult::Continue
    );
    assert_eq!(
        stage(
            &mut r,
            &clock,
            &log,
            vec![denied()],
            Effect::Mutation,
            "new action"
        )
        .await,
        StepResult::Stop
    );
    // The runner invokes cleanup even when the target step was refused.
    let cleanup_log = log.clone();
    let result = r
        .finish(move |result| async move {
            cleanup_log.lock().unwrap().push("release");
            result
        })
        .await;
    assert_sent(&result);
    assert_eq!(*log.lock().unwrap(), ["button down", "release"]);
}
