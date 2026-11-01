//! Pure aggregation of evidence from the Linux accessibility walk.

use cua_driver_contract::{
    CountEvidence, IncompleteReason, IncompleteReasonDetail as Detail,
    IncompleteReasonKind as Kind, NodeBoundary,
};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Event {
    ChildrenFailed,
    AttributeFailed,
    DepthLimit,
    HugeContainer,
    CollapsedMenu,
    NodeBudget,
    TimeBudget,
    UntrustedProvider,
    ScopeMismatch,
}

impl Event {
    fn reason(self) -> IncompleteReason {
        let (kind, detail) = match self {
            Self::ChildrenFailed => (Kind::ProviderFailure, Detail::ChildEnumerationFailed),
            Self::AttributeFailed => (Kind::ProviderFailure, Detail::AttributeReadFailed),
            Self::DepthLimit => (Kind::PolicyOmission, Detail::DepthLimit),
            Self::HugeContainer => (Kind::PolicyOmission, Detail::HugeContainer),
            Self::CollapsedMenu => (Kind::PolicyOmission, Detail::CollapsedMenu),
            Self::NodeBudget => (Kind::Budget, Detail::NodeBudget),
            Self::TimeBudget => (Kind::Budget, Detail::TimeBudget),
            Self::UntrustedProvider => (Kind::ProviderFailure, Detail::UntrustedProvider),
            Self::ScopeMismatch => (Kind::Unknown, Detail::ScopeMismatch),
        };
        IncompleteReason { kind, detail }
    }
}

/// Node positions are positions in the walk, not externally visible indices.
/// A boundary belongs only to the node whose direct children were omitted.
#[derive(Clone, Debug, Default)]
pub(crate) struct Completeness {
    pub(crate) reasons: Vec<IncompleteReason>,
    pub(crate) boundaries: HashMap<usize, NodeBoundary>,
    pub(crate) exhaustive: bool,
}

impl Completeness {
    pub(crate) fn new(exhaustive: bool) -> Self {
        Self {
            exhaustive,
            ..Self::default()
        }
    }

    pub(crate) fn depth_cut(&mut self, in_scope: bool, position: usize, count: CountEvidence) {
        if count != CountEvidence::Known(0) {
            self.record(in_scope, Event::DepthLimit, Some((position, count)));
        }
    }

