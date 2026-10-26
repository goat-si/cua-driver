//! Wall-clock and node budget for the accessibility walk behind
//! `get_window_state` (`timeout_ms` / `max_elements`).
//!
//! Every backend walks its own accessibility API (AT-SPI, AX, UIA/MSAA), but
//! the contract is shared: the walk stops at whichever budget runs out first,
//! the tool returns the PARTIAL tree it has, and the response says so with the
//! same structured fields and the same note on every platform.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Why a walk stopped before it had visited every node it discovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkStop {
    /// The `timeout_ms` wall-clock budget ran out.
    Timeout,
    /// The `max_elements` node budget ran out.
    NodeBudget,
    /// The application could not be reached through the platform
    /// accessibility API before `timeout_ms` ran out (still launching, or not
    /// yet registered), so nothing was walked.
    AppLookupTimeout,
    /// The target repeatedly timed out or stopped answering its accessibility
    /// API and is inside the hung-target circuit-breaker cooldown window.
    AppUnresponsive,
}

impl WalkStop {
    pub fn as_str(self) -> &'static str {
        match self {
            WalkStop::Timeout => "timeout",
            WalkStop::NodeBudget => "node_budget",
            WalkStop::AppLookupTimeout => "app_lookup_timeout",
            WalkStop::AppUnresponsive => "app_unresponsive",
        }
    }
}

/// Budget for one walk. Call [`WalkBudget::admit`] before visiting each node;
/// a refused node is counted as discovered but not visited.
///
/// The clock starts at the first admitted node, so `timeout_ms` bounds the
/// walk itself rather than the backend's setup (resolving the application and
/// the target window, which on a freshly launched app can take most of a
/// second on a slow host). Each backend bounds that setup separately with its
/// own backstop past `timeout_ms`.
#[derive(Debug)]
pub struct WalkBudget {
    /// Set by the first [`WalkBudget::admit`].
    started: Option<Instant>,
    timeout: Duration,
    timeout_ms: u64,
    /// False for [`WalkBudget::nodes_only`], whose clock never fires.
    time_bounded: bool,
    max_elements: usize,
    visited: usize,
    pending: usize,
    stop: Option<WalkStop>,
}

impl WalkBudget {
    /// A walk bounded by `timeout_ms` of wall-clock time and `max_elements`
    /// nodes.
    pub fn new(timeout_ms: u64, max_elements: usize) -> Self {
        Self {
            started: None,
            timeout: Duration::from_millis(timeout_ms),
            timeout_ms,
            time_bounded: true,
            max_elements,
            visited: 0,
            pending: 0,
            stop: None,
        }
    }

    /// A walk bounded only by `max_elements` (internal callers with no
    /// caller-supplied time budget).
    pub fn nodes_only(max_elements: usize) -> Self {
        // A year: never fires, without overflowing Instant.
        Self {
            time_bounded: false,
            ..Self::new(365 * 24 * 60 * 60 * 1000, max_elements)
        }
    }

    /// The caller's wall-clock budget, or `None` for a node-only budget.
    /// Backends use it to bound setup that happens before the first node,
    /// such as waiting for a launching application to become reachable.
    pub fn time_limit(&self) -> Option<Duration> {
        self.time_bounded.then_some(self.timeout)
    }

    /// Admit one node for visiting. `false` means the walk must not visit it:
    /// a budget has run out, and the node is counted as pending.
    pub fn admit(&mut self) -> bool {
        let started = *self.started.get_or_insert_with(Instant::now);
        if self.stop.is_none() {
            if self.visited >= self.max_elements {
                self.stop = Some(WalkStop::NodeBudget);
            } else if started.elapsed() >= self.timeout {
                self.stop = Some(WalkStop::Timeout);
            }
        }
        if self.stop.is_some() {
            self.pending += 1;
            return false;
        }
        self.visited += 1;
        true
    }

    /// Whether the wall-clock budget has run out (for phases that cannot be
    /// interrupted per node). Never true before the walk has started.
    pub fn expired(&self) -> bool {
        self.started
            .is_some_and(|started| started.elapsed() >= self.timeout)
    }

    /// Record that the walk ran out of time outside [`WalkBudget::admit`]
    /// (a bulk fetch that returned after the deadline).
    pub fn stop_for_timeout(&mut self) {
        self.stop.get_or_insert(WalkStop::Timeout);
    }

    /// Record that the application never became reachable within the budget,
    /// so the walk visited nothing.
    pub fn stop_for_app_lookup_timeout(&mut self) {
        self.stop.get_or_insert(WalkStop::AppLookupTimeout);
    }

