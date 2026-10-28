//! Pointer coordinates in window points (GOAT hang lane §3.2).
//!
//! By default a pointer tool's x, y are pixels of the session's latest
//! screenshot of the window, scaled back through that screenshot. With
//! coordinate_space "window_points" they are points from the window's
//! top-left, independent of any screenshot, so a point read before a driver
//! restart or after other input still means the same place. The caller sends
//! the window frame it read the point against (window_basis); when the live
//! frame differs by more than 1 pt in origin or size the call is refused,
//! nothing sent, because the point may now name different content.

use cua_driver_core::protocol::ToolResult;
use serde_json::{json, Value};

use crate::windows::WindowBounds;

/// Frames are fractional points; 1 pt absorbs rounding, a real move or
/// resize is several points.
pub const BASIS_TOLERANCE_PT: f64 = 1.0;

/// The window frame, in screen points, a window-points coordinate was read
/// against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowBasis {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// How a pointer tool's x, y are measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointSpace {
    Screenshot,
    WindowPoints(WindowBasis),
}

/// The coordinate_space property for a pointer tool's input schema.
pub fn coordinate_space_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["screenshot", "window_points"],
        "description": "\"screenshot\" (default): x, y are pixels of the latest screenshot of the window. \"window_points\": x, y are points from the window's top-left; requires window_id and window_basis."
    })
}

/// The window_basis property for a pointer tool's input schema.
pub fn window_basis_schema() -> Value {
    json!({
        "type": "object",
        "description": "With coordinate_space \"window_points\": the window frame {x, y, w, h} in screen points the point was read against. The call is refused, nothing sent, when the live frame differs by more than 1 pt.",
        "properties": {
            "x": { "type": "number" },
            "y": { "type": "number" },
            "w": { "type": "number" },
            "h": { "type": "number" }
        },
        "required": ["x", "y", "w", "h"],
        "additionalProperties": false
    })
}

/// Reads coordinate_space and window_basis from a pointer tool's arguments.
pub fn point_space(args: &Value) -> Result<PointSpace, ToolResult> {
    match args.get("coordinate_space").and_then(Value::as_str) {
        None | Some("screenshot") => {
            if args
                .get("window_basis")
                .is_some_and(|basis| !basis.is_null())
            {
                return Err(super::invalid_arguments(
                    "window_basis applies only with coordinate_space \"window_points\".",
                ));
            }
            Ok(PointSpace::Screenshot)
        }
        Some("window_points") => {
            let basis = args.get("window_basis").and_then(Value::as_object);
            let number = |key: &str| {
                basis
                    .and_then(|basis| basis.get(key))
                    .and_then(Value::as_f64)
                    .filter(|value| value.is_finite())
            };
            match (number("x"), number("y"), number("w"), number("h")) {
                (Some(x), Some(y), Some(w), Some(h)) if w > 0.0 && h > 0.0 => {
                    Ok(PointSpace::WindowPoints(WindowBasis { x, y, w, h }))
                }
                _ => Err(super::invalid_arguments(
                    "coordinate_space \"window_points\" needs window_basis {x, y, w, h}: the \
                     window frame in screen points the point was read against (list_windows \
                     bounds), with positive w and h.",
                )),
            }
        }
        Some(other) => Err(super::invalid_arguments(format!(
            "Unknown coordinate_space \"{other}\" — expected \"screenshot\" or \"window_points\"."
        ))),
    }
}

/// Whether the live frame is still the one the point was read against.
pub fn basis_matches(basis: &WindowBasis, live: &WindowBounds) -> bool {
    (basis.x - live.x).abs() <= BASIS_TOLERANCE_PT
        && (basis.y - live.y).abs() <= BASIS_TOLERANCE_PT
        && (basis.w - live.width).abs() <= BASIS_TOLERANCE_PT
        && (basis.h - live.height).abs() <= BASIS_TOLERANCE_PT
}

