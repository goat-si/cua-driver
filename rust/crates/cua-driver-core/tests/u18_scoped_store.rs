use cua_driver_core::browser::types::ProcessFingerprint;
use cua_driver_core::element_token::{token_for, ResolvedElement};
use cua_driver_core::snapshot_store::{
    InstanceCapture, InstanceEvidence, ScopedWalk, SnapshotPayload, SnapshotStore,
    ValidatedScopedCapture,
};
use serde_json::json;

struct Payload(Vec<usize>);
impl SnapshotPayload for Payload {
    type Element = usize;
    fn len(&self) -> usize {
        self.0.len()
    }
    fn retain(&self, index: usize) -> Option<Self::Element> {
        self.0.get(index).copied()
    }
    fn structural_path(&self, index: usize) -> Option<Vec<usize>> {
        self.0.get(index).map(|_| vec![index])
    }
}

fn fingerprint(start: u64) -> ProcessFingerprint {
    ProcessFingerprint {
        pid: 42,
        start_time: Some(start),
        executable: Some("/app".into()),
    }
}

fn full(store: &SnapshotStore<Payload>) -> u32 {
    store
        .publish_for_session_with_instance(
            42,
            7,
            Payload(vec![10, 11]),
            Some("owner"),
            Some(2.0),
            InstanceCapture::Captured(fingerprint(1)),
        )
        .unwrap()
        .0
}

fn root(
    store: &SnapshotStore<Payload>,
    token: &str,
) -> cua_driver_core::snapshot_store::ValidatedScopedCapture<Payload> {
    store
        .begin_scoped_capture(
            42,
            7,
            token,
            Some("owner"),
            InstanceEvidence::Live(fingerprint(1)),
        )
        .unwrap()
        .validate_root(|element, _, _, _| Ok(*element))
        .unwrap()
}

fn commit(
    store: &SnapshotStore<Payload>,
    capture: ValidatedScopedCapture<Payload>,
    payload: Payload,
) -> Result<u32, cua_driver_core::protocol::ToolResult> {
    store.commit_scoped_capture(capture, ScopedWalk::Complete(payload))
}

#[test]
fn scoped_snapshot_does_not_replace_full_and_is_retired_with_parent() {
    let store = SnapshotStore::new();
    let parent = full(&store);
    let zoom = cua_driver_core::snapshot_store::ZoomContext {
        screenshot: store
            .screenshot_context(42, Some(7), Some("owner"))
            .unwrap(),
        origin_x: 1.0,
        origin_y: 2.0,
        scale_inv: 0.5,
    };
    store.set_zoom(42, Some("owner"), zoom).unwrap();
    let capture = root(&store, &token_for(parent, 1));
    let scoped = commit(&store, capture, Payload(vec![77])).unwrap();
    assert_eq!(store.with_latest(42, 7, |id, _| id), Some(parent));
    assert_eq!(store.zoom(42, Some(7), Some("owner")).unwrap(), zoom);
    assert_eq!(
        store
            .screenshot_context(42, Some(7), Some("owner"))
            .unwrap()
            .snapshot_id,
        parent
    );
    let args = json!({"element_token":token_for(scoped, 0), "_session_id":"owner"});
    assert_eq!(store.pid_for_token(&args), Some(42));
    assert_eq!(store.window_for_snapshot(42, scoped), Some(7));
    assert!(store.resolve(42, &args).is_err());
    let foreign = json!({"element_token":token_for(scoped, 0), "_session_id":"other"});
    assert_eq!(store.pid_for_token(&foreign), None);
    assert!(store
        .resolve_for_instance(42, &foreign, InstanceEvidence::Live(fingerprint(1)))
        .is_err());
    assert!(matches!(
        store
            .resolve_for_instance(42, &args, InstanceEvidence::Live(fingerprint(1)))
            .unwrap(),
        ResolvedElement::Element { element: 77, .. }
    ));
    let bare = json!({"element_token":"0", "window_id":7, "_session_id":"owner"});
    assert!(matches!(
        store
            .resolve_for_instance(42, &bare, InstanceEvidence::Live(fingerprint(1)))
            .unwrap(),
        ResolvedElement::Element { element: 10, .. }
    ));
    full(&store);
    assert!(store
        .resolve_for_instance(42, &args, InstanceEvidence::Live(fingerprint(1)))
        .is_err());
}

#[test]
fn scoped_commit_allows_screenshot_refresh_but_refuses_replaced_parent() {
    let store = SnapshotStore::new();
    let parent = full(&store);
    let first = root(&store, &token_for(parent, 0));
    store
        .refresh_screenshot_for_session(42, 7, Some("owner"), 3.0, None)
        .unwrap();
    commit(&store, first, Payload(vec![1])).unwrap();
    let second = root(&store, &token_for(parent, 0));
    full(&store);
    let failure = commit(&store, second, Payload(vec![2])).unwrap_err();
    assert_eq!(
        failure.structured_content.unwrap()["refusal"]["code"],
        "stale_parent"
    );
}

