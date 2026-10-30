use crate::snapshot_store::{SnapshotPayload, SnapshotStore};
use crate::window_basis::{
    format_basis_id, parse_basis_id, DispatchEffect, DispatchOutcome, LiveFrame, WindowBasis,
};

fn basis() -> WindowBasis {
    WindowBasis {
        window_id: 7,
        pid: 42,
        snapshot_id: String::new(),
        x: -100,
        y: 20,
        width: 200,
        height: 100,
        scale_x: 0.5,
        scale_y: 0.8,
        capture_width: 100,
        capture_height: 80,
        display_scale: Some(1.25),
        captured_at_ms: 1,
    }
}
fn live() -> LiveFrame {
    LiveFrame {
        x: -100,
        y: 20,
        width: 200,
        height: 100,
        display_scale: Some(1.25),
    }
}
#[test]
fn math_and_geometry() {
    let b = basis();
    assert_eq!(b.to_screen(3.9, 4.9), Ok((-93, 26)));
    assert_eq!(b.to_screen(99.9, 79.9), Ok((99, 119)));
    for (x, y) in [(-0.1, 0.0), (100.0, 0.0), (0.0, 80.0)] {
        assert_eq!(b.to_screen(x, y).unwrap_err().code, "point_outside_window");
    }
    assert_eq!(b.to_screen(f64::NAN, 0.0).unwrap_err().code, "invalid");
    assert!(b.check(&live()).is_ok());
    let mut f = live();
    f.x += 1;
    assert_eq!(b.check(&f).unwrap_err().reason, "window_moved");
    let mut f = live();
    f.height += 1;
    assert_eq!(b.check(&f).unwrap_err().reason, "window_resized");
    let mut f = live();
    f.display_scale = Some(1.0);
    assert_eq!(b.check(&f).unwrap_err().reason, "scale_changed");
    let j = b
        .refusal("stale_basis", "window_moved", Some(&live()))
        .structured_content
        .unwrap();
    assert_eq!(j["effect"], "refused");
    assert_eq!(j["nothing_sent"], true);
    assert_eq!(j["window_basis"]["window_id"], 7);
    assert_eq!(j["live_frame"]["x"], -100);
    assert_eq!(parse_basis_id(&format_basis_id(12, 0)), Some((12, 0)));
    assert_eq!(parse_basis_id(&format_basis_id(12, 3)), Some((12, 3)));
    assert_eq!(parse_basis_id("s0000000c.f0"), None);
}
#[derive(Clone)]
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
fn reason(result: Result<WindowBasis, crate::protocol::ToolResult>) -> String {
    result.unwrap_err().structured_content.unwrap()["reason"]
        .as_str()
        .unwrap()
        .to_owned()
}
#[test]
fn generations_sessions_and_tickets() {
    let s = SnapshotStore::new();
    let a = s.begin_capture(42, 7);
    let z = s.begin_capture(42, 7);
    let (id, _) = s
        .publish_basis_for_session(42, 7, Payload, Some("a"), Some(1.0), Some(basis()), z)
        .unwrap();
    let key = format_basis_id(id, 0);
    assert!(s
        .publish_basis_for_session(42, 7, Payload, Some("a"), Some(1.0), Some(basis()), a)
        .is_none());
    assert!(s.basis_for(42, 7, &key, Some("a")).is_ok());
    assert_eq!(
        reason(s.basis_for(43, 7, &key, Some("a"))),
        "unknown_snapshot"
    );
    assert_eq!(reason(s.basis_for(42, 8, &key, Some("a"))), "other_window");
    assert_eq!(reason(s.basis_for(42, 7, &key, Some("b"))), "other_session");
    let t = s.begin_capture(42, 7);
    assert_eq!(
        s.refresh_basis_for_session(42, 7, Some("a"), 1.0, None, Some(basis()), t),
        Some(id)
    );
    assert!(s.basis_for(42, 7, &key, Some("a")).is_ok());
    let mut changed = basis();
    changed.scale_x = 0.25;
    let t = s.begin_capture(42, 7);
    s.refresh_basis_for_session(42, 7, Some("a"), 1.0, None, Some(changed), t)
        .unwrap();
    assert_eq!(
        reason(s.basis_for(42, 7, &key, Some("a"))),
        "frame_replaced"
    );
    let next = format_basis_id(id, 1);
    assert!(s.basis_for(42, 7, &next, Some("a")).is_ok());
    let t = s.begin_capture(42, 7);
    s.refresh_basis_for_session(42, 7, Some("b"), 1.0, None, Some(basis()), t)
        .unwrap();
    assert_eq!(
        reason(s.basis_for(42, 7, &next, Some("b"))),
        "frame_replaced"
    );
    let t = s.begin_capture(42, 7);
    s.refresh_basis_for_session(42, 7, Some("b"), 1.0, None, None, t)
        .unwrap();
    assert_eq!(
        reason(s.basis_for(42, 7, &format_basis_id(id, 3), Some("b"))),
        "no_basis"
    );
}
#[test]
fn effect_json() {
    let o = DispatchOutcome {
        events_planned: 3,
        events_sent: 1,
        prepared_side_effects: vec!["cursor_moved".into()],
        effect: DispatchEffect::Partial,
        cleanup_results: vec![serde_json::json!({"event":"button_up","ok":true})],
    };
    let j = o.to_json();
    assert_eq!(j["effect"], "partial");
    assert_eq!(j["events_sent"], 1);
    assert_eq!(j["cleanup_results"][0]["ok"], true);
}

