//! Read-only semantic actionability samples for an already admitted target.
//!
//! The caller owns admission and its per-PID lease. This module never acquires a
//! lease or performs a mutation. A cancelled blocking sample may continue to
//! read, but cannot start an action or dispatch an input event.
use super::{
    evaluate, ActionabilityReport, Adapter, Clock, Duration, ElementFacts, Evidence, Intent,
    Policy, Reader, Scope, ScopeReader, StabilitySampler, Verdict,
};
use crate::ax::{
    actionability::NativeReader,
    bindings::{self, AXUIElementRef},
    snapshot::RetainedElement,
};
use crate::goat::action_latency as latency;
use core_foundation::base::{CFRelease, CFTypeRef};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Instant,
};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> i32;
}

/// Read only identity data; an absent or failed read never proves a target.
pub(super) trait WindowReader {
    type Element;
    fn pid(&mut self, target: &Self::Element) -> Evidence<i32>;
    fn window(&mut self, target: &Self::Element) -> Evidence<Self::Element>;
    fn window_id(&mut self, window: &Self::Element) -> Evidence<u32>;
}

struct NativeWindowReader;
impl WindowReader for NativeWindowReader {
    type Element = RetainedElement;
    fn pid(&mut self, target: &Self::Element) -> Evidence<i32> {
        let mut pid = 0;
        let status = unsafe { AXUIElementGetPid(target.as_ptr() as AXUIElementRef, &mut pid) };
        if status == bindings::kAXErrorSuccess && pid > 0 {
            Evidence::Known(pid)
        } else {
            Evidence::Unknown("target PID unavailable".into())
        }
    }
    fn window(&mut self, target: &Self::Element) -> Evidence<Self::Element> {
        let window =
            unsafe { bindings::copy_element_attr(target.as_ptr() as AXUIElementRef, "AXWindow") };
        match window {
            Some(window) => {
                let retained = unsafe { RetainedElement::retain(window as usize) };
                unsafe { CFRelease(window as CFTypeRef) };
                Evidence::Known(retained)
            }
            None => Evidence::Unknown("target AXWindow unavailable".into()),
        }
    }
    fn window_id(&mut self, window: &Self::Element) -> Evidence<u32> {
        match unsafe { bindings::ax_get_window_id(window.as_ptr() as AXUIElementRef) } {
            Some(id) => Evidence::Known(id),
            None => Evidence::Unknown("AX window ID unavailable".into()),
        }
    }
}

/// Bind the actual target and its AX window to the admission's exact PID and
/// window. Every incomplete read remains unknown; this does not dispatch.
pub(super) fn bind_exact_with<R: WindowReader>(
    reader: &mut R,
    target: R::Element,
    pid: i32,
    window_id: u32,
) -> Evidence<(R::Element, R::Element)> {
    match reader.pid(&target) {
        Evidence::Known(actual) if actual == pid => {}
        _ => return Evidence::Unknown("target PID does not match admission".into()),
    }
    let window = match reader.window(&target) {
        Evidence::Known(window) => window,
        _ => return Evidence::Unknown("target AXWindow is unproven".into()),
    };
    match reader.window_id(&window) {
        Evidence::Known(actual) if actual == window_id => Evidence::Known((target, window)),
        _ => Evidence::Unknown("target AXWindow does not match admission".into()),
    }
}

/// Owned intent names cross worker boundaries without sending a raw AX pointer.
#[derive(Clone)]
pub(crate) enum SemanticIntent {
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

pub(super) fn unavailable(reason: String) -> ActionabilityReport {
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

/// Resolve an already admitted target while borrowing its existing per-PID
/// lease. Callers must treat an unknown binding as transient and retry it in
/// `auto_wait`; this helper neither acquires a lease nor makes a refusal.
pub(super) async fn bind_exact_after_lease(
    lease: &crate::tools::BackgroundMutationLease,
    target: RetainedElement,
    window_id: u32,
) -> Evidence<SemanticProbe> {
    bind_exact_after_admission(lease.pid(), target, window_id).await
}

pub(super) async fn bind_exact_after_admission(
    pid: i32,
    target: RetainedElement,
    window_id: u32,
) -> Evidence<SemanticProbe> {
    let binding = latency::spawn_blocking(move || {
        bind_exact_with(&mut NativeWindowReader, target, pid, window_id)
    })
    .await;
    match binding {
        Ok(Evidence::Known((target, window))) => {
            Evidence::Known(SemanticProbe::new(pid, target, window))
        }
        Ok(Evidence::Unknown(reason)) => Evidence::Unknown(reason),
        Ok(Evidence::Absent) => Evidence::Unknown("target window is absent".into()),
        Err(error) => Evidence::Unknown(format!("target binding worker failed: {error}")),
    }
}