#[test]
fn rooted_capture_requires_snapshot_ownership_and_live_process() {
    let store = SnapshotStore::new();
    let unprotected = store.publish(42, 7, Payload(vec![1]));
    let err = store
        .begin_scoped_capture(
            42,
            7,
            &token_for(unprotected, 0),
            None,
            InstanceEvidence::Live(fingerprint(1)),
        )
        .unwrap_err();
    assert_eq!(
        err.structured_content.unwrap()["refusal"]["code"],
        "root_requires_protected_snapshot"
    );
    let parent = full(&store);
    for (token, session, evidence) in [
        (
            "0".to_owned(),
            Some("owner"),
            InstanceEvidence::Live(fingerprint(1)),
        ),
        (
            token_for(parent, 0),
            Some("other"),
            InstanceEvidence::Live(fingerprint(1)),
        ),
        (
            token_for(parent, 0),
            Some("owner"),
            InstanceEvidence::Live(fingerprint(2)),
        ),
        (
            token_for(parent, 0),
            Some("owner"),
            InstanceEvidence::NotChecked,
        ),
    ] {
        assert!(store
            .begin_scoped_capture(42, 7, &token, session, evidence)
            .is_err());
    }
}

#[test]
fn scoped_commit_and_cancellation_do_not_supersede_full_capture_ticket() {
    let store = SnapshotStore::new();
    let parent = full(&store);
    let ticket = store.begin_capture(42, 7);
    drop(root(&store, &token_for(parent, 0)));
    let scope = root(&store, &token_for(parent, 1));
    commit(&store, scope, Payload(vec![2])).unwrap();
    let published = store.publish_basis_for_session_with_instance(
        42,
        7,
        Payload(vec![3]),
        Some("owner"),
        None,
        None,
        &ticket,
        InstanceCapture::Captured(fingerprint(1)),
    );
    assert!(published.is_some());
}

#[test]
fn scoped_capacity_evicts_oldest_without_affecting_full_or_other_scopes() {
    let store = SnapshotStore::new();
    let parent = full(&store);
    let mut scopes = Vec::new();
    for i in 0..=cua_driver_core::snapshot_store::SCOPED_CAP_PER_PARENT {
        let capture = root(&store, &token_for(parent, 0));
        scopes.push(commit(&store, capture, Payload(vec![i])).unwrap());
    }
    let resolve = |id| {
        store.resolve_for_instance(
            42,
            &json!({
                "element_token":token_for(id, 0), "_session_id":"owner"
            }),
            InstanceEvidence::Live(fingerprint(1)),
        )
    };
    assert!(resolve(scopes[0]).is_err());
    assert!(resolve(scopes[1]).is_ok());
    assert!(resolve(parent).is_ok());
    assert_eq!(store.with_latest(42, 7, |id, _| id), Some(parent));
}

#[test]
fn root_revalidation_is_outside_store_lock_and_preserves_nested_prefix() {
    let store = SnapshotStore::new();
    let parent = full(&store);
    let capture = store
        .begin_scoped_capture(
            42,
            7,
            &token_for(parent, 1),
            Some("owner"),
            InstanceEvidence::Live(fingerprint(1)),
        )
        .unwrap();
    let capture = capture
        .validate_root(|element, path, window, process| {
            assert_eq!(
                (*element, path, window, process.start_time),
                (11, &[1][..], 7, Some(1))
            );
            assert_eq!(store.with_latest(42, 7, |id, _| id), Some(parent));
            Ok(*element)
        })
        .unwrap();
    let scope = commit(&store, capture, Payload(vec![77])).unwrap();
    let nested = store
        .begin_scoped_capture(
            42,
            7,
            &token_for(scope, 0),
            Some("owner"),
            InstanceEvidence::Live(fingerprint(1)),
        )
        .unwrap();
    let nested = nested
        .validate_root(|element, path, window, _| {
            assert_eq!((*element, path, window), (77, &[1, 0][..], 7));
            Ok(*element)
        })
        .unwrap();
    assert_eq!(nested.parent_snapshot_id(), parent);
    let nested_id = commit(&store, nested, Payload(vec![78])).unwrap();
    assert_ne!(nested_id, scope);
    assert_eq!(store.with_latest(42, 7, |id, _| id), Some(parent));
}

