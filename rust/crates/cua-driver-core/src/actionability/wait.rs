use super::{ActionabilityReport, DispositionIntent, ElementFacts, Verdict};
use std::{future::Future, pin::Pin, time::Duration};

pub const DEFAULT_BUDGET: Duration = Duration::from_millis(5000);
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Default type-text budget, including the reference's per-character allowance.
pub fn default_type_budget(text: &str) -> Duration {
    let characters = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
    Duration::from_millis(5_000u64.saturating_add(characters.saturating_mul(30)))
}
/// Monotonic elapsed time and a cancellation-safe sleep from the same clock.
pub trait Clock {
    fn now(&self) -> Duration;
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Probe futures receive their remaining budget and are cancelled at its end.
/// Zero budget polls the initial probe once; a pending probe cannot overrun it.
pub async fn auto_wait<C, P, F>(
    clock: &C,
    budget: Duration,
    side_effectful_prologue_ran: bool,
    mut probe: P,
) -> ActionabilityReport
where
    C: Clock + Sync,
    P: FnMut(Duration) -> F + Send,
    F: Future<Output = ActionabilityReport> + Send,
{
    let start = clock.now();
    let deadline = start.saturating_add(budget);
    let mut samples = 0u32;
    let mut last: Option<ActionabilityReport> = None;
    loop {
        match (clock.now() >= deadline, last.take()) {
            (true, Some(report)) => {
                return timeout(
                    report,
                    clock.now().saturating_sub(start),
                    samples,
                    side_effectful_prologue_ran,
                )
            }
            (_, previous) => last = previous,
        }
        let remaining = deadline.saturating_sub(clock.now());
        let future = probe(remaining);
        tokio::pin!(future);
        let timer = clock.sleep(remaining);
        tokio::pin!(timer);
        let result =
            tokio::select! { biased; result = &mut future => Some(result), _ = &mut timer => None };
        let probe_timed_out = result.is_none();
        if let Some(report) = result {
            samples = samples.saturating_add(1);
            last = Some(report);
        }
        let mut report = last.take().unwrap_or_else(|| ActionabilityReport {
            verdict: Verdict::Transient {
                reason: "probe deadline expired".into(),
                unknown: true,
            },
            checks: Vec::new(),
            waited_ms: 0,
            samples: 0,
            last_facts: None::<ElementFacts>,
            disposition: None,
        });
        report.samples = samples;
        // Disposition is determined here, never inherited from a probe.
        report.disposition = None;
        report.waited_ms = clock
            .now()
            .saturating_sub(start)
            .as_millis()
            .min(u64::MAX as u128) as u64;
        if matches!(report.verdict, Verdict::Pass)
            && !probe_timed_out
            && (budget.is_zero() || clock.now() <= deadline)
        {
            return report;
        }
        if matches!(report.verdict, Verdict::Terminal { .. }) {
            if !side_effectful_prologue_ran {
                report.disposition = Some(DispositionIntent::NotDeliveredSafe);
            }
            return report;
        }
        if probe_timed_out || clock.now() >= deadline || budget.is_zero() {
            return timeout(
                report,
                clock.now().saturating_sub(start),
                samples,
                side_effectful_prologue_ran,
            );
        }
        last = Some(report);
        clock
            .sleep(POLL_INTERVAL.min(deadline.saturating_sub(clock.now())))
            .await;
    }
}
fn timeout(
    mut report: ActionabilityReport,
    elapsed: Duration,
    samples: u32,
    prologue_ran: bool,
) -> ActionabilityReport {
    let reason = match &report.verdict {
        Verdict::Transient { reason, .. } => reason.clone(),
        Verdict::Pass => "probe completed after deadline".into(),
        Verdict::Terminal { reason, .. } => reason.clone(),
    };
    report.verdict = Verdict::Terminal {
        code: "timeout",
        reason,
    };
    report.waited_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
    report.samples = samples;
    report.disposition = if prologue_ran {
        None
    } else {
        Some(DispositionIntent::NotDeliveredSafe)
    };
    report
}
