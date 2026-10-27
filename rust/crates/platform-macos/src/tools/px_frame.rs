//! The one place that turns window-local screenshot pixels into screen
//! coordinates for the pixel rungs (`click`, `double_click`, `right_click`,
//! `drag`, `scroll`).
//!
//! Issue #2237: each of those tools carried a byte-identical copy of this
//! translation, and every copy silently reinterpreted window-local pixels as
//! screen-absolute when `window_bounds_by_id` returned `None`. A dismissed
//! Open/Save panel's coordinates then landed on whatever was behind it — a
//! misclick reported as success. There is no safe fallback here: without the
//! window's origin there is nothing to add the local point to, so a missing or
//! stale window is a refusal.

use cua_driver_core::protocol::ToolResult;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use crate::windows::WindowBounds;

/// A window's screen origin plus the physical-pixels-per-logical-point scale
/// of its `screencapture` output.
#[derive(Debug, Clone)]
pub struct WindowPxFrame {
    pub bounds: WindowBounds,
    /// Physical capture pixels per logical point (1.0 non-Retina, 2.0 Retina).
    pub scale: f64,
}

impl WindowPxFrame {
    /// Window-local capture pixels → `(screen_x, screen_y, local_pt_x,
    /// local_pt_y)`. Public PID event routes use the local-point pair for
    /// `CGEventSetWindowLocation`; the SkyLight PID/window-routed click keeps
    /// the screen-point pair because WindowServer derives local coordinates.
    pub fn to_screen(&self, cx: f64, cy: f64) -> (f64, f64, f64, f64) {
        let lx = cx / self.scale;
        let ly = cy / self.scale;
        (self.bounds.x + lx, self.bounds.y + ly, lx, ly)
    }
}

/// Why a window-local pixel action cannot be translated.
#[derive(Debug, Clone, PartialEq)]
pub enum PxFrameError {
    /// WindowServer has no usable frame for this id: it was closed, the id is
    /// stale or fabricated, or the record carries degenerate `0×0` bounds
    /// (`windows.rs`'s missing-`kCGWindowBounds` default), which would make
    /// the window origin indistinguishable from the screen origin.
    WindowNotFound { window_id: u32 },
    /// The current capture could not be obtained or decoded, so its pixel
    /// coordinate system cannot be proven against the WindowServer frame.
    CaptureUnavailable { window_id: u32, reason: String },
    /// The capture dimensions are not a coherent 1×/2× representation of the
    /// requested WindowServer frame. Dispatching with this transform would
    /// recreate #2237's wrong-surface misclick.
    FrameMismatch {
        window_id: u32,
        bounds_width: f64,
        bounds_height: f64,
        capture_width: u32,
        capture_height: u32,
        scale_x: f64,
        scale_y: f64,
    },
}

/// Validate that a raw window capture and its WindowServer bounds describe one
/// coordinate frame, returning the physical-pixels-per-point backing scale.
///
/// macOS window captures are 1× or 2×. Small rounding differences are allowed,
/// but arbitrary ratios are not: the original #2237 failure paired a parent
/// window screenshot with panel bounds and would otherwise look like a
/// plausible-but-wrong ~2.2× scale.
pub fn validate_capture_frame(
    window_id: u32,
    bounds: &WindowBounds,
    capture_px_w: u32,
    capture_px_h: u32,
) -> Result<f64, PxFrameError> {
    if bounds.width <= 0.0 || bounds.height <= 0.0 {
        return Err(PxFrameError::WindowNotFound { window_id });
    }
    let scale_x = capture_px_w as f64 / bounds.width;
    let scale_y = capture_px_h as f64 / bounds.height;
    let nearest = if (scale_x - 1.0).abs() <= (scale_x - 2.0).abs() {
        1.0
    } else {
        2.0
    };
    let coherent = scale_x.is_finite()
        && scale_y.is_finite()
        && (scale_x - scale_y).abs() <= 0.03
        && (scale_x - nearest).abs() <= 0.03
        && (scale_y - nearest).abs() <= 0.03;
    if coherent {
        Ok(nearest)
    } else {
        Err(PxFrameError::FrameMismatch {
            window_id,
            bounds_width: bounds.width,
            bounds_height: bounds.height,
            capture_width: capture_px_w,
            capture_height: capture_px_h,
            scale_x,
            scale_y,
        })
    }
}

/// Geometry only: no image bytes, tokens, session bindings or action authority.
/// Keep a bounded process-wide index because all pixel tools share this resolver.
#[derive(Clone)]
struct PublishedWindowFrame {
    pid: i32,
    bounds: WindowBounds,
    dimensions: (u32, u32),
}

