//! Convert a retained element's screen frame to the existing window-points route.
use cua_driver_core::protocol::ToolResult;
use serde_json::{json, Value};

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
    let structured = result.structured_content.get_or_insert_with(|| json!({}));
    if let Some(object) = structured.as_object_mut() {
        let note = "Element click used the pointer route.";
        let combined = match object.get("note").and_then(Value::as_str) {
            Some(existing) if !existing.is_empty() => format!("{existing} {note}"),
            _ => note.to_owned(),
        };
        object.insert("note".into(), json!(combined));
    }
    result
}

pub(super) async fn dispatch<F, Fut>(
    args: Value,
    pid: i32,
    window_id: u32,
    rect: Option<[f64; 4]>,
    bounds: Option<WindowBounds>,
    send: F,
) -> ToolResult
where
    F: FnOnce(Value) -> Fut,
    Fut: std::future::Future<Output = ToolResult>,
{
    match pixel_args(args, pid, window_id, rect, bounds) {
        Ok(pixel) => note(send(pixel).await),
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
    #[tokio::test]
    async fn dispatch_sends_exactly_one_translated_pixel_call() {
        let calls = std::cell::Cell::new(0);
        let result = dispatch(
            json!({"element_token":"s00000001:0", "simulate":true}),
            7,
            9,
            Some([30.0, 40.0, 20.0, 10.0]),
            Some(bounds()),
            |args| {
                calls.set(calls.get() + 1);
                assert_eq!(args["x"], 30.0);
                assert_eq!(args["y"], 25.0);
                async { ToolResult::text("sent").with_structured(json!({"effect":"unverifiable"})) }
            },
        )
        .await;
        assert_eq!(calls.get(), 1);
        assert_eq!(
            result.structured_content.unwrap()["note"],
            "Element click used the pointer route."
        );
        let refused = dispatch(json!({}), 7, 9, None, Some(bounds()), |_| {
            calls.set(calls.get() + 1);
            async { ToolResult::text("sent") }
        })
        .await;
        assert_eq!(calls.get(), 1);
        assert_eq!(
            refused.structured_content.unwrap()["code"],
            "element_frame_unavailable"
        );
    }
}
