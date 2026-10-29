use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;

pub struct ListWindowsTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "list_windows".into(),
        description: "List top-level windows known to WindowServer, including off-screen ones (minimized, other Space, hidden): layer-0 only without `pid`, every layer with `pid`. Use it to find a `pid` and `window_id` for get_window_state; filter with `app` or `pid`, because an unfiltered call lists every window on the host. AppKit-internal helper windows and the driver's own overlay are omitted.\n\
            \n\
            Per record: window_id, pid, app_name, title, bounds, layer, z_index, is_on_screen, space_ids, current_space_id, on_current_space. For the frontmost window take the maximum integer z_index; null means stacking is unavailable, so do not infer it from array order.\n\
            \n\
            To confirm a window closed, call with its `pid`: when enumeration_complete is true, authoritative_window_ids lists every window id WindowServer still has for that pid, hidden and helper windows included, and a window_id missing from it no longer exists. When enumeration_complete is false, a missing window proves nothing.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pid": {
                    "type": "integer",
                    "description": "Only this pid's windows, on every layer."
                },
                "app": {
                    "type": "string",
                    "description": cua_driver_core::tool_args::LIST_WINDOWS_APP_FILTER_DESCRIPTION
                },
                "on_screen_only": {
                    "type": "boolean",
                    "description": "Drop windows not on the current Space."
                }
            },
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for ListWindowsTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid_filter = match parse_pid_filter(&args) {
            Ok(pid) => pid,
            Err(message) => return ToolResult::error(message),
        };
        let on_screen_only = args.bool_or("on_screen_only", false);

        let enumeration = match pid_filter {
            Some(pid) => crate::windows::pid_windows_with_space_snapshot(pid, on_screen_only),
            None if on_screen_only => crate::windows::visible_windows_with_space_snapshot(),
            None => crate::windows::all_windows_with_space_snapshot(),
        };
        let current_space_id = enumeration.current_space_id;
        // Read the pid's full window-id set before any filtering below, so a
        // caller can tell a closed window from one this listing leaves out.
        let (enumeration_complete, authoritative_window_ids) = closure_fields(
            enumeration.complete,
            pid_filter.map(crate::windows::pid_window_id_snapshot),
        );
        let mut windows = enumeration.windows;

        // The driver's own windows (the agent cursor overlay) are not targets.
        let own_pid = std::process::id() as i32;
        windows.retain(|w| w.pid != own_pid);
        if let Some(filter) = args.opt_str("app") {
            let mut bundle_ids = std::collections::HashMap::new();
            windows.retain(|w| {
                let bundle_id = bundle_ids
                    .entry(w.pid)
                    .or_insert_with(|| crate::apps::bundle_id_for_pid(w.pid));
                cua_driver_core::tool_args::app_filter_matches(
                    &filter,
                    &w.app_name,
                    bundle_id.as_deref(),
                )
            });
        }
        crate::windows::retain_ax_reachable(&mut windows, current_space_id);

        ToolResult::text(format_window_list(&windows)).with_structured(list_windows_structured(
            &windows,
            current_space_id,
            enumeration_complete,
            authoritative_window_ids,
        ))
    }
}

/// The optional pid filter: absent or null lists every app; anything else must
/// be a positive pid that fits macOS's pid_t, never a wrapped or unscoped query.
fn parse_pid_filter(args: &Value) -> Result<Option<i32>, String> {
    match args.get("pid") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
            .map(Some)
            .ok_or_else(|| {
                "pid must be a positive process ID no greater than 2147483647".to_owned()
            }),
    }
}

/// enumeration_complete and, with a pid, the authoritative id set. Both
/// snapshots must be complete; an incomplete one publishes no id set, so a
/// caller never reads an unreadable window as a closed one.
fn closure_fields(
    listing_complete: bool,
    identity: Option<crate::windows::WindowIdSnapshot>,
) -> (bool, Option<Vec<u32>>) {
    match identity {
        None => (listing_complete, None),
        Some(identity) if listing_complete && identity.complete => {
            (true, Some(identity.window_ids))
        }
        Some(_) => (false, None),
    }
}