#[derive(Default)]
struct PublishedFrames(VecDeque<(u32, PublishedWindowFrame)>);

impl PublishedFrames {
    const CAPACITY: usize = 128;

    fn insert(&mut self, window_id: u32, frame: PublishedWindowFrame) {
        self.0.retain(|(id, _)| *id != window_id);
        if self.0.len() == Self::CAPACITY {
            self.0.pop_front();
        }
        self.0.push_back((window_id, frame));
    }

    fn get(&self, window_id: u32, pid: i32) -> Option<PublishedWindowFrame> {
        self.0
            .iter()
            .find(|(id, frame)| *id == window_id && frame.pid == pid)
            .map(|(_, frame)| frame.clone())
    }
}

fn published_frames() -> &'static Mutex<PublishedFrames> {
    static FRAMES: OnceLock<Mutex<PublishedFrames>> = OnceLock::new();
    FRAMES.get_or_init(|| Mutex::new(PublishedFrames::default()))
}

/// Record native, pre-resize capture dimensions only after publication succeeds.
pub(super) fn remember_published_frame(
    window_id: u32,
    pid: i32,
    bounds: WindowBounds,
    dimensions: (u32, u32),
) {
    published_frames().lock().unwrap().insert(
        window_id,
        PublishedWindowFrame {
            pid,
            bounds,
            dimensions,
        },
    );
}

fn same_bounds(a: &WindowBounds, b: &WindowBounds) -> bool {
    a.x == b.x && a.y == b.y && a.width == b.width && a.height == b.height
}

/// Lazy inputs make the absence of capture a deterministic, testable property.
/// A matching published frame wins, including an explicit mismatch refusal;
/// moving or resizing the window invalidates that evidence.
fn resolve_dimensions(
    window_id: u32,
    bounds: &WindowBounds,
    published: Option<&PublishedWindowFrame>,
    display_scale: impl FnOnce() -> Option<f64>,
    capture: impl FnOnce() -> Result<(u32, u32), PxFrameError>,
) -> Result<(f64, (u32, u32)), PxFrameError> {
    if !bounds.x.is_finite()
        || !bounds.y.is_finite()
        || !bounds.width.is_finite()
        || !bounds.height.is_finite()
        || bounds.width <= 0.0
        || bounds.height <= 0.0
    {
        return Err(PxFrameError::WindowNotFound { window_id });
    }
    let dimensions = if let Some(frame) = published.filter(|f| same_bounds(&f.bounds, bounds)) {
        frame.dimensions
    } else if let Some(scale) = display_scale().filter(|scale| *scale == 1.0 || *scale == 2.0) {
        let width = (bounds.width * scale).round();
        let height = (bounds.height * scale).round();
        if width < 1.0 || height < 1.0 || width > u32::MAX as f64 || height > u32::MAX as f64 {
            capture()?
        } else {
            (width as u32, height as u32)
        }
    } else {
        capture()?
    };
    validate_capture_frame(window_id, bounds, dimensions.0, dimensions.1)
        .map(|scale| (scale, dimensions))
}

/// Select the display containing the largest part of the window. No overlap,
/// an ambiguous tie, or unavailable mode metadata is not evidence of a scale.
fn scale_for_displays(
    bounds: &WindowBounds,
    displays: &[(WindowBounds, Option<f64>)],
) -> Option<f64> {
    let mut best_area = 0.0;
    let mut best_scale = None;
    for (display, scale) in displays {
        let width = ((bounds.x + bounds.width).min(display.x + display.width)
            - bounds.x.max(display.x))
        .max(0.0);
        let height = ((bounds.y + bounds.height).min(display.y + display.height)
            - bounds.y.max(display.y))
        .max(0.0);
        let area = width * height;
        if area > best_area {
            best_area = area;
            best_scale = *scale;
        } else if area > 0.0 && area == best_area && best_scale != *scale {
            best_scale = None;
        }
    }
    best_scale
}

fn window_display_scale(bounds: &WindowBounds) -> Option<f64> {
    use core_graphics::display::CGDisplay;
    let displays = CGDisplay::active_displays().ok()?;
    let displays: Vec<_> = displays
        .into_iter()
        .map(|id| {
            let display = CGDisplay::new(id);
            let rect = display.bounds();
            let scale = display.display_mode().and_then(|mode| {
                let sx = mode.pixel_width() as f64 / mode.width() as f64;
                let sy = mode.pixel_height() as f64 / mode.height() as f64;
                (sx == sy && (sx == 1.0 || sx == 2.0)).then_some(sx)
            });
            (
                WindowBounds {
                    x: rect.origin.x,
                    y: rect.origin.y,
                    width: rect.size.width,
                    height: rect.size.height,
                },
                scale,
            )
        })
        .collect();
    scale_for_displays(bounds, &displays)
}