/// The refusal when the window moved or resized since the point was read.
pub fn geometry_changed(window_id: u32, basis: &WindowBasis, live: &WindowBounds) -> ToolResult {
    let mut refusal = super::pre_dispatch_refusal(
        "window_geometry_changed",
        format!(
            "Window {window_id} moved or resized since the point was read: basis \
             ({:.0}, {:.0}) {:.0}×{:.0}, now ({:.0}, {:.0}) {:.0}×{:.0}. Nothing was sent. Read \
             the window again (list_windows or get_window_state) and recompute the point.",
            basis.x, basis.y, basis.w, basis.h, live.x, live.y, live.width, live.height
        ),
    );
    if let Some(structured) = refusal.structured_content.as_mut() {
        structured["nothing_sent"] = json!(true);
        structured["window_basis"] =
            json!({"x": basis.x, "y": basis.y, "w": basis.w, "h": basis.h});
        structured["window_frame"] =
            json!({"x": live.x, "y": live.y, "w": live.width, "h": live.height});
    }
    refusal
}

/// Window-local capture pixels for window points, given the live frame and
/// its backing scale: what the pixel tools hand to WindowPxFrame::to_screen,
/// which divides the scale back out, so the screen point is the frame origin
/// plus (x, y) at any backing scale.
pub fn points_to_window_pixels(
    window_id: u32,
    basis: &WindowBasis,
    live: &WindowBounds,
    scale: f64,
    points: &[(f64, f64)],
) -> Result<Vec<(f64, f64)>, ToolResult> {
    if !basis_matches(basis, live) {
        return Err(geometry_changed(window_id, basis, live));
    }
    Ok(points
        .iter()
        .map(|&(x, y)| (x * scale, y * scale))
        .collect())
}

/// The window-local capture pixels for points in space: screenshot pixels
/// scaled back through the session's screenshot, or window points checked
/// against the live frame.
pub(super) async fn to_window_pixels(
    state: &super::ToolState,
    args: &Value,
    pid: i32,
    window_id: Option<u32>,
    space: PointSpace,
    points: &[(f64, f64)],
) -> Result<Vec<(f64, f64)>, ToolResult> {
    match space {
        PointSpace::Screenshot => {
            let ratio = super::screenshot_scale(state, args, pid, window_id)?;
            Ok(points
                .iter()
                .map(|&(x, y)| (x * ratio, y * ratio))
                .collect())
        }
        PointSpace::WindowPoints(basis) => {
            let Some(wid) = window_id else {
                return Err(super::invalid_arguments(
                    "coordinate_space \"window_points\" needs window_id.",
                ));
            };
            let frame = super::px_frame::resolve_or_refuse(wid).await?;
            points_to_window_pixels(wid, &basis, &frame.bounds, frame.scale, points)
        }
    }
}

