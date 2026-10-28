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
            if args.get("scope").and_then(Value::as_str) == Some("desktop")
                || args.get("window_id").is_none_or(Value::is_null)
                || args
                    .get("element_token")
                    .is_some_and(|token| !token.is_null())
            {
                return Err(super::invalid_arguments(
                    "coordinate_space window_points needs a window target (pid and window_id); it cannot be \
                     used with scope desktop.",
                ));
            }
            let has = |key: &str| args.get(key).and_then(Value::as_f64).is_some();
            if !((has("x") && has("y"))
                || (has("from_x") && has("from_y") && has("to_x") && has("to_y")))
            {
                return Err(super::invalid_arguments(
                    "coordinate_space window_points needs the point: x and y (from_x, from_y, \
                     to_x and to_y for drag).",
                ));
            }
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

/// Points resolved for dispatch: the window-local pixels the tool's frame
/// translates, and for window points the one validated frame to translate
/// them with (scale 1: the pixels are the points themselves).
#[derive(Debug, Clone)]
pub struct ResolvedPoints {
    pub pixels: Vec<(f64, f64)>,
    pub frame: Option<super::px_frame::WindowPxFrame>,
}

/// Window points against the live frame: the basis must still match it and
/// every point must lie inside it. Only window bounds are read, so neither
/// a screenshot nor a pixel-scale proof is needed.
pub fn resolve_window_points(
    window_id: u32,
    basis: &WindowBasis,
    points: &[(f64, f64)],
    live: Option<WindowBounds>,
) -> Result<ResolvedPoints, ToolResult> {
    let live = live.ok_or_else(|| {
        super::px_frame::refusal(&super::px_frame::PxFrameError::WindowNotFound { window_id })
    })?;
    if !basis_matches(basis, &live) {
        return Err(geometry_changed(window_id, basis, &live));
    }
    if let Some(&(x, y)) = points.iter().find(|&&(x, y)| {
        !(x.is_finite() && y.is_finite()) || x < 0.0 || y < 0.0 || x > live.width || y > live.height
    }) {
        return Err(super::pre_dispatch_refusal(
            "point_outside_window",
            format!(
                "({x:.1}, {y:.1}) is outside window {window_id}'s {:.0}×{:.0} pt frame. \
                 Nothing was sent.",
                live.width, live.height
            ),
        ));
    }
    Ok(ResolvedPoints {
        pixels: points.to_vec(),
        frame: Some(super::px_frame::WindowPxFrame {
            bounds: live,
            scale: 1.0,
        }),
    })
}

/// Checks, just before an actuator, that the window is still exactly the
/// frame the points were translated with. Comparing with that frame rather
/// than the basis keeps a 1 pt drift on each side of the basis from adding
/// up to 2 pt between translation and dispatch.
pub fn recheck(
    space: PointSpace,
    window_id: u32,
    used: Option<&super::px_frame::WindowPxFrame>,
    live: Option<WindowBounds>,
) -> Result<(), ToolResult> {
    let PointSpace::WindowPoints(basis) = space else {
        return Ok(());
    };
    let Some(used) = used else {
        return Err(super::pre_dispatch_refusal(
            "window_points_unresolved",
            "The window points were not resolved against the window's frame for this route, \
             so nothing was sent.",
        ));
    };
    let live = live.ok_or_else(|| {
        super::px_frame::refusal(&super::px_frame::PxFrameError::WindowNotFound { window_id })
    })?;
    let same = |a: f64, b: f64| (a - b).abs() < SAME_FRAME_PT;
    if same(used.bounds.x, live.x)
        && same(used.bounds.y, live.y)
        && same(used.bounds.width, live.width)
        && same(used.bounds.height, live.height)
    {
        Ok(())
    } else {
        Err(geometry_changed(window_id, &basis, &live))
    }
}

/// Two reads of an unmoved window's frame agree to far better than this.
const SAME_FRAME_PT: f64 = 0.01;

/// What a dispatch closure returns when its guard finds the window moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowMoved {
    pub window_id: u32,
}

impl std::fmt::Display for WindowMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "window {} moved before the input was posted",
            self.window_id
        )
    }
}

impl std::error::Error for WindowMoved {}

/// The window-points recheck carried onto the actuator's own thread and run
/// right before the native post, after all preparatory work.
#[derive(Clone)]
pub struct DispatchGuard {
    window_id: u32,
    frame: super::px_frame::WindowPxFrame,
    read: fn(u32) -> Option<WindowBounds>,
}