fn resolve_window_dimensions(window_id: u32) -> Result<(WindowPxFrame, (u32, u32)), PxFrameError> {
    let window = crate::windows::window_info_by_id(window_id)
        .ok_or(PxFrameError::WindowNotFound { window_id })?;
    let bounds = window.bounds;
    let published = published_frames()
        .lock()
        .unwrap()
        .get(window_id, window.pid);
    let (scale, dimensions) = resolve_dimensions(
        window_id,
        &bounds,
        published.as_ref(),
        || window_display_scale(&bounds),
        || {
            let result = crate::capture::screenshot_window_bytes(window_id)
                .and_then(|png| crate::capture::png_dimensions(&png));
            result.map_err(|error| PxFrameError::CaptureUnavailable {
                window_id,
                reason: error.to_string(),
            })
        },
    )?;
    Ok((WindowPxFrame { bounds, scale }, dimensions))
}

/// Blocking WindowServer/display metadata reads; captures only when neither
/// the last published frame nor the window's display proves its scale.
pub fn resolve_window_px_frame(window_id: u32) -> Result<WindowPxFrame, PxFrameError> {
    resolve_window_dimensions(window_id).map(|(frame, _)| frame)
}

/// The shared refusal for a pixel action whose window cannot be framed.
pub fn refusal(error: &PxFrameError) -> ToolResult {
    match error {
        PxFrameError::WindowNotFound { window_id } => ToolResult::error(format!(
            "window_id {window_id} has no live frame, so window-local pixels cannot be \
             translated to screen coordinates. Refusing to dispatch — treating them as \
             screen-absolute would act on whatever is behind the closed window. Call \
             list_windows for a current window_id, then re-snapshot with get_window_state."
        ))
        .with_structured(error_structured(error)),
        PxFrameError::CaptureUnavailable { window_id, reason } => ToolResult::error(format!(
            "window_id {window_id}'s current capture is unavailable ({reason}), so its \
                 pixel coordinate frame cannot be verified. Refusing to dispatch."
        ))
        .with_structured(error_structured(error)),
        PxFrameError::FrameMismatch {
            window_id,
            bounds_width,
            bounds_height,
            capture_width,
            capture_height,
            scale_x,
            scale_y,
        } => ToolResult::error(format!(
            "window_id {window_id}'s capture is {capture_width}x{capture_height}, but its \
             WindowServer bounds are {bounds_width:.2}x{bounds_height:.2} points \
             (scale_x={scale_x:.4}, scale_y={scale_y:.4}). These are not one coherent \
             1x/2x frame. Refusing to dispatch."
        ))
        .with_structured(error_structured(error)),
    }
}

/// Structured form shared by action refusals and a successful state snapshot
/// whose optional screenshot could not be proven. The latter must keep its AX
/// payload rather than turning a capture problem into a total perception
/// failure.
pub fn error_structured(error: &PxFrameError) -> serde_json::Value {
    match error {
        PxFrameError::WindowNotFound { window_id } => serde_json::json!({
            "code": "px_window_not_found",
            "window_id": window_id,
            "suggestion": "refusing to interpret window-local pixels as screen-absolute; \
                           call list_windows for a current window_id"
        }),
        PxFrameError::CaptureUnavailable { window_id, reason } => serde_json::json!({
            "code": "px_capture_unavailable",
            "window_id": window_id,
            "reason": reason,
            "suggestion": "re-snapshot the window after capture permission/state recovers"
        }),
        PxFrameError::FrameMismatch {
            window_id,
            bounds_width,
            bounds_height,
            capture_width,
            capture_height,
            scale_x,
            scale_y,
        } => serde_json::json!({
            "code": "px_frame_mismatch",
            "window_id": window_id,
            "window_bounds": {
                "width": bounds_width,
                "height": bounds_height
            },
            "capture_size": {
                "width": capture_width,
                "height": capture_height
            },
            "scale_x": scale_x,
            "scale_y": scale_y,
            "suggestion": "re-list and re-snapshot the exact window; do not reuse coordinates \
                           from a different surface"
        }),
    }
}

