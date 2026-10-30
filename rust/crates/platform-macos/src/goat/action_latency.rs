//! Opt-in, content-free timings for the platform invocation (not the MCP relay).
//! Set CUA_DRIVER_GOAT_LATENCY_TRACE=1 before starting the process. Detached
//! detector/restore tails are excluded: this measures the work before return.

use std::cell::RefCell;
use std::future::Future;
use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::tool::{ProtectedResourceOwnership, Tool, ToolDef, ToolRegistry};
use serde_json::Value;

tokio::task_local! {
    static CURRENT: Arc<Trace>;
}
thread_local! {
    static BLOCKING: RefCell<Option<Arc<Trace>>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy)]
pub(crate) enum Stage {
    AxResolve,
    Snapshot,
    Protect,
    /// No longer timed: the post-action focus hold runs detached. Kept so
    /// the trace line keeps its columns.
    #[allow(dead_code)]
    Settle,
    Action,
    ActionBlocking,
    LeaseDrop,
    Detector,
    DetectorSleep,
    DetectorEnumeration,
    /// Waiting for the agent cursor to arrive at the target.
    Glide,
    /// The AX reads before an element action is sent (actions, enabled,
    /// role, title, centre, selection container). Inside ActionBlocking.
    Preflight,
    /// Making a background app's window key before a press. Inside
    /// ActionBlocking.
    KeyPrime,
    /// The AXUIElementPerformAction call itself. Inside ActionBlocking.
    AxPerform,
    /// get_window_state: the WindowServer owner pre-flight.
    ReadOwner,
    /// get_window_state: the accessibility walk.
    ReadWalk,
    KeyEntry,
    KeyScopePrime,
    KeyFinalSample,
    KeyPost,
    KeyHandBack,
    KeyLease,
    KeyGateAgain,
    KeyGuardUntargeted,
    KeyConflict,
    KeyHold,
}

impl Stage {
    const COUNT: usize = 26;
    fn is_primary(self) -> bool {
        !matches!(
            self,
            Self::DetectorSleep
                | Self::DetectorEnumeration
                | Self::Preflight
                | Self::KeyPrime
                | Self::AxPerform
        )
    }
}

struct Metrics {
    durations: [Duration; Stage::COUNT],
    primary_intervals: Vec<(Duration, Duration)>,
    enumerations: u64,
    detector_exit: &'static str,
    tail: &'static str,
    finished: bool,
    key_belief: &'static str,
    key_scope: &'static str,
}

struct Trace {
    id: u64,
    tool: &'static str,
    started: Instant,
    metrics: Mutex<Metrics>,
}

impl Trace {
    fn new(tool: &'static str) -> Arc<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Arc::new(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            tool,
            started: Instant::now(),
            metrics: Mutex::new(Metrics {
                durations: [Duration::ZERO; Stage::COUNT],
                primary_intervals: Vec::new(),
                enumerations: 0,
                detector_exit: "not_run",
                tail: "none",
                finished: false,
                key_belief: "ineligible",
                key_scope: "skipped",
            }),
        })
    }

    fn finish(&self, outcome: &str) -> String {
        let total = self.started.elapsed();
        let mut metrics = self.metrics.lock().unwrap_or_else(|e| e.into_inner());
        metrics.finished = true;
        // Stage spans nest (lease drop can be inside snapshot/detector).
        // Union their intervals so measured + other equals total without
        // double-counting. Individual stage columns remain inclusive.
        let measured = interval_union(&mut metrics.primary_intervals, total);
        let d = &metrics.durations;
        format!(
            "cua_goat_latency request_id={} tool={} outcome={} total_us={} ax_resolve_us={} snapshot_us={} protect_us={} focus_settle_us={} action_guarded_us={} action_blocking_us={} lease_drop_us={} detector_us={} detector_sleep_us={} detector_enumerations={} detector_enumeration_us={} detector_exit={} tail={} glide_us={} preflight_us={} key_prime_us={} ax_perform_us={} read_owner_us={} read_walk_us={} entry_us={} prime_us={} final_sample_us={} post_us={} hand_back_us={} lease_us={} gate_again_us={} guard_untargeted_us={} conflict_us={} key_hold_us={} measured_us={} other_us={} key_belief={} scope={}",
            self.id, self.tool, outcome, total.as_micros(),
            d[Stage::AxResolve as usize].as_micros(),
            d[Stage::Snapshot as usize].as_micros(),
            d[Stage::Protect as usize].as_micros(),
            d[Stage::Settle as usize].as_micros(),
            d[Stage::Action as usize].as_micros(),
            d[Stage::ActionBlocking as usize].as_micros(),
            d[Stage::LeaseDrop as usize].as_micros(),
            d[Stage::Detector as usize].as_micros(),
            d[Stage::DetectorSleep as usize].as_micros(),
            metrics.enumerations,
            d[Stage::DetectorEnumeration as usize].as_micros(),
            metrics.detector_exit, metrics.tail,
            d[Stage::Glide as usize].as_micros(),
            d[Stage::Preflight as usize].as_micros(),
            d[Stage::KeyPrime as usize].as_micros(),
            d[Stage::AxPerform as usize].as_micros(),
            d[Stage::ReadOwner as usize].as_micros(),
            d[Stage::ReadWalk as usize].as_micros(),
            d[Stage::KeyEntry as usize].as_micros(),
            d[Stage::KeyScopePrime as usize].as_micros(),
            d[Stage::KeyFinalSample as usize].as_micros(),
            d[Stage::KeyPost as usize].as_micros(),
            d[Stage::KeyHandBack as usize].as_micros(),
            d[Stage::KeyLease as usize].as_micros(),
            d[Stage::KeyGateAgain as usize].as_micros(),
            d[Stage::KeyGuardUntargeted as usize].as_micros(),
            d[Stage::KeyConflict as usize].as_micros(),
            d[Stage::KeyHold as usize].as_micros(),
            measured.as_micros(),
            total.as_micros() - measured.as_micros(),
            metrics.key_belief, metrics.key_scope,
        )
    }
}

