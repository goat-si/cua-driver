use crate::window_basis::{LiveFrame, WindowBasis};

#[allow(clippy::too_many_arguments)]
pub fn build_capture_basis(
    window_id: u64,
    pid: i32,
    pre: Option<LiveFrame>,
    post: Option<LiveFrame>,
    geometry: Option<LiveFrame>,
    native: (u32, u32),
    delivered: (u32, u32),
    captured_at_ms: u64,
) -> Result<WindowBasis, &'static str> {
    let geometry = geometry.ok_or("no_proven_origin")?;
    let pre = pre.ok_or("window_moved_during_capture")?;
    let post = post.ok_or("window_moved_during_capture")?;
    if pre != post
        || pre.x != geometry.x
        || pre.y != geometry.y
        || pre.width != geometry.width
        || pre.height != geometry.height
        || native != (geometry.width, geometry.height)
        || native.0 == 0
        || native.1 == 0
        || delivered.0 == 0
        || delivered.1 == 0
    {
        return Err("window_moved_during_capture");
    }
    Ok(WindowBasis {
        window_id,
        pid,
        snapshot_id: String::new(),
        x: geometry.x,
        y: geometry.y,
        width: geometry.width,
        height: geometry.height,
        scale_x: f64::from(delivered.0) / f64::from(native.0),
        scale_y: f64::from(delivered.1) / f64::from(native.1),
        capture_width: delivered.0,
        capture_height: delivered.1,
        display_scale: pre.display_scale,
        captured_at_ms,
    })
}

/// Why a screenshot that was returned carries no window_basis.
///
/// `capture` is the basis decision for this capture (None when no capture
/// decision was made), `published` says whether this capture's publication
/// was applied by the store, and `stored` whether the store kept a basis.
pub fn basis_unavailable_reason(
    capture: Option<&Result<WindowBasis, &'static str>>,
    published: bool,
    stored: bool,
) -> Option<&'static str> {
    if stored {
        return None;
    }
    Some(match capture {
        Some(Err(reason)) => reason,
        // The store did not keep this frame: a newer capture or plain
        // publication of the window, its removal, LRU eviction or the end of
        // the session made the capture's ticket obsolete before it landed.
        // The screenshot in this response has no basis to click against.
        Some(Ok(_)) if !published => "capture_not_stored",
        // Published, but the store retired the window's basis identity.
        Some(Ok(_)) => "basis_retired",
        None => "no_proven_origin",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window_basis::LiveFrame;

    fn frame() -> LiveFrame {
        LiveFrame {
            x: -10,
            y: 20,
            width: 800,
            height: 600,
            display_scale: Some(1.25),
        }
    }

    #[test]
    fn proves_geometry_and_reports_per_axis_downscale() {
        let f = frame();
        let basis = build_capture_basis(
            7,
            42,
            Some(f),
            Some(f),
            Some(f),
            (800, 600),
            (400, 200),
            123,
        )
        .unwrap();
        assert_eq!(
            basis.to_json(),
            serde_json::json!({"window_id":7,"pid":42,"snapshot_id":"","x":-10,"y":20,"width":800,"height":600,"scale_x":0.5,"scale_y":1.0/3.0,"capture_width":400,"capture_height":200,"display_scale":1.25,"captured_at_ms":123})
        );
    }

    #[test]
    fn refuses_unproven_or_changed_geometry() {
        let f = frame();
        let mut moved = f;
        moved.x += 1;
        assert_eq!(
            build_capture_basis(
                7,
                42,
                Some(f),
                Some(moved),
                Some(f),
                (800, 600),
                (800, 600),
                1
            )
            .unwrap_err(),
            "window_moved_during_capture"
        );
        assert_eq!(
            build_capture_basis(7, 42, Some(f), Some(f), Some(f), (799, 600), (799, 600), 1)
                .unwrap_err(),
            "window_moved_during_capture"
        );
        assert_eq!(
            build_capture_basis(7, 42, Some(f), Some(f), None, (800, 600), (800, 600), 1)
                .unwrap_err(),
            "no_proven_origin"
        );
        assert_eq!(
            build_capture_basis(7, 42, None, Some(f), Some(f), (800, 600), (800, 600), 1)
                .unwrap_err(),
            "window_moved_during_capture"
        );
        let mut other_scale = f;
        other_scale.display_scale = Some(2.0);
        assert_eq!(
            build_capture_basis(
                7,
                42,
                Some(f),
                Some(other_scale),
                Some(f),
                (800, 600),
                (800, 600),
                1
            )
            .unwrap_err(),
            "window_moved_during_capture"
        );
        assert_eq!(
            build_capture_basis(7, 42, Some(f), Some(f), Some(f), (800, 600), (0, 600), 1)
                .unwrap_err(),
            "window_moved_during_capture"
        );
    }

    #[test]
    fn unavailable_reason_names_why_the_screenshot_has_no_basis() {
        let f = frame();
        let proven =
            build_capture_basis(7, 42, Some(f), Some(f), Some(f), (800, 600), (800, 600), 1);
        assert!(proven.is_ok());
        assert_eq!(basis_unavailable_reason(Some(&proven), true, true), None);
        assert_eq!(
            basis_unavailable_reason(Some(&proven), false, false),
            Some("capture_not_stored")
        );
        assert_eq!(
            basis_unavailable_reason(Some(&proven), true, false),
            Some("basis_retired")
        );
        let moved: Result<WindowBasis, &'static str> = Err("window_moved_during_capture");
        assert_eq!(
            basis_unavailable_reason(Some(&moved), false, false),
            Some("window_moved_during_capture")
        );
        let unproven: Result<WindowBasis, &'static str> = Err("no_proven_origin");
        assert_eq!(
            basis_unavailable_reason(Some(&unproven), true, false),
            Some("no_proven_origin")
        );
        assert_eq!(
            basis_unavailable_reason(None, false, false),
            Some("no_proven_origin")
        );
    }
}
