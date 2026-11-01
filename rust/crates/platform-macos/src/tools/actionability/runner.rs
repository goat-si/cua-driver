//! Ordered actionability and effect history for a single admitted action.
//!
//! Callers choose the retained target and actuator under their existing leases.
//! Each new target, actuator, or preparation requires a fresh `step` probe.
//! Probes must be read-only: a timed-out blocking read may outlive its future.
use super::{wait, ActionabilityReport, Clock, Duration, Future, ToolResult, Verdict};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Effect {
    /// Activation, a key-window prime, or another preparation attempt.
    Prologue,
    /// A target mutation, including a pointer primer, reveal, or wheel.
    Mutation,
}

/// What the actuator proved after it was invoked. `ProvenUnsent` is reserved
/// for the existing exact native error cases that prove no input was sent.
pub(super) enum Execution {
    Continue(ToolResult),
    ProvenUnsent(ToolResult),
    Stop(ToolResult),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum StepResult {
    Continue,
    Stop,
}

pub(super) struct Runner {
    result: ToolResult,
    prologue_attempted: bool,
    sent_or_uncertain: bool,
    // A cancelled in-flight effect may have dispatched. No later step may
    // start until its outcome is known; cancellation never clears this flag.
    pending_effect: bool,
    stopped: bool,
}
impl Runner {
    pub(super) fn new(result: ToolResult) -> Self {
        Self {
            result,
            prologue_attempted: false,
            sent_or_uncertain: false,
            pending_effect: false,
            stopped: false,
        }
    }
    fn has_effect_history(&self) -> bool {
        self.prologue_attempted || self.sent_or_uncertain || self.pending_effect
    }
    /// Run the route's mandatory cleanup even after a refusal. Cleanup owns
    /// the result so it can report an error without losing existing evidence.
    pub(super) async fn finish<C, Cleanup>(self, cleanup: C) -> ToolResult
    where
        C: FnOnce(ToolResult) -> Cleanup,
        Cleanup: Future<Output = ToolResult>,
    {
        cleanup(self.result).await
    }

    /// Run one stage of a route. `probe` receives the remaining budget and is
    /// never allowed to dispatch. `perform` is not even constructed until the
    /// battery passes. `in_flight` must honestly describe an uncertain attempt;
    /// it becomes the retained result before the first effect starts. Later
    /// refusals preserve the result of the previous effect unchanged. The caller
    /// must call `finish` after `Stop`; cleanup is not a newly gated mutation.
    pub(super) async fn step<C, P, Probe, A, Action>(
        &mut self,
        clock: &C,
        budget: Duration,
        probe: P,
        effect: Effect,
        in_flight: ToolResult,
        perform: A,
    ) -> StepResult
    where
        C: Clock + Sync,
        P: FnMut(Duration) -> Probe + Send,
        Probe: Future<Output = ActionabilityReport> + Send,
        A: FnOnce() -> Action,
        Action: Future<Output = Execution>,
    {
        if self.stopped || self.pending_effect {
            self.stopped = true;
            return StepResult::Stop;
        }
        let report = wait(clock, budget, self.has_effect_history(), probe).await;
        match report.verdict {
            Verdict::Pass => {}
            Verdict::Terminal { .. } => {
                self.stopped = true;
                if !self.has_effect_history() {
                    self.result = report.refusal_result().expect("terminal report");
                }
                return StepResult::Stop;
            }
            Verdict::Transient { .. } => {
                unreachable!("auto_wait returns only pass or terminal")
            }
        }
        let prior_history = self.has_effect_history();
        if !prior_history {
            self.result = in_flight;
        }
        if effect == Effect::Prologue {
            self.prologue_attempted = true;
        }
        self.pending_effect = true;
        let outcome = perform().await;
        self.pending_effect = false;
        match outcome {
            Execution::ProvenUnsent(result) => {
                if !prior_history && effect == Effect::Mutation {
                    self.result = result;
                }
                StepResult::Continue
            }
            Execution::Continue(result) => {
                if effect == Effect::Mutation {
                    self.sent_or_uncertain = true;
                }
                self.result = result;
                StepResult::Continue
            }
            Execution::Stop(result) => {
                if effect == Effect::Mutation {
                    self.sent_or_uncertain = true;
                }
                self.result = result;
                self.stopped = true;
                StepResult::Stop
            }
        }
    }
}