#[test]
fn first_screenshot_only_publication_and_no_basis_retirement() {
    let s = SnapshotStore::new();
    let t = s.begin_capture(42, 7);
    let id = s
        .publish_capture_basis_for_session(42, 7, Payload, Some("a"), Some(1.0), Some(basis()), t)
        .unwrap()
        .0;
    let first = format_basis_id(id, 0);
    assert_eq!(
        s.basis_for(42, 7, &first, Some("a")).unwrap().snapshot_id,
        first
    );
    let t = s.begin_capture(42, 7);
    s.refresh_basis_for_session(42, 7, Some("a"), 1.0, None, None, t)
        .unwrap();
    assert_eq!(
        reason(s.basis_for(42, 7, &first, Some("a"))),
        "frame_replaced"
    );
    assert_eq!(
        reason(s.basis_for(42, 7, &format_basis_id(id, 1), Some("a"))),
        "no_basis"
    );
}

#[test]
fn effect_json_accounts_for_zero_and_post_send_failures() {
    for (effect, sent, label) in [
        (DispatchEffect::Refused, 0, "refused"),
        (DispatchEffect::Partial, 1, "partial"),
        (DispatchEffect::Uncertain, 2, "uncertain"),
        (DispatchEffect::Unverifiable, 2, "unverifiable"),
    ] {
        let value = DispatchOutcome {
            events_planned: 2,
            events_sent: sent,
            prepared_side_effects: vec![],
            effect,
            cleanup_results: vec![],
        }
        .to_json();
        assert_eq!(value["effect"], label);
        assert_eq!(
            value.get("nothing_sent").and_then(|v| v.as_bool()),
            if sent == 0 { Some(true) } else { None }
        );
    }
}

#[test]
fn plain_publish_supersedes_an_older_capture() {
    let s = SnapshotStore::new();
    let old = s.begin_capture(42, 7);
    let (id, _) = s
        .publish_for_session(42, 7, Payload, Some("a"), None)
        .unwrap();
    // The capture began before the tree read: it must neither replace the
    // newer snapshot (retiring its tokens) nor attach its frame to it.
    assert!(s
        .publish_basis_for_session(42, 7, Payload, Some("a"), Some(1.0), Some(basis()), old)
        .is_none());
    assert!(s
        .publish_capture_basis_for_session(42, 7, Payload, Some("a"), Some(1.0), Some(basis()), old)
        .is_none());
    assert_eq!(
        s.refresh_basis_for_session(42, 7, Some("a"), 1.0, None, Some(basis()), old),
        None
    );
    assert_eq!(s.window_for_snapshot(42, id), Some(7));
    assert_eq!(
        reason(s.basis_for(42, 7, &format_basis_id(id, 0), Some("a"))),
        "no_basis"
    );
    // A capture that begins after the publication still lands.
    let new = s.begin_capture(42, 7);
    assert_eq!(
        s.refresh_basis_for_session(42, 7, Some("a"), 1.0, None, Some(basis()), new),
        Some(id)
    );
    assert!(s
        .basis_for(42, 7, &format_basis_id(id, 1), Some("a"))
        .is_ok());
}

#[test]
fn plain_capture_publish_and_screenshot_refresh_supersede_older_captures() {
    let s = SnapshotStore::new();
    let first = s.begin_capture(42, 7);
    let (id, _) = s
        .publish_capture_for_session(42, 7, Payload, Some("a"), Some(1.0))
        .unwrap();
    assert!(s
        .publish_capture_basis_for_session(
            42,
            7,
            Payload,
            Some("a"),
            Some(1.0),
            Some(basis()),
            first
        )
        .is_none());
    assert_eq!(s.window_for_snapshot(42, id), Some(7));

    let t = s.begin_capture(42, 7);
    let (id, _) = s
        .publish_basis_for_session(42, 7, Payload, Some("a"), Some(1.0), Some(basis()), t)
        .unwrap();
    let old = s.begin_capture(42, 7);
    assert_eq!(
        s.refresh_screenshot_for_session(42, 7, Some("a"), 1.0, None),
        Some(id)
    );
    // The screenshot-only read is newer than the in-flight capture, so the
    // capture's frame must not replace it.
    assert_eq!(
        s.refresh_basis_for_session(42, 7, Some("a"), 1.0, None, Some(basis()), old),
        None
    );
    assert_eq!(
        reason(s.basis_for(42, 7, &format_basis_id(id, 1), Some("a"))),
        "no_basis"
    );
}

#[test]
fn old_ticket_refresh_after_a_newer_publication_changes_nothing() {
    let s = SnapshotStore::new();
    let old = s.begin_capture(42, 7);
    let newer = s.begin_capture(42, 7);
    let (id, _) = s
        .publish_basis_for_session(42, 7, Payload, Some("a"), Some(1.0), Some(basis()), newer)
        .unwrap();
    let mut older = basis();
    older.x += 50;
    assert_eq!(
        s.refresh_basis_for_session(42, 7, Some("a"), 1.0, None, Some(older), old),
        None
    );
    assert_eq!(
        s.basis_for(42, 7, &format_basis_id(id, 0), Some("a"))
            .unwrap()
            .x,
        -100
    );

    // Same after a plain publication, with a newer capture also in flight.
    let old = s.begin_capture(42, 7);
    let (id, _) = s
        .publish_for_session(42, 7, Payload, Some("a"), Some(1.0))
        .unwrap();
    let new = s.begin_capture(42, 7);
    assert_eq!(
        s.refresh_basis_for_session(42, 7, Some("a"), 1.0, None, Some(basis()), old),
        None
    );
    assert_eq!(
        s.refresh_basis_for_session(42, 7, Some("a"), 1.0, None, Some(basis()), new),
        Some(id)
    );
}