/// Resolve the frame off the async path, mapping both the task error and the
/// refusal onto a `ToolResult` the caller can return directly.
pub async fn resolve_or_refuse(window_id: u32) -> Result<WindowPxFrame, ToolResult> {
    match tokio::task::spawn_blocking(move || resolve_window_px_frame(window_id)).await {
        Ok(Ok(frame)) => Ok(frame),
        Ok(Err(e)) => Err(refusal(&e)),
        Err(e) => Err(ToolResult::error(format!(
            "window frame lookup for window_id {window_id} failed: {e}. Not dispatching."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn published(scale: u32) -> PublishedWindowFrame {
        PublishedWindowFrame {
            pid: 42,
            bounds: frame(1.0).bounds,
            dimensions: (500 * scale, 500 * scale),
        }
    }

    #[test]
    fn published_frame_avoids_display_lookup_and_capture_even_during_stall() {
        for scale in [1, 2] {
            let prior = published(scale);
            let result = resolve_dimensions(
                11,
                &prior.bounds,
                Some(&prior),
                || panic!("a matching publication is sufficient"),
                || panic!("a capture stall must not block a published frame"),
            )
            .unwrap();
            assert_eq!(result, (scale as f64, (500 * scale, 500 * scale)));
        }
    }

    #[test]
    fn changed_bounds_use_display_without_capture() {
        let prior = published(2);
        for bounds in [
            WindowBounds {
                x: -800.0,
                ..prior.bounds.clone()
            },
            WindowBounds {
                y: -200.0,
                ..prior.bounds.clone()
            },
            WindowBounds {
                width: 600.0,
                ..prior.bounds.clone()
            },
            WindowBounds {
                height: 600.0,
                ..prior.bounds.clone()
            },
        ] {
            let result = resolve_dimensions(
                11,
                &bounds,
                Some(&prior),
                || Some(1.0),
                || panic!("a proven display does not need a capture"),
            )
            .unwrap();
            assert_eq!(result, (1.0, (bounds.width as u32, bounds.height as u32)));
        }
    }

    #[test]
    fn uncached_window_uses_retina_display_without_capture() {
        let bounds = frame(1.0).bounds;
        assert_eq!(
            resolve_dimensions(
                11,
                &bounds,
                None,
                || Some(2.0),
                || panic!("display metadata must suffice for a cold window")
            )
            .unwrap(),
            (2.0, (1000, 1000))
        );
    }

    #[test]
    fn matching_malformed_publication_refuses_without_hiding_mismatch() {
        let mut prior = published(2);
        prior.dimensions = (1920, 960);
        assert!(matches!(
            resolve_dimensions(
                11,
                &prior.bounds,
                Some(&prior),
                || panic!("do not override published evidence"),
                || panic!("do not recapture to hide the mismatch")
            ),
            Err(PxFrameError::FrameMismatch { .. })
        ));
    }

    #[test]
    fn no_metadata_captures_once_and_preserves_errors() {
        let bounds = frame(1.0).bounds;
        for display in [None, Some(1.5), Some(f64::NAN), Some(0.0)] {
            let mut captures = 0;
            let result = resolve_dimensions(
                11,
                &bounds,
                None,
                || display,
                || {
                    captures += 1;
                    Ok((1000, 1000))
                },
            )
            .unwrap();
            assert_eq!(result, (2.0, (1000, 1000)));
            assert_eq!(captures, 1);
        }
        let error = PxFrameError::CaptureUnavailable {
            window_id: 11,
            reason: "capture_backend_stalled".into(),
        };
        assert_eq!(
            resolve_dimensions(11, &bounds, None, || None, || Err(error.clone())),
            Err(error)
        );
        assert!(matches!(
            resolve_dimensions(11, &bounds, None, || None, || Ok((250, 250))),
            Err(PxFrameError::FrameMismatch { .. })
        ));
    }

    #[test]
    fn invalid_live_bounds_refuse_before_any_scale_source() {
        for value in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let bounds = WindowBounds {
                width: value,
                ..frame(1.0).bounds
            };
            assert_eq!(
                resolve_dimensions(
                    11,
                    &bounds,
                    Some(&published(2)),
                    || panic!("invalid bounds"),
                    || panic!("invalid bounds")
                ),
                Err(PxFrameError::WindowNotFound { window_id: 11 })
            );
        }
    }

    #[test]
    fn published_index_is_bounded_replaces_and_checks_owner() {
        let mut frames = PublishedFrames::default();
        for id in 0..=PublishedFrames::CAPACITY as u32 {
            frames.insert(id, published(2));
        }
        assert_eq!(frames.0.len(), PublishedFrames::CAPACITY);
        assert!(frames.get(0, 42).is_none());
        assert!(frames.get(1, 43).is_none());
        frames.insert(1, published(1));
        assert_eq!(frames.0.len(), PublishedFrames::CAPACITY);
        assert_eq!(frames.get(1, 42).unwrap().dimensions, (500, 500));
    }

    #[test]
    fn display_selection_uses_largest_overlap_including_negative_origins() {
        let displays = [
            (
                WindowBounds {
                    x: -1000.0,
                    y: 0.0,
                    width: 1000.0,
                    height: 1000.0,
                },
                Some(1.0),
            ),
            (
                WindowBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 1000.0,
                    height: 1000.0,
                },
                Some(2.0),
            ),
        ];
        let mut bounds = WindowBounds {
            x: -400.0,
            y: 100.0,
            width: 500.0,
            height: 500.0,
        };
        assert_eq!(scale_for_displays(&bounds, &displays), Some(1.0));
        bounds.x = -100.0;
        assert_eq!(scale_for_displays(&bounds, &displays), Some(2.0));
        bounds.x = -250.0;
        assert_eq!(scale_for_displays(&bounds, &displays), None);
        bounds.x = 2000.0;
        assert_eq!(scale_for_displays(&bounds, &displays), None);
        bounds.x = 100.0;
        assert_eq!(
            scale_for_displays(&bounds, &[(displays[1].0.clone(), None)]),
            None
        );
    }

    fn frame(scale: f64) -> WindowPxFrame {
        WindowPxFrame {
            bounds: WindowBounds {
                x: 100.0,
                y: 580.0,
                width: 500.0,
                height: 500.0,
            },
            scale,
        }
    }

    #[test]
    fn resolves_1x_window_local_to_screen() {
        let (sx, sy, lx, ly) = frame(1.0).to_screen(30.0, 40.0);
        assert_eq!((sx, sy), (130.0, 620.0));
        assert_eq!((lx, ly), (30.0, 40.0));
    }

    #[test]
    fn resolves_2x_retina_scale() {
        // A point 60 capture px in is 30 pt in.
        let (sx, sy, lx, ly) = frame(2.0).to_screen(60.0, 80.0);
        assert_eq!((sx, sy), (130.0, 620.0));
        assert_eq!((lx, ly), (30.0, 40.0));
    }

    #[test]
    fn backing_scale_detects_retina_capture() {
        let b = frame(1.0).bounds;
        assert_eq!(validate_capture_frame(11, &b, 1000, 1000).unwrap(), 2.0);
    }

    #[test]
    fn backing_scale_is_one_for_point_sized_capture() {
        let b = frame(1.0).bounds;
        assert_eq!(validate_capture_frame(11, &b, 500, 500).unwrap(), 1.0);
    }

    #[test]
    fn backing_scale_never_divides_by_zero_bounds() {
        // windows.rs defaults absent kCGWindowBounds to 0×0.
        let mut b = frame(1.0).bounds;
        b.width = 0.0;
        assert_eq!(
            validate_capture_frame(11, &b, 1000, 1000),
            Err(PxFrameError::WindowNotFound { window_id: 11 })
        );
    }

    #[test]
    fn capture_narrower_than_bounds_is_a_mismatch() {
        let b = frame(1.0).bounds;
        assert!(matches!(
            validate_capture_frame(11, &b, 250, 250),
            Err(PxFrameError::FrameMismatch { .. })
        ));
    }

    #[test]
    fn parent_capture_cannot_masquerade_as_a_panel_scale() {
        let bounds = WindowBounds {
            x: 72.0,
            y: 88.0,
            width: 880.0,
            height: 448.0,
        };
        assert!(matches!(
            validate_capture_frame(55, &bounds, 1920, 960),
            Err(PxFrameError::FrameMismatch { .. })
        ));
    }

    /// Locks the exact regression: a stale window_id used to fall through to
    /// "treat x,y as screen coordinates".
    #[test]
    fn missing_window_refuses_instead_of_treating_local_as_screen_px() {
        let refused = refusal(&PxFrameError::WindowNotFound { window_id: 67340 });
        assert_eq!(refused.is_error, Some(true));
        let structured = refused.structured_content.expect("structured refusal");
        assert_eq!(structured["code"], "px_window_not_found");
        assert_eq!(structured["window_id"], 67340);
        assert!(
            structured["suggestion"]
                .as_str()
                .unwrap()
                .contains("list_windows"),
            "the refusal must name the retry path"
        );
    }
}