/// Adds the point the call used, in window points, to a result: as a
/// sentence of its text (an action's structured content is the closed
/// ActionResult contract, which carries the text as its summary) and as a
/// structured "point" where the result is not projected (refusals).
pub fn annotate(
    mut result: ToolResult,
    space: PointSpace,
    point: Option<(f64, f64)>,
) -> ToolResult {
    let (PointSpace::WindowPoints(_), Some((x, y))) = (space, point) else {
        return result;
    };
    let sentence = format!(" Point: ({x:.1}, {y:.1}) in window points.");
    match result.content.iter_mut().find_map(|content| match content {
        cua_driver_core::protocol::Content::Text { text, .. } => Some(text),
        _ => None,
    }) {
        Some(text) => text.push_str(&sentence),
        None => result
            .content
            .push(cua_driver_core::protocol::Content::text(
                sentence.trim_start(),
            )),
    }
    let structured = result.structured_content.get_or_insert_with(|| json!({}));
    if let Some(object) = structured.as_object_mut() {
        object.insert(
            "point".into(),
            json!({"x": x, "y": y, "unit": "window_points"}),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: f64, y: f64, width: f64, height: f64) -> WindowBounds {
        WindowBounds {
            x,
            y,
            width,
            height,
        }
    }

    fn basis(x: f64, y: f64, w: f64, h: f64) -> WindowBasis {
        WindowBasis { x, y, w, h }
    }

    #[test]
    fn window_points_land_on_the_frame_origin_plus_the_point_at_any_scale() {
        for (origin, scale) in [
            ((0.0, 0.0), 1.0),
            ((1051.0, 75.0), 2.0),
            ((-1200.0, 40.0), 2.0),
            ((-1200.0, 40.0), 1.0),
        ] {
            let live = bounds(origin.0, origin.1, 800.0, 600.0);
            let pixels = points_to_window_pixels(
                7,
                &basis(origin.0, origin.1, 800.0, 600.0),
                &live,
                scale,
                &[(100.0, 50.0)],
            )
            .unwrap();
            let frame = super::super::px_frame::WindowPxFrame {
                bounds: live,
                scale,
            };
            let (sx, sy, lx, ly) = frame.to_screen(pixels[0].0, pixels[0].1);
            assert_eq!(
                (sx, sy),
                (origin.0 + 100.0, origin.1 + 50.0),
                "scale {scale}"
            );
            assert_eq!((lx, ly), (100.0, 50.0));
        }
    }

    #[test]
    fn a_moved_or_resized_window_refuses_and_one_point_of_rounding_does_not() {
        let live = bounds(100.0, 200.0, 800.0, 600.0);
        assert!(basis_matches(&basis(100.0, 200.0, 800.0, 600.0), &live));
        assert!(basis_matches(&basis(101.0, 199.0, 799.0, 601.0), &live));
        for stale in [
            basis(105.0, 200.0, 800.0, 600.0),
            basis(100.0, 190.0, 800.0, 600.0),
            basis(100.0, 200.0, 820.0, 600.0),
            basis(100.0, 200.0, 800.0, 560.0),
        ] {
            let refusal =
                points_to_window_pixels(7, &stale, &live, 2.0, &[(1.0, 1.0)]).unwrap_err();
            let structured = refusal.structured_content.expect("structured");
            assert_eq!(structured["code"], "window_geometry_changed");
            assert_eq!(structured["nothing_sent"], true);
            assert_eq!(structured["effect"], "refused");
        }
    }

    #[test]
    fn both_drag_ends_use_one_basis_check() {
        let live = bounds(0.0, 0.0, 800.0, 600.0);
        assert!(points_to_window_pixels(
            7,
            &basis(30.0, 0.0, 800.0, 600.0),
            &live,
            2.0,
            &[(10.0, 10.0), (50.0, 50.0)]
        )
        .is_err());
        assert_eq!(
            points_to_window_pixels(
                7,
                &basis(0.0, 0.0, 800.0, 600.0),
                &live,
                2.0,
                &[(10.0, 10.0), (50.0, 50.0)]
            )
            .unwrap(),
            vec![(20.0, 20.0), (100.0, 100.0)]
        );
    }

    #[test]
    fn the_coordinate_space_argument_is_checked() {
        assert_eq!(point_space(&json!({})).unwrap(), PointSpace::Screenshot);
        assert_eq!(
            point_space(&json!({"coordinate_space": "screenshot"})).unwrap(),
            PointSpace::Screenshot
        );
        assert_eq!(
            point_space(&json!({
                "coordinate_space": "window_points",
                "window_basis": {"x": 1.5, "y": -3, "w": 800, "h": 600}
            }))
            .unwrap(),
            PointSpace::WindowPoints(basis(1.5, -3.0, 800.0, 600.0))
        );
        for bad in [
            json!({"coordinate_space": "window_points"}),
            json!({"coordinate_space": "window_points", "window_basis": {"x": 0, "y": 0, "w": 0, "h": 600}}),
            json!({"coordinate_space": "window_points", "window_basis": {"x": 0, "y": 0, "w": 800}}),
            json!({"coordinate_space": "pixels"}),
            json!({"window_basis": {"x": 0, "y": 0, "w": 800, "h": 600}}),
        ] {
            let refusal = point_space(&bad).unwrap_err();
            assert_eq!(
                refusal.structured_content.unwrap()["code"],
                "invalid_arguments",
                "{bad}"
            );
        }
    }

    #[test]
    fn a_window_points_result_carries_its_point() {
        let space = PointSpace::WindowPoints(basis(0.0, 0.0, 10.0, 10.0));
        let result = annotate(ToolResult::text("ok"), space, Some((3.0, 4.0)));
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(
            text.contains("ok Point: (3.0, 4.0) in window points."),
            "{text}"
        );
        assert_eq!(
            result.structured_content.unwrap()["point"],
            json!({"x": 3.0, "y": 4.0, "unit": "window_points"})
        );
        let plain = annotate(
            ToolResult::text("ok"),
            PointSpace::Screenshot,
            Some((3.0, 4.0)),
        );
        assert!(plain.structured_content.is_none());
    }
}
