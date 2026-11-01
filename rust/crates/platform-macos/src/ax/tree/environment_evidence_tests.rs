use crate::ax::bindings::AccessibilityOptIn;
use crate::ax::enablement::{ensure_with, ProcessEnablement, Wait};
use crate::ax::evidence::{EvidenceStatus, ReadSite};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

#[test]
fn top_level_enumeration_preserves_unknown_and_empty() {
    use super::walker_seam_tests::{step, Fake, Reply};
    use crate::ax::bindings::{
        kAXErrorCannotComplete, kAXErrorNoValue, kAXErrorSuccess, window_list_status,
    };
    use crate::ax::evidence::{EvidenceScope, Observed, ReadEvidence};
    use crate::ax::tree_source::{AxTreeSource, Read, ReadFailure};

    assert_eq!(
        window_list_status(kAXErrorCannotComplete, false, false),
        EvidenceStatus::Unknown(ReadFailure::Ax(kAXErrorCannotComplete))
    );
    assert_eq!(
        window_list_status(kAXErrorNoValue, false, false),
        EvidenceStatus::Absent
    );
    assert_eq!(
        window_list_status(kAXErrorSuccess, false, false),
        EvidenceStatus::Absent
    );
    assert_eq!(
        window_list_status(kAXErrorSuccess, true, false),
        EvidenceStatus::Unknown(ReadFailure::WrongType)
    );
    assert_eq!(
        window_list_status(kAXErrorSuccess, true, true),
        EvidenceStatus::Known
    );

    for child_failure in [false, true] {
        let child_reply = if child_failure {
            Read::Unknown(ReadFailure::Ax(kAXErrorCannotComplete))
        } else {
            Read::Known(vec![])
        };
        let fake = Fake::new(
            &[(1, 1, 1, &[])],
            vec![step("children 1", Reply::Children(child_reply))],
        );
        let root = fake.handle(1);
        let scope = EvidenceScope::start();
        let ledger = super::SkipLedger::start();
        let children = fake.observed_children(&root);
        let union = super::top_level_union_with(
            children,
            || {
                assert_eq!(
                    super::WALK_SKIPPED.with(std::cell::Cell::get),
                    child_failure
                );
                Observed {
                    legacy: Vec::new(),
                    evidence: vec![ReadEvidence {
                        owner: Some(1),
                        site: ReadSite::TopLevelWindows,
                        status: EvidenceStatus::Known,
                        attempt: 0,
                        listing: None,
                    }],
                }
            },
            |_, _| panic!("no windows"),
            |_| panic!("no duplicate"),
        );
        assert!(union.is_empty());
        assert_eq!(ledger.finish(), child_failure);
        let observed = scope.finish(union);
        assert!(observed
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::Children
                && entry.owner == Some(1)
                && entry.status
                    == if child_failure {
                        EvidenceStatus::Unknown(ReadFailure::Ax(kAXErrorCannotComplete))
                    } else {
                        EvidenceStatus::Known
                    }));
        assert!(observed
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::TopLevelWindows
                && entry.status == EvidenceStatus::Known));
        fake.assert_consumed();
        drop(root);
        fake.assert_lifetimes(&[]);
    }

    for window_failure in [false, true] {
        let fake = Fake::new(
            &[(1, 1, 1, &[])],
            vec![step("children 1", Reply::Children(Read::Absent))],
        );
        let root = fake.handle(1);
        let scope = EvidenceScope::start();
        let ledger = super::SkipLedger::start();
        let children = fake.observed_children(&root);
        let status = if window_failure {
            EvidenceStatus::Unknown(ReadFailure::Ax(kAXErrorCannotComplete))
        } else {
            EvidenceStatus::Known
        };
        let union = super::top_level_union_with(
            children,
            || Observed {
                legacy: Vec::new(),
                evidence: vec![ReadEvidence {
                    owner: Some(1),
                    site: ReadSite::TopLevelWindows,
                    status,
                    attempt: 0,
                    listing: None,
                }],
            },
            |_, _| panic!("no windows"),
            |_| panic!("no duplicate"),
        );
        assert!(union.is_empty());
        assert!(!ledger.finish());
        let observed = scope.finish(union);
        assert!(observed
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::TopLevelWindows
                && entry.owner == Some(1)
                && entry.status == status));
        fake.assert_consumed();
        drop(root);
        fake.assert_lifetimes(&[]);
    }

    let fake = Fake::new(
        &[(1, 1, 1, &[2]), (2, 2, 2, &[]), (3, 3, 3, &[])],
        vec![step("children 1", Reply::Children(Read::Known(vec![2])))],
    );
    let root = fake.handle(1);
    let children = fake.observed_children(&root);
    let duplicate = fake.handle(2);
    let extra = fake.handle(3);
    let windows = || Observed::legacy_only(vec![duplicate, extra]);
    let union = super::top_level_union_with(
        children,
        windows,
        |left, right| fake.key(left) == fake.key(right),
        drop,
    );
    assert_eq!(
        union.iter().map(|item| fake.key(item)).collect::<Vec<_>>(),
        [2, 3]
    );
    fake.assert_consumed();
    drop(union);
    drop(root);
    fake.assert_lifetimes(&[]);
}

