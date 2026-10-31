//! Platform-neutral pre-dispatch actionability evidence and evaluation.
//!
//! Unlike the reference, unknown hit tests block: insufficient evidence cannot
//! establish reachability. The battery point is advisory; platforms must still
//! validate the final delivery coordinate. Timeout reports use `timeout`, as the
//! reference does. JSON is an additive refusal detail and is never emitted for a
//! successful action. This module is not connected to any existing tool.

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
pub use receives_events::{candidate_points, collect_hits, Hit, HitEvidence, Occluder};
pub use report::{ActionabilityReport, Check, CheckStatus, DispositionIntent, Verdict};
pub use requirements::{Intent, Requirements};
pub use stability::{geometry_matches, StabilitySampler, SAMPLE_INTERVAL};
pub use vocabulary::{is_primary, supported_affordances, Affordance};
pub use wait::{auto_wait, default_type_budget, Clock, DEFAULT_BUDGET, POLL_INTERVAL};

#[cfg(test)]
mod tests;
