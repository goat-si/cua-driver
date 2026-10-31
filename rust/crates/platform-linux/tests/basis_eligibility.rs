use platform_linux::basis_eligibility::{
    capture_eligibility, session_kind, CaptureBackend, SessionKind,
};

#[test]
fn only_exact_x11_drawable_capture_is_eligible() {
    for backend in [CaptureBackend::XShm, CaptureBackend::XGetImage] {
        assert_eq!(capture_eligibility(SessionKind::X11, backend, true), Ok(()));
        assert_eq!(
            capture_eligibility(SessionKind::X11, backend, false),
            Err("no_proven_origin")
        );
        assert_eq!(
            capture_eligibility(SessionKind::Wayland, backend, true),
            Err("no_proven_origin")
        );
    }
    for backend in [
        CaptureBackend::Import,
        CaptureBackend::RootRegion,
        CaptureBackend::Other,
    ] {
        assert_eq!(
            capture_eligibility(SessionKind::X11, backend, true),
            Err("no_proven_origin")
        );
    }
}

#[test]
fn wayland_session_stays_ineligible_when_native_backend_is_disabled() {
    use platform_linux::basis_eligibility::session_kind;
    assert_eq!(session_kind(false, true, false), SessionKind::Wayland);
    assert_eq!(session_kind(false, false, true), SessionKind::Wayland);
    assert_eq!(session_kind(true, false, false), SessionKind::Wayland);
    assert_eq!(session_kind(false, false, false), SessionKind::X11);
}

use cua_driver_core::window_basis::LiveFrame;
use platform_linux::basis_eligibility::{capture_basis, response_fields, CapturePath};
use serde_json::json;

#[test]
fn untrusted_rows_are_removed_before_capture_only_publication() {
    use cua_driver_core::snapshot_store::{SnapshotPayload, SnapshotStore};
    use platform_linux::basis_eligibility::discard_untrusted_snapshot;
    struct Payload;
    impl SnapshotPayload for Payload {
        type Element = ();
        fn len(&self) -> usize {
            0
        }
        fn retain(&self, _: usize) -> Option<()> {
            None
        }
    }
    let store = SnapshotStore::new();
    let old = store
        .publish_for_session(42, 7, Payload, None, Some(1.0))
        .unwrap()
        .0;
    let ticket = store.begin_capture(42, 7);
    assert_eq!(
        discard_untrusted_snapshot(&store, 42, 7, Some(&ticket)),
        Some(old)
    );
    let f = frame();
    let basis = capture_basis(
        7,
        42,
        SessionKind::X11,
        CapturePath::X11Client(Some(f)),
        Some(f),
        Some(f),
        (800, 600),
        (200, 150),
        123,
    )
    .unwrap();
    assert!(store
        .refresh_basis_for_session(42, 7, None, 0.25, None, Some(basis.clone()), &ticket)
        .is_none());
    let (_, _, stored) = store
        .publish_capture_basis_for_session(42, 7, Payload, None, Some(0.25), Some(basis), &ticket)
        .unwrap();
    let stored = stored.unwrap();
    assert!(store.basis_for(42, 7, &stored.snapshot_id, None).is_ok());
    assert_eq!(
        store.screenshot_context(42, Some(7), None).unwrap().scale,
        0.25
    );
    assert!(discard_untrusted_snapshot(&store, 42, 7, None).is_some());
}

fn frame() -> LiveFrame {
    LiveFrame {
        x: 10,
        y: 20,
        width: 800,
        height: 600,
        display_scale: None,
    }
}

#[test]
fn capture_decision_fences_provenance_and_uses_final_size() {
    let f = frame();
    let decide = |session, path, pre, post| {
        capture_basis(7, 42, session, path, pre, post, (800, 600), (200, 150), 123)
    };
    for session in [
        session_kind(false, true, false),
        session_kind(false, false, true),
        SessionKind::Wayland,
    ] {
        assert_eq!(
            decide(session, CapturePath::X11Client(Some(f)), Some(f), Some(f)).unwrap_err(),
            "no_proven_origin"
        );
    }
    for path in [
        CapturePath::Preview,
        CapturePath::RootRegion,
        CapturePath::WaylandDispatch,
        CapturePath::ImportFallback,
        CapturePath::X11Client(None),
    ] {
        assert_eq!(
            decide(SessionKind::X11, path, Some(f), Some(f)).unwrap_err(),
            "no_proven_origin"
        );
    }
    let mut moved = f;
    moved.x += 1;
    assert_eq!(
        decide(
            SessionKind::X11,
            CapturePath::X11Client(Some(f)),
            Some(f),
            Some(moved)
        )
        .unwrap_err(),
        "window_moved_during_capture"
    );
    assert_eq!(
        decide(
            SessionKind::X11,
            CapturePath::X11Client(Some(moved)),
            Some(f),
            Some(f)
        )
        .unwrap_err(),
        "window_moved_during_capture"
    );
    let basis = decide(
        SessionKind::X11,
        CapturePath::X11Client(Some(f)),
        Some(f),
        Some(f),
    )
    .unwrap();
    assert_eq!(
        (
            basis.scale_x,
            basis.scale_y,
            basis.capture_width,
            basis.capture_height
        ),
        (0.25, 0.25, 200, 150)
    );
}

#[test]
fn response_emits_only_the_stored_basis() {
    let f = frame();
    let candidate = capture_basis(
        7,
        42,
        SessionKind::X11,
        CapturePath::X11Client(Some(f)),
        Some(f),
        Some(f),
        (800, 600),
        (200, 150),
        123,
    );
    let mut stored = candidate.clone().unwrap();
    stored.snapshot_id = "stored-snapshot".into();
    assert_eq!(
        response_fields(None, Some(&candidate), false, true, false),
        json!({"basis_unavailable_reason":"capture_not_stored"})
    );
    assert_eq!(
        response_fields(None, Some(&candidate), true, true, false),
        json!({"basis_unavailable_reason":"basis_retired"})
    );
    assert_eq!(
        response_fields(Some(&stored), Some(&candidate), true, true, false),
        json!({"window_basis":stored.to_json()})
    );
    assert_eq!(
        response_fields(None, Some(&candidate), false, true, true),
        json!({})
    );
    assert_eq!(response_fields(None, None, false, false, false), json!({}));
}
