use crate::ax::bindings::AccessibilityOptIn;
use crate::ax::enablement::{ensure_with, ProcessEnablement, Wait};
use crate::ax::evidence::{EvidenceStatus, ReadSite};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

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
