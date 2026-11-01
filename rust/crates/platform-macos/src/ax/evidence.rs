//! Internal, value-free observations retained alongside legacy projections.
//!
//! These types deliberately have no serialization or public node integration.
//! Sites and statuses are populated by the subsequent evidence-capture step.
#![allow(dead_code)]

use super::content_attrs::AttrStatus;
use super::content_attrs::AttributeListing;
use super::tree_source::{Read, ReadFailure};
use std::cell::RefCell;

thread_local! {
    static WALK_EVIDENCE: RefCell<Option<Vec<ReadEvidence>>> = const { RefCell::new(None) };
    static WALK_OWNER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// A walk owns its evidence until it returns. Nested walks temporarily replace
/// the collector and restore it even if they unwind.
pub(crate) struct EvidenceScope {
    previous: Option<Vec<ReadEvidence>>,
    previous_owner: Option<usize>,
}

impl EvidenceScope {
    pub(crate) fn start() -> Self {
        let previous = WALK_EVIDENCE.with(|cell| cell.replace(Some(Vec::new())));
        let previous_owner = WALK_OWNER.with(|cell| cell.replace(None));
        Self {
            previous,
            previous_owner,
        }
    }

    pub(crate) fn finish<T>(self, legacy: T) -> Observed<T> {
        let evidence = WALK_EVIDENCE.with(|cell| {
            cell.borrow_mut()
                .take()
                .expect("walk evidence scope must remain active")
        });
        Observed { legacy, evidence }
    }
}

impl Drop for EvidenceScope {
    fn drop(&mut self) {
        WALK_EVIDENCE.with(|cell| *cell.borrow_mut() = self.previous.take());
        WALK_OWNER.with(|cell| cell.set(self.previous_owner));
    }
}

pub(crate) fn current_owner() -> Option<usize> {
    WALK_OWNER.with(std::cell::Cell::get)
}

pub(crate) struct EvidenceOwner(Option<usize>);

impl EvidenceOwner {
    pub(crate) fn start(owner: usize) -> Self {
        Self(WALK_OWNER.with(|cell| cell.replace(Some(owner))))
    }
}

impl Drop for EvidenceOwner {
    fn drop(&mut self) {
        WALK_OWNER.with(|cell| cell.set(self.0));
    }
}

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

    /// Keep this observation in the active walk before returning its unchanged
    /// legacy projection. Outside a walk there is no enclosing collector.
    pub(crate) fn into_walk_legacy(self) -> T {
        WALK_EVIDENCE.with(|cell| {
            if let Some(active) = cell.borrow_mut().as_mut() {
                active.extend(self.evidence);
            }
        });
        self.legacy
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
    ProviderEnablement,
    ProviderCachedEnablement,
    ProviderOptIn,
    ScopeCandidate(&'static str),
    RecoveryHit,
    RecoveryDeadline,
    RecoveryParent,
    RecoveryWindowId,
    RecoveryRoot,
    TopLevelWindows,
    TopLevelWindowsMember,
    TopLevelRemoteWindow,
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

pub(crate) fn observe_read<T>(
    owner: Option<usize>,
    site: ReadSite,
    legacy: Read<T>,
) -> Observed<Read<T>> {
    let status = read_status(&legacy);
    Observed {
        legacy,
        evidence: vec![ReadEvidence {
            owner,
            site,
            status,
            attempt: 0,
            listing: None,
        }],
    }
}

pub(crate) fn record_status(owner: Option<usize>, site: ReadSite, status: EvidenceStatus) {
    Observed {
        legacy: (),
        evidence: vec![ReadEvidence {
            owner,
            site,
            status,
            attempt: 0,
            listing: None,
        }],
    }
    .into_walk_legacy();
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

        let outer = EvidenceScope::start();
        Observed {
            legacy: (),
            evidence: vec![first],
        }
        .into_walk_legacy();
        let unwound = std::panic::catch_unwind(|| {
            let _inner = EvidenceScope::start();
            Observed {
                legacy: (),
                evidence: vec![replay],
            }
            .into_walk_legacy();
            panic!("nested walk unwinds");
        });
        assert!(unwound.is_err());
        Observed {
            legacy: (),
            evidence: vec![first],
        }
        .into_walk_legacy();
        assert_eq!(outer.finish(()).evidence, [first, first]);
    }
}