#[test]
fn recovery_probe_preserves_failure_and_no_match() {
    use crate::ax::bindings::{kAXErrorCannotComplete, kAXErrorSuccess, recovery_hit_outcome};
    use crate::ax::tree_source::ReadFailure;
    let failed = recovery_hit_outcome::<usize>(kAXErrorCannotComplete, None, None);
    assert!(failed.legacy.is_none());
    assert!(
        failed
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::RecoveryHit
                && entry.status
                    == EvidenceStatus::Unknown(ReadFailure::Ax(kAXErrorCannotComplete))),
        "missing failed hit evidence"
    );
    let absent = recovery_hit_outcome::<usize>(kAXErrorSuccess, None, None);
    assert!(absent.legacy.is_none());
    assert!(
        absent
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::RecoveryHit
                && entry.status == EvidenceStatus::Absent)
    );
    let found = recovery_hit_outcome(kAXErrorSuccess, Some(42usize), Some(42));
    assert_eq!(found.legacy, Some(42));
    assert!(found
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::RecoveryHit && entry.status == EvidenceStatus::Known));
    let now = Instant::now();
    let scope = crate::ax::evidence::EvidenceScope::start();
    let skipped: Option<()> =
        super::window_center_hit_if_live(Some(now), now, || panic!("no hit after deadline"));
    let observed = scope.finish(skipped);
    assert!(observed.legacy.is_none());
    assert!(observed
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::RecoveryDeadline
            && entry.status == EvidenceStatus::DeadlineExpired));
    for (ids, expected, status) in [
        (vec![Some(1), Some(2)], None, EvidenceStatus::Absent),
        (vec![Some(1), None], None, EvidenceStatus::Unavailable),
        (
            vec![Some(1), Some(42), Some(42)],
            Some(2),
            EvidenceStatus::Known,
        ),
    ] {
        let result = super::recovery_root_outcome(&ids, 42);
        assert_eq!(result.legacy, expected);
        assert!(result
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::RecoveryRoot && entry.status == status));
    }
}

#[test]
fn scope_candidate_reads_preserve_unknown_and_absent() {
    use super::scope_candidate_with;
    use super::walker_seam_tests::{step, Fake, Reply};
    use crate::ax::tree_source::{Read, ReadFailure};
    use crate::ax::window_scope::{decide_window_scope, WindowScope};
    use crate::windows::WindowOwner;

    for name in ["AXRole", "AXSubrole", "AXIdentifier"] {
        for case in 0..4 {
            let make_read = |target: &str, known: &str| {
                if target != name {
                    return Read::Known(known.to_owned());
                }
                match case {
                    0 => {
                        Read::Unknown(ReadFailure::Ax(crate::ax::bindings::kAXErrorCannotComplete))
                    }
                    1 => Read::Known(known.to_owned()),
                    2 => Read::Absent,
                    _ => Read::Unknown(ReadFailure::WrongType),
                }
            };
            let fake = Fake::new(
                &[(1, 1, 1, &[])],
                vec![
                    step("timeout 1", Reply::Unit),
                    step(
                        "text 1 AXRole",
                        Reply::Text(make_read("AXRole", "AXWindow")),
                    ),
                    step(
                        "text 1 AXSubrole",
                        Reply::Text(make_read("AXSubrole", "ordinary")),
                    ),
                    step(
                        "text 1 AXIdentifier",
                        Reply::Text(make_read("AXIdentifier", "ordinary")),
                    ),
                ],
            );
            let element = fake.handle(1);
            let id_calls = Cell::new(0);
            let observed = scope_candidate_with(&fake, &element, || {
                id_calls.set(id_calls.get() + 1);
                Some(42)
            });
            let role_present = name != "AXRole" || case == 1;
            assert_eq!(id_calls.get(), usize::from(role_present));
            let decision = decide_window_scope(&[observed.legacy], 42, || WindowOwner::SamePid);
            if role_present {
                assert_eq!(decision.scope, WindowScope::Matched);
            } else {
                assert!(decision.walk.is_empty());
            }
            fake.assert_consumed();
            drop(element);
            fake.assert_lifetimes(&[]);
            let status = match case {
                0 => EvidenceStatus::Unknown(ReadFailure::Ax(
                    crate::ax::bindings::kAXErrorCannotComplete,
                )),
                1 => EvidenceStatus::Known,
                2 => EvidenceStatus::Absent,
                _ => EvidenceStatus::Unknown(ReadFailure::WrongType),
            };
            assert!(
                observed.evidence.iter().any(|entry| entry.owner == Some(1)
                    && entry.site == ReadSite::ScopeCandidate(name)
                    && entry.status == status),
                "missing candidate evidence for {name}"
            );
        }
    }
    let fake = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
            step("text 1 AXSubrole", Reply::Text(Read::Absent)),
            step("text 1 AXIdentifier", Reply::Text(Read::Absent)),
        ],
    );
    let element = fake.handle(1);
    let observed = scope_candidate_with(&fake, &element, || None);
    assert_eq!(observed.legacy.ax_window_id, None);
    fake.assert_consumed();
    drop(element);
    fake.assert_lifetimes(&[]);
    assert!(observed.evidence.iter().any(|entry| entry.owner == Some(1)
        && entry.site == ReadSite::ScopeCandidate("AXWindowId")
        && entry.status == EvidenceStatus::Unavailable));
}