fn interval_union(intervals: &mut [(Duration, Duration)], total: Duration) -> Duration {
    intervals.sort_unstable();
    let mut end = Duration::ZERO;
    let mut measured = Duration::ZERO;
    for &(start, stop) in intervals.iter() {
        let stop = stop.min(total);
        measured += stop.saturating_sub(start.max(end));
        end = end.max(stop);
    }
    measured
}

fn current() -> Option<Arc<Trace>> {
    CURRENT
        .try_with(Arc::clone)
        .ok()
        .or_else(|| BLOCKING.with(|slot| slot.borrow().clone()))
}

pub(crate) fn key_belief(eligible: bool, hit: bool) {
    if let Some(trace) = current() {
        let mut metrics = trace.metrics.lock().unwrap_or_else(|e| e.into_inner());
        if !metrics.finished {
            metrics.key_belief = if hit {
                "hit"
            } else if eligible {
                "miss"
            } else {
                "ineligible"
            };
            metrics.key_scope = if hit { "skipped" } else { "entered" };
        }
    }
}

pub(crate) struct Span(Option<(Arc<Trace>, Stage, Instant)>);

pub(crate) fn span(stage: Stage) -> Span {
    Span(current().map(|trace| (trace, stage, Instant::now())))
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some((trace, stage, started)) = &self.0 {
            let stopped = Instant::now();
            let mut metrics = trace.metrics.lock().unwrap_or_else(|e| e.into_inner());
            if metrics.finished {
                return;
            }
            metrics.durations[*stage as usize] += stopped.duration_since(*started);
            if stage.is_primary() {
                metrics.primary_intervals.push((
                    started.duration_since(trace.started),
                    stopped.duration_since(trace.started),
                ));
            }
            if matches!(stage, Stage::DetectorEnumeration) {
                metrics.enumerations += 1;
            }
        }
    }
}

/// Time a synchronous section as `stage` (no-op without a trace).
pub(crate) fn section<T>(stage: Stage, run: impl FnOnce() -> T) -> T {
    let _span = span(stage);
    run()
}

pub(crate) fn resolve<T>(read: impl FnOnce() -> T) -> T {
    let _span = span(Stage::AxResolve);
    read()
}

pub(crate) async fn timed<T>(stage: Stage, future: impl Future<Output = T>) -> T {
    let _span = span(stage);
    future.await
}

pub(crate) fn detector_exit(reason: &'static str) {
    if let Some(trace) = current() {
        let mut metrics = trace.metrics.lock().unwrap_or_else(|e| e.into_inner());
        if !metrics.finished {
            metrics.detector_exit = reason;
        }
    }
}

pub(crate) fn tail_spawned(ok: bool) {
    if let Some(trace) = current() {
        let mut metrics = trace.metrics.lock().unwrap_or_else(|e| e.into_inner());
        if !metrics.finished {
            metrics.tail = if ok { "detached" } else { "spawn_failed" };
        }
    }
}

pub(crate) fn detector_sleep(duration: Duration) {
    let _span = span(Stage::DetectorSleep);
    std::thread::sleep(duration);
}