impl DispatchGuard {
    /// None for screenshot pixels. For window points the validated frame must
    /// be present: an actuator without it fails closed.
    pub fn new(
        space: PointSpace,
        window_id: Option<u32>,
        frame: Option<super::px_frame::WindowPxFrame>,
    ) -> Result<Option<Self>, ToolResult> {
        Self::with_reader(space, window_id, frame, crate::windows::window_bounds_by_id)
    }

    fn with_reader(
        space: PointSpace,
        window_id: Option<u32>,
        frame: Option<super::px_frame::WindowPxFrame>,
        read: fn(u32) -> Option<WindowBounds>,
    ) -> Result<Option<Self>, ToolResult> {
        if space == PointSpace::Screenshot {
            return Ok(None);
        }
        match (window_id, frame) {
            (Some(window_id), Some(frame)) => Ok(Some(Self {
                window_id,
                frame,
                read,
            })),
            _ => Err(super::pre_dispatch_refusal(
                "window_points_unresolved",
                "The window points were not resolved against the window's frame for this \
                 route, so nothing was sent.",
            )),
        }
    }

    /// The window is still exactly the frame the points were translated with.
    pub fn check(&self) -> Result<(), WindowMoved> {
        let same = |a: f64, b: f64| (a - b).abs() < SAME_FRAME_PT;
        match (self.read)(self.window_id) {
            Some(live)
                if same(self.frame.bounds.x, live.x)
                    && same(self.frame.bounds.y, live.y)
                    && same(self.frame.bounds.width, live.width)
                    && same(self.frame.bounds.height, live.height) =>
            {
                Ok(())
            }
            _ => Err(WindowMoved {
                window_id: self.window_id,
            }),
        }
    }
}

/// Runs the guard when there is one.
pub fn check_guard(guard: &Option<DispatchGuard>) -> Result<(), WindowMoved> {
    guard.as_ref().map_or(Ok(()), DispatchGuard::check)
}

/// The guard's refusal when a dispatch closure stopped on it.
pub fn moved_refusal(error: &anyhow::Error, space: PointSpace) -> Option<ToolResult> {
    let moved = error.downcast_ref::<WindowMoved>()?;
    let PointSpace::WindowPoints(basis) = space else {
        return None;
    };
    Some(match crate::windows::window_bounds_by_id(moved.window_id) {
        Some(live) => geometry_changed(moved.window_id, &basis, &live),
        None => super::px_frame::refusal(&super::px_frame::PxFrameError::WindowNotFound {
            window_id: moved.window_id,
        }),
    })
}

/// [recheck] against the window's frame now.
pub(super) fn recheck_now(
    space: PointSpace,
    window_id: Option<u32>,
    used: Option<&super::px_frame::WindowPxFrame>,
) -> Result<(), ToolResult> {
    match window_id {
        Some(wid) if matches!(space, PointSpace::WindowPoints(_)) => {
            recheck(space, wid, used, crate::windows::window_bounds_by_id(wid))
        }
        _ => Ok(()),
    }
}

/// The points in space as window-local pixels: screenshot pixels scaled
/// back through the session's screenshot (the frame is resolved later as
/// before), or window points against the live frame (that frame returned).
pub(super) async fn to_window_pixels(
    state: &super::ToolState,
    args: &Value,
    pid: i32,
    window_id: Option<u32>,
    space: PointSpace,
    points: &[(f64, f64)],
) -> Result<ResolvedPoints, ToolResult> {
    match space {
        PointSpace::Screenshot => {
            let ratio = super::screenshot_scale(state, args, pid, window_id)?;
            Ok(ResolvedPoints {
                pixels: points
                    .iter()
                    .map(|&(x, y)| (x * ratio, y * ratio))
                    .collect(),
                frame: None,
            })
        }
        PointSpace::WindowPoints(basis) => {
            let Some(wid) = window_id else {
                return Err(super::invalid_arguments(
                    "coordinate_space window_points needs window_id.",
                ));
            };
            resolve_window_points(
                wid,
                &basis,
                points,
                crate::windows::window_bounds_by_id(wid),
            )
        }
    }
}