#[test]
fn provider_enablement_preserves_failed_and_cached_outcomes() {
    let stamp = (10, 20);
    let now = Instant::now();
    let stored = Cell::new(None);
    let trace = RefCell::new(Vec::new());
    let failed = ensure_with(
        None,
        Some(stamp),
        now,
        || {
            trace.borrow_mut().push("opt-in");
            AccessibilityOptIn::ManualAccessibility
        },
        || {
            trace.borrow_mut().push("wait");
            false
        },
        |stamp, wait| {
            trace.borrow_mut().push("cache-write");
            stored.set(Some(ProcessEnablement { stamp, wait }));
        },
        Some(7),
    );
    assert_eq!(*trace.borrow(), ["opt-in", "wait", "cache-write"]);
    assert!(matches!(
        stored.get().expect("timeout cached").wait,
        Wait::TimedOut { attempts: 1, .. }
    ));
    let cached = ensure_with(
        stored.get(),
        Some(stamp),
        now + Duration::from_secs(1),
        || panic!("cached failure must not re-enable"),
        || panic!("cached failure must not wait"),
        |_, _| panic!("cached failure must not write"),
        Some(7),
    );
    assert!(
        failed.evidence.iter().any(|entry| entry.owner == Some(7)
            && entry.site == ReadSite::ProviderEnablement
            && entry.status == EvidenceStatus::Unavailable
            && entry.attempt == 1),
        "missing failed materialization evidence"
    );
    assert!(
        cached.evidence.iter().any(|entry| entry.owner == Some(7)
            && entry.site == ReadSite::ProviderCachedEnablement
            && entry.status == EvidenceStatus::Unavailable
            && entry.attempt == 1),
        "missing cached failure evidence"
    );

    let ready_cache = Cell::new(None);
    let ready = ensure_with(
        None,
        Some(stamp),
        now,
        || AccessibilityOptIn::EnhancedUserInterface,
        || true,
        |stamp, wait| ready_cache.set(Some(ProcessEnablement { stamp, wait })),
        Some(7),
    );
    assert!(ready
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::ProviderEnablement
            && entry.status == EvidenceStatus::Known
            && entry.owner == Some(7)));
    let cached_ready = ensure_with(
        ready_cache.get(),
        Some(stamp),
        now + Duration::from_secs(1),
        || panic!("cached ready must not re-enable"),
        || panic!("cached ready must not wait"),
        |_, _| panic!("cached ready must not write"),
        Some(7),
    );
    assert!(cached_ready
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::ProviderCachedEnablement
            && entry.status == EvidenceStatus::Known
            && entry.owner == Some(7)));
    let rejected = ensure_with(
        None,
        Some(stamp),
        now,
        || AccessibilityOptIn::NotAccepted,
        || panic!("rejected opt-in must not wait"),
        |_, _| panic!("rejected opt-in must not cache"),
        Some(7),
    );
    assert!(rejected
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::ProviderOptIn
            && entry.status == EvidenceStatus::Unavailable
            && entry.owner == Some(7)));
}
