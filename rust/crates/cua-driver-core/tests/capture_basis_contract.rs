use cua_driver_core::basis_capture::{basis_unavailable_reason, build_capture_basis};
use cua_driver_core::window_basis::LiveFrame;

#[test]
fn capture_basis_rejects_movement_and_reports_downscale() {
    let frame = LiveFrame {
        x: 12,
        y: 34,
        width: 800,
        height: 600,
        display_scale: None,
    };
    let basis = build_capture_basis(
        7,
        42,
        Some(frame),
        Some(frame),
        Some(frame),
        (800, 600),
        (400, 300),
        1,
    )
    .unwrap();
    assert_eq!((basis.scale_x, basis.scale_y), (0.5, 0.5));
    let mut moved = frame;
    moved.x += 1;
    let rejected = build_capture_basis(
        7,
        42,
        Some(frame),
        Some(moved),
        Some(frame),
        (800, 600),
        (400, 300),
        1,
    );
    assert_eq!(
        basis_unavailable_reason(Some(&rejected), false, false),
        Some("window_moved_during_capture")
    );
}
