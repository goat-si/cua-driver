//! Internal, value-free observations retained alongside legacy projections.
//!
//! These types deliberately have no serialization or public node integration.
//! Sites and statuses are populated by the subsequent evidence-capture step.
#![allow(dead_code)]

use super::content_attrs::AttrStatus;
use super::content_attrs::AttributeListing;
use super::tree_source::{Read, ReadFailure};

pub(crate) struct Observed<T> {
    pub(crate) legacy: T,
    pub(crate) evidence: Vec<ReadEvidence>,
}

impl<T> Observed<T> {
    /// Establish the existing projection before adding evidence capture.
    pub(crate) fn legacy_only(legacy: T) -> Self {
        Self {
            legacy,
            evidence: Vec::new(),
        }
    }
}

/// Attribute names used here must be static AX names, never values from a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadSite {
    Role,
    ActionNames,
    Content(&'static str),
    Control(&'static str),
    Frame(&'static str),
    Scrollbar(&'static str),
    Tab(&'static str),
    Dense(&'static str),
    Children,
    ElementSet(&'static str),
    ValueSettable,
    SelectedSettable,
    MenuSelected,
    Batch(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EvidenceStatus {
    Known,
    Absent,
    Unknown(ReadFailure),
    Unavailable,
    DeadlineExpired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadEvidence {
    pub(crate) owner: Option<usize>,
    pub(crate) site: ReadSite,
    pub(crate) status: EvidenceStatus,
    /// Identifies a batch attempt or its later single-read replay.
    pub(crate) attempt: u32,
    /// Classification metadata from an existing text-status read, if any.
    pub(crate) listing: Option<AttributeListing>,
}

pub(crate) fn read_status<T>(read: &Read<T>) -> EvidenceStatus {
    match read {
        Read::Known(_) => EvidenceStatus::Known,
        Read::Absent => EvidenceStatus::Absent,
        Read::Unknown(failure) => EvidenceStatus::Unknown(*failure),
    }
}

pub(crate) fn attr_status(status: &AttrStatus) -> (EvidenceStatus, Option<AttributeListing>) {
    match status {
        AttrStatus::Known(_) => (EvidenceStatus::Known, None),
        AttrStatus::Absent => (EvidenceStatus::Absent, None),
        AttrStatus::Failed { error, listed } => (
            EvidenceStatus::Unknown(ReadFailure::Ax(*error)),
            Some(*listed),
        ),
        AttrStatus::WrongType => (EvidenceStatus::Unknown(ReadFailure::WrongType), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::bindings::kAXErrorCannotComplete;

    #[test]
    fn evidence_keeps_error_listing_and_attempt_without_the_value() {
        let first = ReadEvidence {
            owner: Some(7),
            site: ReadSite::Content("AXDescription"),
            status: EvidenceStatus::Unknown(ReadFailure::Ax(kAXErrorCannotComplete)),
            attempt: 0,
            listing: Some(AttributeListing::ListUnreadable),
        };
        let replay = ReadEvidence {
            status: EvidenceStatus::Known,
            attempt: 1,
            ..first
        };
        let observed = Observed {
            legacy: 42,
            evidence: vec![first, replay],
        };
        assert_eq!(observed.legacy, 42);
        assert_eq!(observed.evidence[0], first);
        assert_ne!(observed.evidence[0].attempt, observed.evidence[1].attempt);
        assert!(
            matches!(observed.evidence[0].status, EvidenceStatus::Unknown(ReadFailure::Ax(error)) if error == kAXErrorCannotComplete)
        );
    }
}