/// The validated window-points frame, or the window's pixel frame resolved
/// as before for screenshot pixels.
pub(super) async fn frame_or_resolve(
    validated: Option<super::px_frame::WindowPxFrame>,
    window_id: u32,
) -> Result<super::px_frame::WindowPxFrame, ToolResult> {
    match validated {
        Some(frame) => Ok(frame),
        None => super::px_frame::resolve_or_refuse(window_id).await,
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
    fn window_points_land_on_the_frame_origin_plus_the_point() {
        for origin in [(0.0, 0.0), (1051.0, 75.0), (-1200.0, 40.0)] {
            let live = bounds(origin.0, origin.1, 800.0, 600.0);
            let resolved = resolve_window_points(
                7,
                &basis(origin.0, origin.1, 800.0, 600.0),
                &[(100.0, 50.0)],
                Some(live),
            )
            .unwrap();
            let frame = resolved.frame.expect("the validated frame");
            let (sx, sy, lx, ly) = frame.to_screen(resolved.pixels[0].0, resolved.pixels[0].1);
            assert_eq!((sx, sy), (origin.0 + 100.0, origin.1 + 50.0));
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
                resolve_window_points(7, &stale, &[(1.0, 1.0)], Some(live.clone())).unwrap_err();
            let structured = refusal.structured_content.expect("structured");
            assert_eq!(structured["code"], "window_geometry_changed");
            assert_eq!(structured["nothing_sent"], true);
            assert_eq!(structured["effect"], "refused");
        }
        let gone = resolve_window_points(7, &basis(0.0, 0.0, 1.0, 1.0), &[(0.0, 0.0)], None);
        assert_eq!(gone.unwrap_err().is_error, Some(true));
    }

    #[test]
    fn a_point_outside_the_window_refuses_including_either_drag_end() {
        let live = bounds(0.0, 0.0, 800.0, 600.0);
        let b = basis(0.0, 0.0, 800.0, 600.0);
        for points in [
            vec![(801.0, 10.0)],
            vec![(10.0, -1.0)],
            vec![(10.0, 10.0), (900.0, 10.0)],
            vec![(f64::NAN, 10.0)],
        ] {
            let refusal = resolve_window_points(7, &b, &points, Some(live.clone())).unwrap_err();
            assert_eq!(
                refusal.structured_content.unwrap()["code"],
                "point_outside_window",
                "{points:?}"
            );
        }
        let both =
            resolve_window_points(7, &b, &[(10.0, 10.0), (800.0, 600.0)], Some(live)).unwrap();
        assert_eq!(both.pixels, vec![(10.0, 10.0), (800.0, 600.0)]);
    }

    #[test]
    fn a_move_between_resolving_and_dispatch_refuses() {
        let b = basis(0.0, 0.0, 800.0, 600.0);
        let space = PointSpace::WindowPoints(b);
        // Translated with a frame 1 pt left of the basis; at dispatch it is
        // 1 pt right of it. Each is within the basis tolerance, but the two
        // differ by 2 pt, so the dispatch is refused.
        let resolved =
            resolve_window_points(7, &b, &[(1.0, 1.0)], Some(bounds(-1.0, 0.0, 800.0, 600.0)))
                .unwrap();
        let used = resolved.frame.as_ref();
        let refusal = recheck(space, 7, used, Some(bounds(1.0, 0.0, 800.0, 600.0))).unwrap_err();
        assert_eq!(
            refusal.structured_content.unwrap()["code"],
            "window_geometry_changed"
        );
        assert!(recheck(space, 7, used, Some(bounds(-1.0, 0.0, 800.0, 600.0))).is_ok());
        assert!(recheck(space, 7, used, Some(bounds(-1.0, 0.0, 801.0, 600.0))).is_err());
        assert!(
            recheck(space, 7, used, None).is_err(),
            "a closed window refuses"
        );
        assert!(
            recheck(space, 7, None, Some(bounds(-1.0, 0.0, 800.0, 600.0))).is_err(),
            "no frame fails closed"
        );
        assert!(recheck(PointSpace::Screenshot, 7, None, None).is_ok());
    }

    fn moved_reader(_: u32) -> Option<WindowBounds> {
        Some(bounds(30.0, 0.0, 800.0, 600.0))
    }
    fn still_reader(_: u32) -> Option<WindowBounds> {
        Some(bounds(0.0, 0.0, 800.0, 600.0))
    }

    #[test]
    fn the_dispatch_guard_stops_a_post_after_a_move_during_preparation() {
        let b = basis(0.0, 0.0, 800.0, 600.0);
        let space = PointSpace::WindowPoints(b);
        let resolved =
            resolve_window_points(7, &b, &[(1.0, 1.0)], Some(bounds(0.0, 0.0, 800.0, 600.0)))
                .unwrap();
        let moved =
            DispatchGuard::with_reader(space, Some(7), resolved.frame.clone(), moved_reader)
                .unwrap()
                .expect("a guard for window points");
        assert_eq!(moved.check(), Err(WindowMoved { window_id: 7 }));
        let still = DispatchGuard::with_reader(space, Some(7), resolved.frame, still_reader)
            .unwrap()
            .unwrap();
        assert_eq!(still.check(), Ok(()));
        // An actuator without the validated frame fails closed.
        let refusal = DispatchGuard::with_reader(space, Some(7), None, still_reader)
            .err()
            .expect("refused");
        assert_eq!(
            refusal.structured_content.unwrap()["code"],
            "window_points_unresolved"
        );
        assert!(DispatchGuard::new(PointSpace::Screenshot, Some(7), None)
            .unwrap()
            .is_none());
        // The error a closure returns is recognized as the guard's.
        let error = anyhow::Error::from(WindowMoved { window_id: 7 });
        assert!(error.downcast_ref::<WindowMoved>().is_some());
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
                "window_id": 9, "x": 1, "y": 2,
                "coordinate_space": "window_points",
                "window_basis": {"x": 1.5, "y": -3, "w": 800, "h": 600}
            }))
            .unwrap(),
            PointSpace::WindowPoints(basis(1.5, -3.0, 800.0, 600.0))
        );
        for bad in [
            json!({"window_id": 9, "x": 1, "coordinate_space": "window_points",
                "window_basis": {"x": 0, "y": 0, "w": 800, "h": 600}}),
            json!({"window_id": 9, "element_token": "s00000001:3", "coordinate_space": "window_points",
                "window_basis": {"x": 0, "y": 0, "w": 800, "h": 600}}),
            json!({"window_id": 9, "coordinate_space": "window_points"}),
            json!({"scope": "desktop", "window_id": 9, "coordinate_space": "window_points",
                "window_basis": {"x": 0, "y": 0, "w": 800, "h": 600}}),
            json!({"coordinate_space": "window_points",
                "window_basis": {"x": 0, "y": 0, "w": 800, "h": 600}}),
            json!({"window_id": 9, "coordinate_space": "window_points", "window_basis": {"x": 0, "y": 0, "w": 0, "h": 600}}),
            json!({"window_id": 9, "coordinate_space": "window_points", "window_basis": {"x": 0, "y": 0, "w": 800}}),
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

    /// Every pointer tool refuses window points at desktop scope before any
    /// dispatch: there is no window frame to anchor them.
    #[tokio::test]
    async fn desktop_scope_window_points_refuse_before_dispatch() {
        use cua_driver_core::tool::Tool;
        use std::sync::Arc;
        let state = Arc::new(super::super::ToolState::new(false, false, None));
        let space = json!({
            "scope": "desktop",
            "coordinate_space": "window_points",
            "window_basis": {"x": 0, "y": 0, "w": 800, "h": 600}
        });
        let with = |extra: Value| {
            let mut args = space.clone();
            for (key, value) in extra.as_object().unwrap() {
                args[key] = value.clone();
            }
            args
        };
        let results = vec![
            super::super::click::ClickTool::new(state.clone())
                .invoke(with(json!({"x": 10, "y": 10})))
                .await,
            super::super::double_click::DoubleClickTool::new(state.clone())
                .invoke(with(json!({"x": 10, "y": 10, "pid": 1, "window_id": 1})))
                .await,
            super::super::double_click::DoubleClickTool::new(state.clone())
                .invoke(
                    json!({"pid": 1, "window_id": 1, "element_token": "s00000001:1",
                    "coordinate_space": "window_points",
                    "window_basis": {"x": 0, "y": 0, "w": 800, "h": 600}}),
                )
                .await,
            super::super::right_click::RightClickTool::new(state.clone())
                .invoke(with(json!({"x": 10, "y": 10})))
                .await,
            super::super::scroll::ScrollTool::new(state.clone())
                .invoke(with(json!({"x": 10, "y": 10, "direction": "down"})))
                .await,
            super::super::drag::DragTool::new(state.clone())
                .invoke(with(
                    json!({"from_x": 1, "from_y": 1, "to_x": 9, "to_y": 9}),
                ))
                .await,
            super::super::move_cursor::MoveCursorTool::new(state.clone())
                .invoke(with(json!({"x": 10, "y": 10})))
                .await,
        ];
        for (index, result) in results.into_iter().enumerate() {
            let structured = result.structured_content.expect("structured refusal");
            assert_eq!(structured["code"], "invalid_arguments", "tool {index}");
            assert_eq!(structured["dispatched"], false, "tool {index}");
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