#[test]
fn a_root_that_moved_vanished_or_changed_window_is_not_published() {
    let store = SnapshotStore::new();
    let parent = full(&store);
    for reason in ["moved", "vanished", "other_window"] {
        let candidate = store
            .begin_scoped_capture(
                42,
                7,
                &token_for(parent, 0),
                Some("owner"),
                InstanceEvidence::Live(fingerprint(1)),
            )
            .unwrap();
        let error = candidate
            .validate_root(|_, _, _, _| Err(cua_driver_core::protocol::ToolResult::error(reason)))
            .unwrap_err();
        assert_eq!(error.is_error, Some(true), "{reason}");
        assert_eq!(store.with_latest(42, 7, |id, _| id), Some(parent));
    }
}

#[test]
fn all_full_retirement_paths_invalidate_derived_scope() {
    for action in ["remove", "remove_for_capture", "clear", "session", "lru"] {
        let store = SnapshotStore::new();
        let parent = full(&store);
        let scope = commit(
            &store,
            root(&store, &token_for(parent, 0)),
            Payload(vec![7]),
        )
        .unwrap();
        match action {
            "remove" => {
                store.remove(42, 7);
            }
            "remove_for_capture" => {
                let ticket = store.begin_capture(42, 7);
                store.remove_for_capture(42, 7, &ticket);
            }
            "clear" => {
                store.clear();
            }
            "session" => {
                store.retire_session_screenshots("owner");
            }
            "lru" => {
                for window in 100..108 {
                    store.publish(42, window, Payload(vec![1]));
                }
            }
            _ => unreachable!(),
        }
        let args = json!({"element_token":token_for(scope, 0), "_session_id":"owner"});
        let error = store
            .resolve_for_instance(42, &args, InstanceEvidence::Live(fingerprint(1)))
            .unwrap_err();
        assert_eq!(
            error.structured_content.unwrap()["refusal"]["code"],
            "stale_element_token",
            "{action}"
        );
    }
}

#[test]
fn incomplete_walk_is_diagnostics_only_and_never_publishes() {
    use cua_driver_contract::{IncompleteReason, IncompleteReasonDetail, IncompleteReasonKind};
    let store = SnapshotStore::new();
    let parent = full(&store);
    let result = store
        .commit_scoped_capture(
            root(&store, &token_for(parent, 0)),
            ScopedWalk::Incomplete(vec![IncompleteReason {
                kind: IncompleteReasonKind::Budget,
                detail: IncompleteReasonDetail::NodeBudget,
            }]),
        )
        .unwrap_err();
    let data = result.structured_content.unwrap();
    assert_eq!(data["drilldown_status"], "incomplete");
    assert_eq!(data["drilldown_diagnostics"][0]["detail"], "node_budget");
    assert!(data.get("snapshot_id").is_none());
    assert_eq!(store.with_latest(42, 7, |id, _| id), Some(parent));
}

#[test]
fn parent_retirement_during_the_walk_rejects_publication() {
    for retirement in ["session", "remove", "clear"] {
        let store = SnapshotStore::new();
        let parent = full(&store);
        let capture = root(&store, &token_for(parent, 0));
        match retirement {
            "session" => {
                store.retire_session_screenshots("owner");
            }
            "remove" => {
                store.remove(42, 7);
            }
            "clear" => {
                store.clear();
            }
            _ => unreachable!(),
        }
        let error = commit(&store, capture, Payload(vec![3])).unwrap_err();
        assert_eq!(
            error.structured_content.unwrap()["refusal"]["code"],
            "stale_parent",
            "{retirement}"
        );
    }
}

#[test]
fn stale_parent_takes_precedence_over_an_incomplete_walk() {
    let store = SnapshotStore::new();
    let parent = full(&store);
    let capture = root(&store, &token_for(parent, 0));
    full(&store);
    let error = store
        .commit_scoped_capture(capture, ScopedWalk::Incomplete(vec![]))
        .unwrap_err();
    assert_eq!(
        error.structured_content.unwrap()["refusal"]["code"],
        "stale_parent"
    );
}

#[test]
fn retired_session_cannot_publish_a_capture_after_same_name_is_reused() {
    let store = SnapshotStore::new();
    let parent = full(&store);
    // The full snapshot remains because a different session owns its frame.
    store
        .refresh_screenshot_for_session(42, 7, Some("other"), 1.0, None)
        .unwrap();
    let old = root(&store, &token_for(parent, 0));
    store.retire_session_screenshots("owner");
    assert_eq!(store.with_latest(42, 7, |id, _| id), Some(parent));
    assert!(store
        .begin_scoped_capture(
            42,
            7,
            &token_for(parent, 0),
            Some("owner"),
            InstanceEvidence::Live(fingerprint(1))
        )
        .is_err());
    let error = commit(&store, old, Payload(vec![1])).unwrap_err();
    assert_eq!(
        error.structured_content.unwrap()["refusal"]["code"],
        "stale_parent"
    );
    let fresh_parent = full(&store);
    let new = root(&store, &token_for(fresh_parent, 0));
    assert!(commit(&store, new, Payload(vec![2])).is_ok());
}