    /// Record that the target application is inside a hung-target cooldown
    /// window after a prior accessibility timeout, short-circuiting the walk.
    pub fn stop_for_app_unresponsive(&mut self) {
        self.stop.get_or_insert(WalkStop::AppUnresponsive);
    }

    pub fn outcome(&self) -> WalkOutcome {
        WalkOutcome {
            timeout_ms: self.timeout_ms,
            stop: self.stop,
            nodes_visited: self.visited,
            nodes_pending: self.pending,
            elapsed_ms: self
                .started
                .map_or(0, |started| started.elapsed().as_millis() as u64),
        }
    }
}

/// What a finished walk reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalkOutcome {
    pub timeout_ms: u64,
    pub stop: Option<WalkStop>,
    pub nodes_visited: usize,
    pub nodes_pending: usize,
    pub elapsed_ms: u64,
}

impl WalkOutcome {
    /// A walk that produced nothing because the whole budget ran out before
    /// the backend returned (an uninterruptible provider call).
    pub fn timed_out(timeout_ms: u64, elapsed: Duration) -> Self {
        Self {
            timeout_ms,
            stop: Some(WalkStop::Timeout),
            nodes_visited: 0,
            nodes_pending: 0,
            elapsed_ms: elapsed.as_millis() as u64,
        }
    }

    pub fn truncated(&self) -> bool {
        self.stop.is_some()
    }

    pub fn reason(&self) -> Option<&'static str> {
        self.stop.map(WalkStop::as_str)
    }

    /// Write the shared walk fields into a `get_window_state` payload.
    pub fn apply(&self, structured: &mut Value) {
        structured["truncated"] = json!(self.truncated());
        if let Some(reason) = self.reason() {
            structured["truncation_reason"] = json!(reason);
        }
        structured["nodes_visited"] = json!(self.nodes_visited);
        structured["nodes_pending"] = json!(self.nodes_pending);
        structured["walk_elapsed_ms"] = json!(self.elapsed_ms);
        structured["timeout_ms"] = json!(self.timeout_ms);
    }

    /// The PARTIAL TREE note for the tree text, when the walk was cut short.
    pub fn note(&self) -> Option<String> {
        self.truncated().then(|| {
            truncation_note(
                self.reason(),
                self.timeout_ms,
                self.nodes_visited,
                self.nodes_pending,
            )
        })
    }
}

/// The note that heads a partial tree, naming why the walk stopped.
pub fn truncation_note(
    reason: Option<&str>,
    timeout_ms: u64,
    visited: usize,
    pending: usize,
) -> String {
    let why = match reason {
        Some("timeout") => format!("the {timeout_ms} ms timeout_ms budget ran out"),
        Some("node_budget") => "the max_elements node budget ran out".to_owned(),
        Some("app_unresponsive") => {
            "the application stopped answering its accessibility API".to_owned()
        }
        Some("app_lookup_timeout") => format!(
            "the application was not reachable through its accessibility API (AT-SPI, AX) \
             within {timeout_ms} ms; it may still be launching"
        ),
        Some("huge_container") => "a container with more children than can be enumerated \
            (e.g. a spreadsheet's cell grid) was not expanded"
            .to_owned(),
        Some(other) => other.to_owned(),
        None => "the walk stopped early".to_owned(),
    };
    let remedy = if reason == Some("node_budget") {
        "retry with a larger max_elements"
    } else {
        "retry with a larger timeout_ms (e.g. 5000)"
    };
    format!(
        "⚠️ PARTIAL TREE: {why} after {visited} node(s) ({pending} discovered but not visited). \
         Every element listed is real; elements after the cut are missing. If the element you \
         need is absent, {remedy} or narrow with query / max_depth."
    )
}

/// Default short-term cooldown window during which a target that timed out is
/// short-circuited instead of issuing another blocking accessibility walk.
pub const DEFAULT_HUNG_TARGET_COOLDOWN: Duration = Duration::from_millis(1_000);

