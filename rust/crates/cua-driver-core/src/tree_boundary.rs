//! Boundary metadata follows retained nodes, not transient row offsets.

pub use cua_driver_contract::{BoundaryReason, CountEvidence, NodeBoundary};

/// Add a direct-child count without wrapping. Unknown evidence remains unknown.
pub fn add_direct_children(boundary: &mut NodeBoundary, count: u32) {
    if let CountEvidence::Known(existing) = &mut boundary.children_skipped {
        *existing = existing.saturating_add(count);
    }
}

/// Carry a dropped node's boundary to its nearest retained ancestor, or to
/// the read-level reasons when even the root was dropped. Direct-child counts
/// are never combined across levels of the tree.
pub fn retain_boundary(
    dropped: &NodeBoundary,
    ancestor: Option<&mut Option<NodeBoundary>>,
    read_reasons: &mut Vec<BoundaryReason>,
) {
    let reasons = if let Some(slot) = ancestor {
        let target = slot.get_or_insert_with(|| NodeBoundary {
            reasons: Vec::new(),
            children_skipped: CountEvidence::Unknown,
            descendants_omitted: None,
        });
        target.descendants_omitted = Some(true);
        &mut target.reasons
    } else {
        read_reasons
    };
    reasons.extend(dropped.reasons.iter().copied());
    reasons.sort_unstable();
    reasons.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_contract::{IncompleteReasonDetail as Detail, IncompleteReasonKind as Kind};
    fn reason(detail: Detail) -> BoundaryReason {
        BoundaryReason {
            kind: Kind::Budget,
            detail,
        }
    }
    fn boundary(detail: Detail, count: CountEvidence) -> NodeBoundary {
        NodeBoundary {
            reasons: vec![reason(detail)],
            children_skipped: count,
            descendants_omitted: None,
        }
    }
    #[test]
    fn nested_cuts_keep_direct_counts_and_all_unique_reasons() {
        let child = boundary(Detail::TimeBudget, CountEvidence::Known(10));
        let mut ancestor = Some(boundary(Detail::NodeBudget, CountEvidence::Known(2)));
        let mut read = vec![];
        retain_boundary(&child, Some(&mut ancestor), &mut read);
        retain_boundary(&child, Some(&mut ancestor), &mut read);
        let ancestor = ancestor.unwrap();
        assert_eq!(ancestor.children_skipped, CountEvidence::Known(2));
        assert_eq!(ancestor.descendants_omitted, Some(true));
        assert_eq!(ancestor.reasons.len(), 2);
        let mut absent = None;
        retain_boundary(&ancestor, Some(&mut absent), &mut read);
        assert_eq!(absent.unwrap().children_skipped, CountEvidence::Unknown);
        assert!(read.is_empty());
    }
    #[test]
    fn count_saturates_and_dropped_root_becomes_read_reasons() {
        let mut root = boundary(Detail::NodeBudget, CountEvidence::Known(u32::MAX - 1));
        add_direct_children(&mut root, 10);
        assert_eq!(root.children_skipped, CountEvidence::Known(u32::MAX));
        let mut reasons = vec![];
        retain_boundary(&root, None, &mut reasons);
        retain_boundary(&root, None, &mut reasons);
        assert_eq!(reasons, root.reasons);
    }
}