fn list_windows_structured(
    windows: &[crate::windows::WindowInfo],
    current_space_id: Option<u64>,
    enumeration_complete: bool,
    authoritative_window_ids: Option<Vec<u32>>,
) -> Value {
    let mut structured = serde_json::json!({
        "windows": windows.iter().map(window_record_json).collect::<Vec<_>>(),
        "current_space_id": current_space_id,
        "enumeration_complete": enumeration_complete,
    });
    if let Some(ids) = authoritative_window_ids {
        structured["authoritative_window_ids"] = serde_json::json!(ids);
    }
    structured
}

fn format_window_list(windows: &[crate::windows::WindowInfo]) -> String {
    let mut lines = vec![format!("Found {} window(s).", windows.len())];
    for w in windows {
        let title = if w.title.is_empty() {
            "(no title)".to_owned()
        } else {
            format!("\"{}\"", w.title)
        };
        let tag = if w.is_on_screen { "" } else { " [off-screen]" };
        lines.push(format!(
            "- {} (pid {}) {} [window_id: {}]{tag}",
            w.app_name, w.pid, title, w.window_id
        ));
    }
    lines.join("\n")
}

pub(super) fn window_record_json(w: &crate::windows::WindowInfo) -> Value {
    serde_json::json!({
        "window_id": w.window_id,
        "pid": w.pid,
        "app_name": w.app_name,
        "title": w.title,
        "bounds": {
            "x": w.bounds.x,
            "y": w.bounds.y,
            "width": w.bounds.width,
            "height": w.bounds.height
        },
        "layer": w.layer,
        "z_index": w.z_index,
        "is_on_screen": w.is_on_screen,
        "current_space_id": w.current_space_id,
        "on_current_space": w.on_current_space,
        "space_ids": w.space_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_record_includes_observed_z_index() {
        let window = crate::windows::WindowInfo {
            window_id: 42,
            pid: 123,
            app_name: "Example".into(),
            title: "Document".into(),
            bounds: crate::windows::WindowBounds {
                x: 1.0,
                y: 2.0,
                width: 300.0,
                height: 200.0,
            },
            layer: 0,
            z_index: 7,
            is_on_screen: true,
            current_space_id: Some(1),
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
        };

        assert_eq!(window_record_json(&window)["z_index"], serde_json::json!(7));
        assert_eq!(
            window_record_json(&window)["current_space_id"],
            serde_json::json!(1)
        );
        assert_eq!(
            window_record_json(&window)["on_current_space"],
            serde_json::json!(true)
        );
    }

    fn window(
        window_id: u32,
        pid: i32,
        app: &str,
        title: &str,
        on_screen: bool,
    ) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id,
            pid,
            app_name: app.into(),
            title: title.into(),
            bounds: crate::windows::WindowBounds {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            layer: 0,
            z_index: 1,
            is_on_screen: on_screen,
            current_space_id: Some(1),
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
        }
    }

    #[test]
    fn empty_list_keeps_the_count_only_header() {
        assert_eq!(format_window_list(&[]), "Found 0 window(s).");
    }

    #[test]
    fn pid_filter_accepts_only_a_positive_i32_pid() {
        assert_eq!(parse_pid_filter(&serde_json::json!({})), Ok(None));
        assert_eq!(
            parse_pid_filter(&serde_json::json!({"pid": null})),
            Ok(None)
        );
        assert_eq!(
            parse_pid_filter(&serde_json::json!({"pid": 42})),
            Ok(Some(42))
        );
        assert_eq!(
            parse_pid_filter(&serde_json::json!({"pid": i32::MAX})),
            Ok(Some(i32::MAX))
        );
        for pid in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(3_000_000_000_u64),
            serde_json::json!(u64::MAX),
            serde_json::json!("7"),
            serde_json::json!(1.5),
            serde_json::json!(true),
        ] {
            assert!(
                parse_pid_filter(&serde_json::json!({ "pid": pid.clone() })).is_err(),
                "{pid} must not become an unscoped or wrapped query"
            );
        }
    }

    /// Live WindowServer check of the real adapter, run by hand:
    /// cargo test -p platform-macos --lib live_listing -- --ignored
    #[tokio::test]
    #[ignore = "reads the live WindowServer of the current user session"]
    async fn live_listing_is_covered_by_the_authoritative_ids() {
        let mut pids: Vec<i32> = crate::windows::all_windows_any_layer()
            .iter()
            .map(|window| window.pid)
            .filter(|pid| *pid > 0)
            .collect();
        pids.sort_unstable();
        pids.dedup();
        assert!(
            !pids.is_empty(),
            "the session has at least one window to observe"
        );
        for pid in pids {
            assert_listing_is_covered(pid).await;
        }
    }

    async fn assert_listing_is_covered(pid: i32) {
        let result = ListWindowsTool
            .invoke(serde_json::json!({ "pid": pid }))
            .await;
        let structured = result.structured_content.expect("structured listing");
        assert_eq!(structured["enumeration_complete"], true, "{structured}");
        let ids: Vec<u64> = structured["authoritative_window_ids"]
            .as_array()
            .expect("ids with pid and a complete enumeration")
            .iter()
            .map(|id| id.as_u64().expect("integer id"))
            .collect();
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
        for window in structured["windows"].as_array().expect("windows") {
            let id = window["window_id"].as_u64().expect("window_id");
            assert!(ids.contains(&id), "listed window {id} missing from {ids:?}");
        }
        eprintln!(
            "pid {pid}: {} listed, {} authoritative ids",
            structured["windows"].as_array().map_or(0, Vec::len),
            ids.len()
        );
        assert!(cua_driver_contract::validate_success_output("list_windows", structured).unwrap());
    }

    #[tokio::test]
    async fn an_out_of_range_pid_is_refused_before_enumeration() {
        let result = ListWindowsTool
            .invoke(serde_json::json!({"pid": 3_000_000_000_u64}))
            .await;
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn closure_ids_need_a_complete_listing_and_a_complete_identity_snapshot() {
        let snapshot = |complete: bool, window_ids: Vec<u32>| crate::windows::WindowIdSnapshot {
            complete,
            window_ids,
        };
        assert_eq!(closure_fields(true, None), (true, None));
        assert_eq!(closure_fields(false, None), (false, None));
        assert_eq!(
            closure_fields(true, Some(snapshot(true, vec![41, 42]))),
            (true, Some(vec![41, 42]))
        );
        assert_eq!(
            closure_fields(true, Some(snapshot(true, vec![]))),
            (true, Some(vec![]))
        );
        assert_eq!(
            closure_fields(false, Some(snapshot(true, vec![41]))),
            (false, None)
        );
        assert_eq!(
            closure_fields(true, Some(snapshot(false, vec![]))),
            (false, None)
        );
    }

    #[test]
    fn authoritative_ids_cover_windows_the_listing_filters_out() {
        // The listing kept one window after own-pid, app, on_screen_only and
        // AX-helper filtering; the identity snapshot still names the overlay,
        // the helper, the off-screen and the non-zero-layer windows.
        let listed = [window(42, 7, "Example", "Document", true)];
        let structured =
            list_windows_structured(&listed, Some(1), true, Some(vec![42, 77, 78, 79]));
        assert_eq!(structured["enumeration_complete"], true);
        assert_eq!(
            structured["authoritative_window_ids"],
            serde_json::json!([42, 77, 78, 79])
        );
        assert_eq!(structured["windows"].as_array().map(Vec::len), Some(1));
        assert!(cua_driver_contract::validate_success_output("list_windows", structured).unwrap());

        let unscoped = list_windows_structured(&listed, None, false, None);
        assert_eq!(unscoped["enumeration_complete"], false);
        assert!(unscoped.get("authoritative_window_ids").is_none());
        assert!(cua_driver_contract::validate_success_output("list_windows", unscoped).unwrap());
    }

    #[test]
    fn text_lists_one_row_per_window_with_ids_and_screen_state() {
        let text = format_window_list(&[
            window(10700, 65305, "Calculator", "Calculator", true),
            window(10701, 65305, "Calculator", "", false),
        ]);
        assert_eq!(
            text,
            "Found 2 window(s).\n\
             - Calculator (pid 65305) \"Calculator\" [window_id: 10700]\n\
             - Calculator (pid 65305) (no title) [window_id: 10701] [off-screen]"
        );
    }
}