/// Key identifying an accessibility walk target: `(pid, window_id_or_0)`.
pub type WalkTargetKey = (i32, u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BreakerEntry {
    tripped_at: Instant,
    consecutive_timeouts: u32,
}

/// Short-term circuit breaker that prevents back-to-back accessibility walks
/// against an unresponsive target from cascading into multi-second stalls.
#[derive(Debug)]
pub struct HungTargetCircuitBreaker {
    cooldown: Duration,
    entries: Mutex<HashMap<WalkTargetKey, BreakerEntry>>,
}

impl Default for HungTargetCircuitBreaker {
    fn default() -> Self {
        Self::new(DEFAULT_HUNG_TARGET_COOLDOWN)
    }
}

impl HungTargetCircuitBreaker {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `target` is currently inside its cooldown window at `now`.
    pub fn is_open_at(&self, target: WalkTargetKey, now: Instant) -> bool {
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = map.get(&target).copied() else {
            return false;
        };
        if now.saturating_duration_since(entry.tripped_at) < self.cooldown {
            true
        } else {
            map.remove(&target);
            false
        }
    }

    /// Pre-walk gate: if `target` timed out within `self.cooldown` of `now`,
    /// trips `budget.stop_for_app_unresponsive()` and returns `false` so the
    /// caller can short-circuit before issuing blocking accessibility IPC.
    pub fn preflight_at(
        &self,
        target: WalkTargetKey,
        budget: &mut WalkBudget,
        now: Instant,
    ) -> bool {
        if self.is_open_at(target, now) {
            budget.stop_for_app_unresponsive();
            false
        } else {
            true
        }
    }

    pub fn preflight(&self, target: WalkTargetKey, budget: &mut WalkBudget) -> bool {
        self.preflight_at(target, budget, Instant::now())
    }

    /// Post-walk bookkeeper: records `WalkStop::Timeout` /
    /// `WalkStop::AppUnresponsive` at `now`, or clears the entry when a walk
    /// completes without an accessibility responsiveness timeout.
    pub fn record_outcome_at(&self, target: WalkTargetKey, outcome: &WalkOutcome, now: Instant) {
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match outcome.stop {
            Some(WalkStop::Timeout | WalkStop::AppUnresponsive) => {
                let prev = map.get(&target).map_or(0, |e| e.consecutive_timeouts);
                map.insert(
                    target,
                    BreakerEntry {
                        tripped_at: now,
                        consecutive_timeouts: prev.saturating_add(1),
                    },
                );
            }
            None | Some(WalkStop::NodeBudget) => {
                map.remove(&target);
            }
            Some(WalkStop::AppLookupTimeout) => {}
        }
    }

    pub fn record_outcome(&self, target: WalkTargetKey, outcome: &WalkOutcome) {
        self.record_outcome_at(target, outcome, Instant::now());
    }

    /// Number of consecutive timeouts recorded for `target` if still active.
    pub fn consecutive_timeouts_at(&self, target: WalkTargetKey, now: Instant) -> u32 {
        if !self.is_open_at(target, now) {
            return 0;
        }
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&target)
            .map_or(0, |e| e.consecutive_timeouts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_budget_stops_and_counts_pending() {
        let mut budget = WalkBudget::nodes_only(2);
        assert!(budget.admit());
        assert!(budget.admit());
        assert!(!budget.admit());
        assert!(!budget.admit());
        let outcome = budget.outcome();
        assert_eq!(outcome.reason(), Some("node_budget"));
        assert_eq!((outcome.nodes_visited, outcome.nodes_pending), (2, 2));
    }

    #[test]
    fn only_a_caller_budget_bounds_setup_waits() {
        assert_eq!(
            WalkBudget::new(1500, 10).time_limit(),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(WalkBudget::nodes_only(10).time_limit(), None);
    }

    #[test]
    fn an_unreachable_application_reports_app_lookup_timeout() {
        let mut budget = WalkBudget::new(1000, 10);
        budget.stop_for_app_lookup_timeout();
        let outcome = budget.outcome();
        assert!(outcome.truncated());
        assert_eq!(outcome.reason(), Some("app_lookup_timeout"));
        assert_eq!((outcome.nodes_visited, outcome.nodes_pending), (0, 0));
        let mut structured = json!({});
        outcome.apply(&mut structured);
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["truncation_reason"], "app_lookup_timeout");
        let note = outcome.note().unwrap();
        assert!(note.contains("not reachable through its accessibility API"));
        assert!(note.contains("1000 ms"));
    }

    #[test]
    fn expired_deadline_stops_with_timeout() {
        let mut budget = WalkBudget::new(0, 100);
        // Setup before the first node does not count against the budget.
        assert!(!budget.expired());
        assert!(!budget.admit());
        assert!(budget.expired());
        let outcome = budget.outcome();
        assert_eq!(outcome.reason(), Some("timeout"));
        assert_eq!((outcome.nodes_visited, outcome.nodes_pending), (0, 1));
    }

    #[test]
    fn the_clock_starts_at_the_first_node() {
        let mut budget = WalkBudget::new(50, 100);
        std::thread::sleep(Duration::from_millis(80));
        assert!(budget.admit(), "setup time must not exhaust the budget");
    }

    #[test]
    fn a_complete_walk_is_not_truncated() {
        let mut budget = WalkBudget::new(60_000, 10);
        for _ in 0..10 {
            assert!(budget.admit());
        }
        let outcome = budget.outcome();
        assert!(!outcome.truncated());
        assert_eq!(outcome.note(), None);
        let mut structured = json!({});
        outcome.apply(&mut structured);
        assert_eq!(structured["truncated"], false);
        assert!(structured.get("truncation_reason").is_none());
        assert_eq!(structured["nodes_visited"], 10);
        assert_eq!(structured["timeout_ms"], 60_000);
    }

    #[test]
    fn a_timed_out_walk_reports_the_budget() {
        let outcome = WalkOutcome::timed_out(1000, Duration::from_millis(1200));
        let mut structured = json!({});
        outcome.apply(&mut structured);
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["truncation_reason"], "timeout");
        let note = outcome.note().unwrap();
        assert!(note.starts_with("⚠️ PARTIAL TREE: the 1000 ms timeout_ms budget ran out"));
    }

    #[test]
    fn truncation_note_names_the_budget_and_the_remedy() {
        let note = truncation_note(Some("timeout"), 1000, 240, 88);
        assert!(note.contains("PARTIAL TREE"));
        assert!(note.contains("1000 ms"));
        assert!(note.contains("240 node(s)"));
        assert!(note.contains("88 discovered"));
        assert!(note.contains("timeout_ms"));
        assert!(note.contains("query"));
        let note = truncation_note(Some("node_budget"), 1000, 5000, 3);
        assert!(note.contains("max_elements"));
        let note = truncation_note(Some("app_unresponsive"), 1000, 3, 0);
        assert!(note.contains("stopped answering"));
        let note = truncation_note(Some("app_lookup_timeout"), 250, 0, 0);
        assert!(note.contains("250 ms"));
        assert!(note.contains("still be launching"));
        let note = truncation_note(Some("huge_container"), 1000, 1918, 0);
        assert!(note.contains("not expanded"));
    }

    #[test]
    fn hung_target_circuit_breaker_short_circuits_within_cooldown_and_resets_after() {
        let breaker = HungTargetCircuitBreaker::new(Duration::from_millis(1_000));
        let target: WalkTargetKey = (4242, 100);
        let t0 = Instant::now();

        let mut first_budget = WalkBudget::new(1000, 100);
        assert!(breaker.preflight_at(target, &mut first_budget, t0));
        first_budget.stop_for_timeout();
        let timed_out = first_budget.outcome();
        breaker.record_outcome_at(target, &timed_out, t0);
        assert_eq!(breaker.consecutive_timeouts_at(target, t0), 1);

        // Within the 1000ms cooldown window: preflight trips AppUnresponsive.
        let mut second_budget = WalkBudget::new(1000, 100);
        assert!(!breaker.preflight_at(target, &mut second_budget, t0 + Duration::from_millis(250)));
        assert!(!second_budget.admit());
        let short_circuited = second_budget.outcome();
        assert_eq!(short_circuited.reason(), Some("app_unresponsive"));
        assert!(short_circuited
            .note()
            .unwrap()
            .contains("stopped answering its accessibility API"));

        // A different window or PID is unaffected.
        let mut sibling_budget = WalkBudget::new(1000, 100);
        assert!(breaker.preflight_at(
            (4242, 101),
            &mut sibling_budget,
            t0 + Duration::from_millis(250)
        ));

        // Once the cooldown expires, preflight permits a new walk.
        let t_expired = t0 + Duration::from_millis(1_001);
        let mut third_budget = WalkBudget::new(1000, 100);
        assert!(breaker.preflight_at(target, &mut third_budget, t_expired));
        assert_eq!(breaker.consecutive_timeouts_at(target, t_expired), 0);
    }

    #[test]
    fn hung_target_circuit_breaker_clears_on_complete_or_node_budget_walk() {
        let breaker = HungTargetCircuitBreaker::new(Duration::from_millis(1_000));
        let target: WalkTargetKey = (77, 1);
        let t0 = Instant::now();

        let timed_out = WalkOutcome::timed_out(1000, Duration::from_millis(1000));
        breaker.record_outcome_at(target, &timed_out, t0);
        assert!(breaker.is_open_at(target, t0 + Duration::from_millis(100)));

        // A walk that hits NodeBudget proves the app is responsive again.
        let mut node_capped = WalkBudget::nodes_only(1);
        assert!(node_capped.admit());
        assert!(!node_capped.admit());
        breaker.record_outcome_at(
            target,
            &node_capped.outcome(),
            t0 + Duration::from_millis(200),
        );
        assert!(!breaker.is_open_at(target, t0 + Duration::from_millis(250)));
    }
}
