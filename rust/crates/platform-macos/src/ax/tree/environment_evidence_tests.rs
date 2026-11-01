use crate::ax::bindings::AccessibilityOptIn;
use crate::ax::enablement::{ensure_with, ProcessEnablement, Wait};
use crate::ax::evidence::{EvidenceStatus, ReadSite};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

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
