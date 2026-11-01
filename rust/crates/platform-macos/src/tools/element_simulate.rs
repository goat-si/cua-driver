//! Convert a retained element's screen frame to the existing window-points route.
use cua_driver_core::protocol::ToolResult;
use serde_json::{json, Value};

use super::element_pointer::{ElementTarget, Hit};
use crate::windows::WindowBounds;

pub(super) fn option(args: &Value) -> Result<bool, ToolResult> {
    match args.get("simulate") {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(super::invalid_arguments(
            "click.simulate must be a boolean; nothing was sent.",
        )),
    }
}

pub(super) fn validate(args: &Value) -> Result<bool, ToolResult> {
    let simulate = option(args)?;
    if !simulate {
        return Ok(false);
    }
    let count = super::element_pointer::click_count(args)?;
    if count != 1 {
        return Err(super::invalid_arguments(
            "click.simulate requires count 1; nothing was sent.",
        ));
    }
    let token = args
        .get("element_token")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    let incompatible = ["x", "y", "capture_id", "debug_image_out"]
        .iter()
        .any(|key| args.get(*key).is_some_and(|value| !value.is_null()))
        || args
            .get("from_zoom")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || args
            .get("action")
            .is_some_and(|v| v.as_str() != Some("press"))
        || args
            .get("button")
            .is_some_and(|v| !matches!(v.as_str(), Some("left" | "")))
        || args
            .get("modifier")
            .is_some_and(|v| !matches!(v.as_array(), Some(a) if a.is_empty()));
    if !token || incompatible {
        return Err(super::invalid_arguments(
            "click.simulate requires an unmodified left press on an element; nothing was sent.",
        ));
    }
    Ok(true)
}

pub(super) fn pixel_args(
    mut args: Value,
    pid: i32,
    window_id: u32,
    rect: Option<[f64; 4]>,
    bounds: Option<WindowBounds>,
) -> Result<Value, ToolResult> {
    let unavailable = |reason: &str| {
        super::pre_dispatch_refusal(
            "element_frame_unavailable",
            format!("click: {reason}; nothing was sent."),
        )
    };
    let [x, y, w, h] = rect.ok_or_else(|| unavailable("the element frame is unreadable"))?;
    if ![x, y, w, h].iter().all(|v| v.is_finite()) || w <= 0.0 || h <= 0.0 {
        return Err(unavailable("the element frame is invalid or zero-size"));
    }
    let bounds = bounds.ok_or_else(|| unavailable("the window frame is unreadable"))?;
    let (lx, ly) = (x + w / 2.0 - bounds.x, y + h / 2.0 - bounds.y);
    if ![lx, ly, bounds.x, bounds.y, bounds.width, bounds.height]
        .iter()
        .all(|v| v.is_finite())
        || bounds.width <= 0.0
        || bounds.height <= 0.0
        || lx < 0.0
        || ly < 0.0
        || lx >= bounds.width
        || ly >= bounds.height
    {
        return Err(unavailable("the element center lies outside its window"));
    }
    let Some(map) = args.as_object_mut() else {
        return Err(super::invalid_arguments(
            "click arguments must be an object",
        ));
    };
    map.remove("element_token");
    map.remove("simulate");
    map.insert("pid".into(), json!(pid));
    map.insert("window_id".into(), json!(window_id));
    map.insert("x".into(), json!(lx));
    map.insert("y".into(), json!(ly));
    map.insert("coordinate_space".into(), json!("window_points"));
    map.insert(
        "window_basis".into(),
        json!({"x":bounds.x,"y":bounds.y,"w":bounds.width,"h":bounds.height,"exact":true}),
    );
    Ok(args)
}

pub(super) fn note(mut result: ToolResult) -> ToolResult {
    const NOTE: &str = "Element click used the pointer route.";
    // A successful click is published as the closed action result, whose
    // summary is the first text block; state the route there as well.
    if let Some(cua_driver_core::protocol::Content::Text { text, .. }) = result
        .content
        .iter_mut()
        .find(|content| matches!(content, cua_driver_core::protocol::Content::Text { .. }))
    {
        text.push(' ');
        text.push_str(NOTE);
    }
    let structured = result.structured_content.get_or_insert_with(|| json!({}));
    if let Some(object) = structured.as_object_mut() {
        let note = NOTE;
        let combined = match object.get("note").and_then(Value::as_str) {
            Some(existing) if !existing.is_empty() => format!("{existing} {note}"),
            _ => note.to_owned(),
        };
        object.insert("note".into(), json!(combined));
    }
    result
}

pub(super) fn checked_pixel_args(
    args: Value,
    pid: i32,
    window_id: u32,
    target: &dyn ElementTarget,
) -> Result<Value, ToolResult> {
    let rect = target.element_rect();
    let raw_bounds = target.window_bounds();
    let bounds = raw_bounds.map(|[x, y, width, height]| WindowBounds {
        x,
        y,
        width,
        height,
    });
    let pixel = pixel_args(args, pid, window_id, rect, bounds)?;
    let Some([x, y, w, h]) = rect else {
        return Err(super::pre_dispatch_refusal(
            "element_frame_unavailable",
            "click: the element frame is unreadable; nothing was sent.",
        ));
    };
    let center = (x + w / 2.0, y + h / 2.0);
    match target.hit_at(center) {
        Hit::Target => {},
        Hit::Elsewhere(_) | Hit::Nothing => return Err(super::pre_dispatch_refusal(
            "element_not_at_point", "The element's center does not hit the element or its descendant in the requested window; nothing was sent.")),
    }
    if target.element_rect() != rect || target.window_bounds() != raw_bounds {
        return Err(super::pre_dispatch_refusal("element_frame_unavailable", "click: the element or window frame changed while checking the target; nothing was sent."));
    }
    Ok(pixel)
}