pub(crate) fn enumerate<T>(read: impl FnOnce() -> T) -> T {
    let _span = span(Stage::DetectorEnumeration);
    read()
}

/// Tokio task locals do not cross spawn_blocking. Install only a synchronous
/// thread-local scope, restored even on panic before the pool thread is reused.
pub(crate) fn spawn_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let Some(trace) = current() else {
        return tokio::task::spawn_blocking(f);
    };
    tokio::task::spawn_blocking(move || {
        struct Restore(Option<Arc<Trace>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                BLOCKING.with(|slot| slot.replace(self.0.take()));
            }
        }
        let _restore = Restore(BLOCKING.with(|slot| slot.replace(Some(trace))));
        f()
    })
}

pub(crate) fn spawn_action_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    spawn_blocking(move || {
        let _span = span(Stage::ActionBlocking);
        f()
    })
}

fn write_line(line: String) {
    // A closed diagnostics pipe must never panic or change a tool result.
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}

struct Finish<F: FnOnce(String)> {
    trace: Arc<Trace>,
    emit: Option<F>,
    outcome: &'static str,
}

impl<F: FnOnce(String)> Drop for Finish<F> {
    fn drop(&mut self) {
        if let Some(emit) = self.emit.take() {
            emit(self.trace.finish(self.outcome));
        }
    }
}

async fn invoke_traced(
    tool: &dyn Tool,
    name: &'static str,
    args: Value,
    emit: impl FnOnce(String) + Send,
) -> ToolResult {
    // A nested platform call is part of its enclosing action, not a new
    // request. Batch steps enter independently and therefore get separate ids.
    if current().is_some() {
        return tool.invoke(args).await;
    }
    let trace = Trace::new(name);
    let mut finish = Finish {
        trace: trace.clone(),
        emit: Some(emit),
        outcome: "interrupted",
    };
    let result = CURRENT.scope(trace, tool.invoke(args)).await;
    finish.outcome = if result.is_error == Some(true) {
        "error"
    } else {
        "ok"
    };
    result
}

struct TracedTool {
    inner: Box<dyn Tool>,
    name: &'static str,
}

#[async_trait]
impl Tool for TracedTool {
    fn def(&self) -> &ToolDef {
        self.inner.def()
    }
    fn has_independent_input_lane(&self, args: &Value) -> bool {
        self.inner.has_independent_input_lane(args)
    }
    async fn protected_resource_ownership(
        &self,
        adapter: &str,
        args: &Value,
    ) -> ProtectedResourceOwnership {
        self.inner.protected_resource_ownership(adapter, args).await
    }
    async fn protected_resource_scope(
        &self,
        adapter: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        self.inner.protected_resource_scope(adapter, args).await
    }
    async fn validate_protected_resource_scope(
        &self,
        adapter: &str,
        args: &Value,
        approved: &Value,
    ) -> Result<(), String> {
        self.inner
            .validate_protected_resource_scope(adapter, args, approved)
            .await
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        invoke_traced(self.inner.as_ref(), self.name, args, write_line).await
    }
}

/// Local registration facade; deref preserves all other registry operations.
/// When disabled it installs the original tools, with no invocation wrapper.
pub(crate) struct Registry<'a>(&'a mut ToolRegistry);

impl<'a> Registry<'a> {
    pub(crate) fn new(registry: &'a mut ToolRegistry) -> Self {
        Self(registry)
    }
    pub(crate) fn register(&mut self, tool: Box<dyn Tool>) {
        let tool = crate::tools::pixel_restore::wrap(tool);
        static ENABLED: OnceLock<bool> = OnceLock::new();
        let enabled = *ENABLED.get_or_init(|| {
            std::env::var_os("CUA_DRIVER_GOAT_LATENCY_TRACE").is_some_and(|v| v == "1")
        });
        let name = match tool.def().name.as_str() {
            "click" => "click",
            "double_click" => "double_click",
            "right_click" => "right_click",
            "drag" => "drag",
            "scroll" => "scroll",
            "type_text" => "type_text",
            "press_key" => "press_key",
            "hotkey" => "hotkey",
            "set_value" => "set_value",
            "invoke_menu" => "invoke_menu",
            "get_window_state" => "get_window_state",
            _ => {
                self.0.register(tool);
                return;
            }
        };
        if enabled {
            self.0.register(Box::new(TracedTool { inner: tool, name }));
        } else {
            self.0.register(tool);
        }
    }
}

impl Deref for Registry<'_> {
    type Target = ToolRegistry;
    fn deref(&self) -> &Self::Target {
        self.0
    }
}
impl DerefMut for Registry<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

#[cfg(test)]
#[path = "action_latency_tests.rs"]
mod tests;
