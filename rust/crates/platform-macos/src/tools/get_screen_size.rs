use async_trait::async_trait;
use cua_driver_contract::GetScreenSizeInput;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_input,
};
use serde_json::Value;

pub struct GetScreenSizeTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        // Matches `GetScreenSizeTool.swift` description verbatim.
        name: "get_screen_size".into(),
        description: "Return the logical size of the main display in points plus its backing \
            scale factor. Agents click in points; Retina displays have scale_factor 2.0. \
            Requires no TCC permissions."
            .into(),
        input_schema: serde_json::json!({"type":"object","properties":{
            "session": cua_driver_core::tool_schema::session_schema()
        },"additionalProperties":false}),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for GetScreenSizeTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if let Err(result) = parse_typed_input::<GetScreenSizeInput>("get_screen_size", args) {
            return result;
        }
        screen_size_result(main_screen_size())
    }
}

fn screen_size_result(size: Option<(i64, i64, f64)>) -> ToolResult {
    match size {
        Some((w, h, scale)) => ToolResult::text(format!(
            "✅ Main display: {w}x{h} points @ {scale}x"
        ))
        .with_structured(serde_json::json!({
            "width": w, "height": h, "scale_factor": scale,
        })),
        None => ToolResult::error("Main display or its backing scale is unavailable."),
    }
}

fn size_from_bounds(w: i64, h: i64, scale: Option<f64>) -> Option<(i64, i64, f64)> {
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h, scale?))
}

/// Returns `(width_points, height_points, backing_scale_factor)` from
/// CoreGraphics — safe to call from any thread (no AppKit main-thread requirement).
///
/// The previous NSScreen-based implementation required `MainThreadMarker::new()`
/// which always returns `None` on async tokio threads, causing the tool to
/// return an error even when a display is attached.
pub(crate) fn main_screen_size() -> Option<(i64, i64, f64)> {
    use core_graphics::display::{CGDisplayBounds, CGMainDisplayID};

    // SAFETY: CGMainDisplayID / CGDisplayBounds are thread-safe CG APIs.
    let display_id = unsafe { CGMainDisplayID() };
    if display_id == 0 {
        return None;
    }
    let bounds = unsafe { CGDisplayBounds(display_id) };
    let w = bounds.size.width as i64;
    let h = bounds.size.height as i64;
    size_from_bounds(w, h, get_backing_scale(display_id))
}

/// Return the current display mode's backing-pixel to point ratio.
pub(crate) fn get_backing_scale(display_id: u32) -> Option<f64> {
    use core_graphics::display::CGDisplay;

    let mode = CGDisplay::new(display_id)
        .display_mode()
        .map(|mode| (mode.width(), mode.pixel_width()));
    backing_scale_from_mode(mode)
}

fn backing_scale_from_mode(mode: Option<(u64, u64)>) -> Option<f64> {
    let (point_width, pixel_width) = mode?;
    if point_width == 0 || pixel_width == 0 {
        return None;
    }
    let ratio = pixel_width as f64 / point_width as f64;
    // Round to nearest 0.5 to avoid floating point noise.
    let scale = (ratio * 2.0).round() / 2.0;
    (scale > 0.0).then_some(scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics::display::CGDisplay;

    #[test]
    #[ignore = "requires an attached macOS display; run explicitly on a native Retina host"]
    fn main_display_scale_matches_current_mode() {
        let display = CGDisplay::main();
        let mode = display
            .display_mode()
            .expect("main display should expose its current mode");
        let point_width = mode.width();
        let pixel_width = mode.pixel_width();
        assert!(point_width > 0, "display mode should have a point width");
        assert!(pixel_width > 0, "display mode should have a pixel width");
        assert!(
            pixel_width > point_width,
            "native Retina validation requires more backing pixels than points; got {pixel_width}px for {point_width}pt"
        );

        let expected = ((pixel_width as f64 / point_width as f64) * 2.0).round() / 2.0;
        let actual = get_backing_scale(display.id);

        assert_eq!(
            actual,
            Some(expected),
            "backing scale should use the current mode's pixel and point widths"
        );
    }

    #[test]
    fn display_mode_widths_preserve_rounding_and_unknowns() {
        assert_eq!(backing_scale_from_mode(Some((1920, 3840))), Some(2.0));
        assert_eq!(backing_scale_from_mode(Some((1920, 2880))), Some(1.5));
        assert_eq!(backing_scale_from_mode(Some((0, 3840))), None);
        assert_eq!(backing_scale_from_mode(Some((1920, 0))), None);
    }
}
#[cfg(test)]
mod scale_tests {
    use super::{backing_scale_from_mode, screen_size_result, size_from_bounds};

    #[test]
    fn a_missing_display_mode_is_unknown_scale() {
        assert_eq!(backing_scale_from_mode(None), None);
    }

    #[test]
    fn zero_widths_are_unknown_scale() {
        assert_eq!(backing_scale_from_mode(Some((0, 3840))), None);
        assert_eq!(backing_scale_from_mode(Some((1920, 0))), None);
    }

    #[test]
    fn valid_modes_keep_the_existing_rounding() {
        assert_eq!(backing_scale_from_mode(Some((1920, 1920))), Some(1.0));
        assert_eq!(backing_scale_from_mode(Some((1920, 2880))), Some(1.5));
        assert_eq!(backing_scale_from_mode(Some((1920, 3840))), Some(2.0));
        assert_eq!(backing_scale_from_mode(Some((1000, 1499))), Some(1.5));
    }

    #[test]
    fn unknown_scale_produces_a_screen_size_error_and_success_keeps_its_shape() {
        let unknown = size_from_bounds(1920, 1080, None);
        assert_eq!(unknown, None);
        assert_eq!(screen_size_result(unknown).is_error, Some(true));
        let result = screen_size_result(size_from_bounds(1920, 1080, Some(2.0)));
        assert_ne!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({
                "width": 1920, "height": 1080, "scale_factor": 2.0
            }))
        );
    }

    #[test]
    fn overlay_retains_its_last_fit_when_the_new_scale_is_unknown() {
        use crate::cursor::overlay::{geometry_from_frame, initial_overlay_scale};
        use cursor_overlay::{ScreenFrame, SurfaceFit, SurfaceGeometry};
        use std::time::Instant;

        assert_eq!(
            initial_overlay_scale(2.0, || panic!("AppKit scale wins")),
            Some(2.0)
        );
        assert_eq!(initial_overlay_scale(0.0, || Some(1.5)), Some(1.5));
        assert_eq!(initial_overlay_scale(f64::NAN, || None), None);

        let initial = SurfaceGeometry::new(ScreenFrame::new(0.0, 0.0, 100.0, 100.0), 2.0);
        let changed = ScreenFrame::new(0.0, 0.0, 200.0, 100.0);
        let mut fit = SurfaceFit::new(Some(initial));
        let unknown = geometry_from_frame(fit.applied(), changed, || None);
        assert_eq!(fit.observe(Instant::now(), unknown), None);
        assert_eq!(fit.applied(), Some(initial));
        let recovered = geometry_from_frame(fit.applied(), changed, || Some(1.5));
        assert_eq!(fit.observe(Instant::now(), recovered), recovered);
        assert_eq!(fit.applied(), recovered);
    }
}