/// Carry the exact retained element alongside the translated coordinates so
/// the caller can check that element again before the pointer operation.
pub(super) async fn dispatch<T, F, Fut>(
    prepared: Result<Value, ToolResult>,
    element: T,
    send: F,
) -> ToolResult
where
    F: FnOnce(Value, T) -> Fut,
    Fut: std::future::Future<Output = ToolResult>,
{
    match prepared {
        Ok(pixel) => note(send(pixel, element).await),
        Err(refusal) => refusal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bounds() -> WindowBounds {
        WindowBounds {
            x: 10.0,
            y: 20.0,
            width: 300.0,
            height: 200.0,
        }
    }
    #[test]
    fn translates_center_once_and_refuses_bad_frames() {
        let args = json!({"element_token":"s00000001:0", "simulate":true});
        let point = pixel_args(
            args.clone(),
            7,
            9,
            Some([30.0, 40.0, 20.0, 10.0]),
            Some(bounds()),
        )
        .unwrap();
        assert_eq!(
            (point["x"].as_f64(), point["y"].as_f64()),
            (Some(30.0), Some(25.0))
        );
        for rect in [
            None,
            Some([30.0, 40.0, 0.0, 10.0]),
            Some([400.0, 40.0, 20.0, 10.0]),
        ] {
            let refusal = pixel_args(args.clone(), 7, 9, rect, Some(bounds())).unwrap_err();
            let s = refusal.structured_content.unwrap();
            assert_eq!(s["code"], "element_frame_unavailable");
            assert_eq!(s["dispatched"], false);
        }
    }
    #[test]
    fn only_boolean_simulate_is_accepted() {
        assert!(!option(&json!({})).unwrap());
        assert!(!option(&json!({"simulate":false})).unwrap());
        assert!(option(&json!({"simulate":true})).unwrap());
        for value in [json!(null), json!("true"), json!(1)] {
            assert!(option(&json!({"simulate":value})).is_err());
        }
    }
    #[test]
    fn note_preserves_existing_text_and_is_present_on_plain_errors() {
        let result = note(ToolResult::error("failed"));
        assert_eq!(
            result.structured_content.unwrap()["note"],
            "Element click used the pointer route."
        );
        let result = note(ToolResult::text("sent").with_structured(json!({"note":"Existing."})));
        assert_eq!(
            result.structured_content.unwrap()["note"],
            "Existing. Element click used the pointer route."
        );
    }

    /// A successful click's structured content is replaced by the published
    /// action result, whose summary is the first text block: the route must
    /// be stated there too or a client never sees it.
    #[test]
    fn pointer_route_is_stated_in_the_summary_text() {
        let result = note(ToolResult::text("✅ Sent click.").with_structured(json!({})));
        let first = result.content.iter().find_map(|content| match content {
            cua_driver_core::protocol::Content::Text { text, .. } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(
            first.as_deref(),
            Some("✅ Sent click. Element click used the pointer route.")
        );
    }

    #[tokio::test]
    async fn dispatch_sends_exactly_one_translated_pixel_call() {
        let calls = std::cell::Cell::new(0);
        let args = json!({"element_token":"s00000001:0", "simulate":true});
        let prepared = pixel_args(
            args.clone(),
            7,
            9,
            Some([30.0, 40.0, 20.0, 10.0]),
            Some(bounds()),
        );
        let retained = std::sync::Arc::new(73_u32);
        let weak = std::sync::Arc::downgrade(&retained);
        let result = dispatch(prepared, retained, |pixel, retained| {
            calls.set(calls.get() + 1);
            assert_eq!(*retained, 73);
            assert!(weak.upgrade().is_some());
            assert_eq!(
                (pixel["x"].as_f64(), pixel["y"].as_f64()),
                (Some(30.0), Some(25.0))
            );
            async move {
                assert_eq!(*retained, 73);
                ToolResult::text("sent").with_structured(json!({"effect":"unverifiable"}))
            }
        })
        .await;
        assert_eq!(calls.get(), 1);
        assert!(weak.upgrade().is_none());
        assert_eq!(
            result.structured_content.unwrap()["note"],
            "Element click used the pointer route."
        );
        let refused = dispatch(
            pixel_args(args, 7, 9, None, Some(bounds())),
            73_u32,
            |_, _| {
                calls.set(calls.get() + 1);
                async { ToolResult::text("sent") }
            },
        )
        .await;
        assert_eq!(calls.get(), 1);
        assert_eq!(
            refused.structured_content.unwrap()["code"],
            "element_frame_unavailable"
        );
    }
}