    pub(crate) fn record(
        &mut self,
        in_scope: bool,
        event: Event,
        boundary: Option<(usize, CountEvidence)>,
    ) {
        if !in_scope {
            return;
        }
        let reason = event.reason();
        if !self.reasons.contains(&reason) {
            self.reasons.push(reason);
        }
        if let Some((position, children_skipped)) = boundary {
            let entry = self
                .boundaries
                .entry(position)
                .or_insert_with(|| NodeBoundary {
                    reasons: Vec::new(),
                    children_skipped,
                    descendants_omitted: None,
                });
            if !entry.reasons.contains(&reason) {
                entry.reasons.push(reason);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::walk_budget::WalkBudget;
    use serde_json::{json, Value};

    /// A fake reader emits only facts; no D-Bus calls or clocks are involved.
    struct Record {
        in_scope: bool,
        indexed: bool,
        position: usize,
        event: Option<Event>,
        direct_children: Option<CountEvidence>,
    }

    fn fake_walk(
        records: &[Record],
        exhaustive: bool,
    ) -> (Completeness, HashMap<usize, NodeBoundary>) {
        let mut c = Completeness::new(exhaustive);
        for record in records {
            if let Some(event) = record.event {
                c.record(
                    record.in_scope,
                    event,
                    record.direct_children.map(|count| (record.position, count)),
                );
            }
        }
        let indexed = records
            .iter()
            .filter(|r| r.in_scope && r.indexed)
            .filter_map(|r| {
                c.boundaries
                    .get(&r.position)
                    .cloned()
                    .map(|boundary| (r.position, boundary))
            })
            .collect();
        (c, indexed)
    }

    fn output(c: &Completeness) -> Value {
        let mut budget = WalkBudget::new(1000, usize::MAX);
        if c.exhaustive {
            budget.assert_exhaustive();
        }
        for reason in &c.reasons {
            budget.record_skip(*reason);
        }
        let mut result = json!({});
        budget.outcome().apply(&mut result);
        result
    }

    fn has(c: &Completeness, detail: &str) -> bool {
        output(c)["observation_incomplete_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["detail"] == detail)
    }

    #[test]
    fn in_scope_child_enumeration_failure_is_incomplete() {
        let mut c = Completeness::new(true);
        c.record(
            true,
            Event::ChildrenFailed,
            Some((1, CountEvidence::Unknown)),
        );
        assert_eq!(output(&c)["observation_complete"], false);
        assert!(has(&c, "child_enumeration_failed"));
        assert_eq!(c.boundaries[&1].children_skipped, CountEvidence::Unknown);
    }

    #[test]
    fn sibling_failure_does_not_affect_scope() {
        let mut c = Completeness::new(true);
        c.record(
            false,
            Event::ChildrenFailed,
            Some((3, CountEvidence::Known(2))),
        );
        assert_eq!(output(&c)["observation_complete"], true);
        assert!(c.boundaries.is_empty());
    }

    #[test]
    fn cuts_and_failures_preserve_direct_child_evidence() {
        for (event, detail) in [
            (Event::ChildrenFailed, "child_enumeration_failed"),
            (Event::DepthLimit, "depth_limit"),
            (Event::HugeContainer, "huge_container"),
            (Event::CollapsedMenu, "collapsed_menu"),
        ] {
            let mut c = Completeness::new(true);
            c.record(true, event, Some((7, CountEvidence::Known(3))));
            assert!(has(&c, detail));
            assert_eq!(c.boundaries[&7].children_skipped, CountEvidence::Known(3));
        }
    }

    #[test]
    fn attributes_budgets_and_scope_are_aggregated() {
        for (event, detail) in [
            (Event::AttributeFailed, "attribute_read_failed"),
            (Event::NodeBudget, "node_budget"),
            (Event::TimeBudget, "time_budget"),
            (Event::UntrustedProvider, "untrusted_provider"),
            (Event::ScopeMismatch, "scope_mismatch"),
        ] {
            let mut c = Completeness::new(true);
            c.record(true, event, None);
            assert!(has(&c, detail));
            assert!(c.boundaries.is_empty());
            assert_eq!(output(&c)["observation_complete"], false);
        }
    }

    #[test]
    fn trusted_clean_scope_and_unasserted_scope() {
        let clean = Completeness::new(true);
        assert_eq!(output(&clean)["observation_complete"], true);
        assert_eq!(output(&clean)["observation_incomplete_reasons"], json!([]));
        assert!(has(&Completeness::new(false), "not_asserted"));
    }

    #[test]
    fn depth_cut_distinguishes_leaves_and_unknown_counts() {
        let mut c = Completeness::new(true);
        c.depth_cut(true, 0, CountEvidence::Known(0));
        assert_eq!(output(&c)["observation_complete"], true);
        c.depth_cut(true, 1, CountEvidence::Known(3));
        c.depth_cut(true, 2, CountEvidence::Unknown);
        assert!(has(&c, "depth_limit"));
        assert_eq!(c.boundaries[&1].children_skipped, CountEvidence::Known(3));
        assert_eq!(c.boundaries[&2].children_skipped, CountEvidence::Unknown);
    }

    #[test]
    fn retained_attribute_failures_deduplicate_and_keep_sibling_evidence() {
        let mut c = Completeness::new(true);
        // Role name, numeric role and state each failed on retained nodes.
        for _ in 0..3 {
            c.record(true, Event::AttributeFailed, None);
        }
        // A sibling's failed read is outside this window's scope.
        c.record(
            false,
            Event::ChildrenFailed,
            Some((4, CountEvidence::Known(2))),
        );
        assert_eq!(c.reasons.len(), 1);
        assert!(has(&c, "attribute_read_failed"));
        assert!(c.boundaries.is_empty());
    }

    #[test]
    fn timeout_and_untrusted_fallback_remain_distinct_evidence() {
        let mut c = Completeness::new(false);
        c.record(true, Event::TimeBudget, None);
        c.record(true, Event::UntrustedProvider, None);
        assert!(has(&c, "time_budget"));
        assert!(has(&c, "untrusted_provider"));
        assert!(has(&c, "not_asserted"));
    }

    #[test]
    fn fake_reader_preserves_siblings_and_limits_boundaries_to_indexed_nodes() {
        let records = [
            Record {
                in_scope: true,
                indexed: false,
                position: 0,
                event: Some(Event::ChildrenFailed),
                direct_children: Some(CountEvidence::Known(3)),
            },
            Record {
                in_scope: true,
                indexed: true,
                position: 1,
                event: Some(Event::DepthLimit),
                direct_children: Some(CountEvidence::Known(2)),
            },
            Record {
                in_scope: false,
                indexed: true,
                position: 2,
                event: Some(Event::ChildrenFailed),
                direct_children: Some(CountEvidence::Unknown),
            },
            Record {
                in_scope: true,
                indexed: true,
                position: 3,
                event: Some(Event::AttributeFailed),
                direct_children: None,
            },
        ];
        let (c, indexed) = fake_walk(&records, true);
        assert!(has(&c, "child_enumeration_failed"));
        assert!(has(&c, "depth_limit"));
        assert!(has(&c, "attribute_read_failed"));
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[&1].children_skipped, CountEvidence::Known(2));
        assert!(!indexed.contains_key(&0));
        assert!(!indexed.contains_key(&2));
        assert!(!indexed.contains_key(&3));
    }

    #[test]
    fn fake_reader_clean_presentations_and_optional_interfaces_are_complete() {
        // Clean containers, absent optional interfaces, a successful child-count
        // fallback, tooltip exclusion, text capping and tab compaction emit no
        // omitted-scope event.
        let records: Vec<_> = (0..6)
            .map(|position| Record {
                in_scope: true,
                indexed: position % 2 == 0,
                position,
                event: None,
                direct_children: None,
            })
            .collect();
        let (c, indexed) = fake_walk(&records, true);
        assert_eq!(output(&c)["observation_complete"], true);
        assert_eq!(output(&c)["observation_incomplete_reasons"], json!([]));
        assert!(indexed.is_empty());
    }

    #[test]
    fn fake_reader_app_reference_timeout_menu_and_scope_mismatch_are_reported() {
        for (event, detail, exhaustive) in [
            (Event::ChildrenFailed, "child_enumeration_failed", true), // dropped app reference or timed-out GetChildren
            (Event::CollapsedMenu, "collapsed_menu", true),
            (Event::HugeContainer, "huge_container", true),
            (Event::ScopeMismatch, "scope_mismatch", false),
        ] {
            let (c, _) = fake_walk(
                &[Record {
                    in_scope: true,
                    indexed: false,
                    position: 0,
                    event: Some(event),
                    direct_children: None,
                }],
                exhaustive,
            );
            assert!(has(&c, detail));
            assert_eq!(output(&c)["observation_complete"], false);
        }
    }
}
