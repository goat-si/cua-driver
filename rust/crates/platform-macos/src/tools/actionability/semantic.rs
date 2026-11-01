//! Read-only semantic actionability samples for an already admitted target.
//!
//! The caller owns admission and its per-PID lease. This module never acquires a
//! lease or performs a mutation. A cancelled blocking sample may continue to
//! read, but cannot start an action or dispatch an input event.
use super::{
    evaluate, ActionabilityReport, Adapter, Clock, Duration, ElementFacts, Intent, Policy, Reader,
    Scope, ScopeReader, StabilitySampler, Verdict,
};
use crate::ax::{actionability::NativeReader, bindings::AXUIElementRef, snapshot::RetainedElement};
use crate::goat::action_latency as latency;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Instant,
};

/// Owned intent names cross worker boundaries without sending a raw AX pointer.
#[derive(Clone)]
pub(super) enum SemanticIntent {
    Click,
    Open,
    RightClick,
    Select,
    Focus,
    PerformAction(String),
}
impl SemanticIntent {
    fn intent(&self) -> Intent<'_> {
        match self {
            Self::Click => Intent::Click,
            Self::Open => Intent::Open,
            Self::RightClick => Intent::RightClick,
            Self::Select => Intent::Select,
            Self::Focus => Intent::Focus,
            Self::PerformAction(name) => Intent::PerformAction { name },
        }
    }
}

/// One monotonic origin is shared by the waiter and all its blocking samples.
pub(super) struct SemanticClock(Instant);
impl Default for SemanticClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for SemanticClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// Evaluate one exact target using injected readers. The sampler contains only
/// geometry and timing, never an AX pointer or an actuator.
pub(super) fn sample_with<E, R, H, C>(
    target: E,
    window: E,
    reader: &mut R,
    scope_reader: &mut H,
    clock: &C,
    stability: &mut StabilitySampler,
    intent: &SemanticIntent,
) -> ActionabilityReport
where
    E: Copy,
    R: Reader<Element = E>,
    H: ScopeReader<E>,
    C: Clock,
{
    let mut adapter = Adapter {
        target,
        window,
        scope: Scope::WindowInternal,
        stability: std::mem::take(stability),
    };
    let facts: ElementFacts = adapter.semantic_facts(reader, scope_reader, clock);
    *stability = adapter.stability;
    evaluate(intent.intent(), Policy::headless(), &facts)
}

fn unavailable(reason: String) -> ActionabilityReport {
    ActionabilityReport {
        verdict: Verdict::Transient {
            reason,
            unknown: true,
        },
        checks: vec![],
        waited_ms: 0,
        samples: 0,
        last_facts: None,
        disposition: None,
    }
}

/// Retained objects are owned for every blocking read, including one that
/// outlives an async timeout. Raw AX pointers exist only inside that read.
pub(super) struct SemanticProbe {
    pid: i32,
    target: RetainedElement,
    window: RetainedElement,
    stability: Arc<Mutex<StabilitySampler>>,
    clock: Arc<SemanticClock>,
}
impl SemanticProbe {
    pub(super) fn new(pid: i32, target: RetainedElement, window: RetainedElement) -> Self {
        Self {
            pid,
            target,
            window,
            stability: Arc::new(Mutex::new(StabilitySampler::default())),
            clock: Arc::new(SemanticClock::default()),
        }
    }
    pub(super) fn clock(&self) -> &SemanticClock {
        &self.clock
    }

    pub(super) async fn sample(&self, intent: SemanticIntent) -> ActionabilityReport {
        let target = self.target.clone();
        let window = self.window.clone();
        let stability = self.stability.clone();
        let clock = self.clock.clone();
        let pid = self.pid;
        match latency::spawn_blocking(move || {
            let Ok(mut stability) = stability.lock() else {
                return unavailable("semantic sample state is unavailable".into());
            };
            let target_ptr = target.as_ptr() as AXUIElementRef;
            let window_ptr = window.as_ptr() as AXUIElementRef;
            let mut scope = super::scope::NativeScope::new(pid);
            sample_with(
                target_ptr,
                window_ptr,
                &mut NativeReader,
                &mut scope,
                &*clock,
                &mut stability,
                &intent,
            )
        })
        .await
        {
            Ok(report) => report,
            Err(error) => unavailable(format!("semantic sample worker failed: {error}")),
        }
    }
}
