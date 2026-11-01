//! Platform-neutral pre-dispatch actionability evidence and evaluation.
//!
//! Unlike the reference, unknown hit tests block: insufficient evidence cannot
//! establish reachability. The battery point is advisory; platforms must still
//! validate the final delivery coordinate. This module is not connected to any
//! existing tool.
//!
//! Platform adopters: use `Click` with `Policy::headless()` for AXPress and
//! `RightClick` with that policy for AXShowMenu; use `Open` for AXOpen,
//! `PerformAction { name }` for an exact advertised custom action, and
//! `SelectText` for text-range selection. For these three semantic operations in
//! the background, use `Policy::headless()`; `Policy::background_window()` is
//! only for physical pointer delivery. For physical background-window pointer
//! input, use `Policy::background_window()` and collect hits with
//! `collect_window_hits` against the target window. In that policy `visible`,
//! `offscreen` and bounds must describe visibility within that same window, not
//! the frontmost desktop, and enabled evidence is also required. Use
//! `ScrollWheel` for physical wheel input on macOS,
//! Windows, and Linux; the wheel can bubble to a scrollable ancestor, so no
//! scroll affordance is required. Use `Scroll` for semantic scroll actions.
//! Foreground wheel delivery uses `Policy::headed()` and `collect_hits`;
//! headless policy refuses physical wheel delivery. Coordinate-only input is
//! outside this battery: call
//! `coordinate_only()` and do not treat its result as a pass. Convert a terminal
//! report to a tool result with `ActionabilityReport::refusal_result()` and pass
//! the result through unchanged, including its typed disposition.

mod evaluate;
mod evidence;
mod receives_events;
mod report;
mod requirements;
mod stability;
mod vocabulary;
mod wait;

pub use evaluate::{evaluate, Policy};
pub use evidence::{ElementFacts, Evidence, Point, Rect};
pub use receives_events::{
    candidate_points, collect_hits, collect_window_hits, Hit, HitEvidence, Occluder,
};
pub use report::{ActionabilityReport, Check, CheckStatus, DispositionIntent, Verdict};
pub use requirements::{Intent, Requirements};
pub use stability::{geometry_matches, StabilitySampler, SAMPLE_INTERVAL};
pub use vocabulary::{is_primary, supported_affordances, Affordance};
pub use wait::{auto_wait, default_type_budget, Clock, DEFAULT_BUDGET, POLL_INTERVAL};

/// A coordinate-only target has no element evidence to evaluate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinateOutcome {
    NotEvaluated,
}

pub const fn coordinate_only() -> CoordinateOutcome {
    CoordinateOutcome::NotEvaluated
}

#[cfg(test)]
mod tests;
