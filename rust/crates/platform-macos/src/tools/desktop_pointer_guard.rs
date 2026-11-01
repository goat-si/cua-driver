//! Desktop-target pointer input drives the user's real pointer.
//!
//! `move_cursor`, `click`, `drag` and `scroll` with a desktop target post
//! their events at a screen point through the real pointer. Over a window of
//! the app the user is in (the frontmost one), that is ordinary foreground
//! work. Over a window of another app it is background work done with the
//! user's pointer: the pointer jumps out from under the user, and a click also
//! activates that app. Bench v2 caught this on Chrome pages: when the browser
//! route was refused, agents hovered the page with a desktop `move_cursor`
//! and moved the user's pointer (PR-03: 10 of 31 trials).
//!
//! So a desktop-target pointer action whose point lands on a window of an
//! app that is not frontmost refuses with `background_unavailable`, before
//! any event, unless the caller passed `delivery_mode:"foreground"`. Over a
//! Chromium browser the refusal names the browser tools, which act on the
//! page without the user's pointer, instead of recommending foreground.
//! Points over no window, or over the frontmost app, are unchanged.

use cua_driver_core::{
    delivery::{background_unavailable_result, DeliveryMode},
    protocol::ToolResult,
};
use serde_json::{json, Value};

/// The window under a desktop point, as the guard needs it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WindowUnderPoint {
    pub pid: i32,
    pub window_id: u32,
    pub app_name: String,
    pub bundle_id: String,
}

impl WindowUnderPoint {
    fn is_chromium_browser(&self) -> bool {
        super::type_text::is_chromium_identity(&self.bundle_id)
    }
}

/// Topmost ordinary (layer 0, on-screen) window containing the desktop point
/// `(x, y)` in screen points, skipping the driver's own windows (the cursor
/// overlay). Blocking: enumerates windows.
pub(crate) fn window_under_point(x: f64, y: f64) -> Option<WindowUnderPoint> {
    let own = std::process::id() as i32;
    let window = crate::windows::visible_windows()
        .into_iter()
        .filter(|w| w.layer == 0 && w.is_on_screen && w.pid != own)
        .filter(|w| {
            let b = &w.bounds;
            x >= b.x && x < b.x + b.width && y >= b.y && y < b.y + b.height
        })
        .max_by_key(|w| w.z_index)?;
    Some(WindowUnderPoint {
        pid: window.pid,
        window_id: window.window_id,
        app_name: window.app_name,
        bundle_id: crate::apps::bundle_id_for_pid(window.pid).unwrap_or_default(),
    })
}

/// The refusal for a desktop-target pointer action at `point` over `under`,
/// or `None` when it may proceed. Pure, for tests.
pub(crate) fn refusal_for(
    tool: &str,
    point: (f64, f64),
    under: Option<&WindowUnderPoint>,
    frontmost_pid: Option<i32>,
    delivery: DeliveryMode,
) -> Option<ToolResult> {
    let under = under?;
    if delivery.is_foreground() || frontmost_pid == Some(under.pid) {
        return None;
    }
    let (x, y) = point;
    let event_kind = match tool {
        "move_cursor" => "pointer_move",
        "drag" => "drag",
        "scroll" => "scroll",
        _ => "click",
    };
    let lead = format!(
        "{tool} with a desktop target moves the user's real pointer, and ({x:.0}, {y:.0}) is over \
         {app} (pid {pid}, window {wid}), which is not the app in front, so this would do \
         background work with the user's pointer. Nothing was sent.",
        app = under.app_name,
        pid = under.pid,
        wid = under.window_id,
    );
    let window = json!({
        "pid": under.pid,
        "window_id": under.window_id,
        "app_name": under.app_name,
    });
    if under.is_chromium_browser() {
        let message = format!(
            "{lead} {app} is a web browser: act on the page with the browser tools, which leave \
             the user's pointer alone. Call get_browser_state with pid {pid} and window_id {wid}, \
             then browser_pointer (hover, drag, scroll, double or right click), browser_click or \
             browser_type with its target_id and tab_id. If get_browser_state refuses, make the \
             browser_prepare call in its detail.next_call. If browser_prepare refuses too, this \
             page cannot be driven in the background right now: say so rather than taking the \
             user's pointer.",
            app = under.app_name,
            pid = under.pid,
            wid = under.window_id,
        );
        return Some(
            ToolResult::error(message).with_structured(json!({
                "code": "background_unavailable",
                "event_kind": event_kind,
                "window": window,
                "suggestion": "Use the browser tools: get_browser_state, then browser_pointer, browser_click or browser_type.",
                "escalation": {
                    "recommended": "page",
                    "reason": "the point is on a background web browser page; the browser tools act on it without the user's pointer",
                },
                "next_call": {
                    "tool": "get_browser_state",
                    "arguments": { "pid": under.pid, "window_id": under.window_id },
                },
            })),
        );
    }
    let message = format!(
        "{lead} Use a window target instead: target {{kind:\"window\", pid:{pid}, \
         window_id:{wid}}} with x,y in that window's screenshot pixels{move_note}. Retry with \
         delivery_mode:\"foreground\" only when nobody is using this screen or the user agreed \
         to hand over the pointer.",
        pid = under.pid,
        wid = under.window_id,
        move_note = if tool == "move_cursor" {
            " (it moves only the agent cursor and hovers through a background route where one \
             exists)"
        } else {
            ""
        },
    );
    Some(background_unavailable_result(
        message,
        "background_unavailable",
        "the desktop point is over a background app; a desktop-target pointer action would move the user's pointer",
        json!({ "event_kind": event_kind, "window": window }),
    ))
}

/// Guard a desktop-target pointer action whose events land at `points`
/// (screen points). Returns the refusal for the first point over a background
/// app, or `None` when every point may proceed.
pub(crate) async fn check(
    tool: &'static str,
    args: &Value,
    points: Vec<(f64, f64)>,
) -> Option<ToolResult> {
    let protection = protected_refusal(tool, &points).await;
    check_with(tool, args, points, protection, |points, delivery| {
        let frontmost = crate::apps::frontmost_pid();
        points.into_iter().find_map(|point| {
            let under = window_under_point(point.0, point.1);
            refusal_for(tool, point, under.as_ref(), frontmost, delivery)
        })
    })
    .await
}

async fn check_with<F>(
    tool: &str,
    args: &Value,
    points: Vec<(f64, f64)>,
    protection: Option<ToolResult>,
    user_window_check: F,
) -> Option<ToolResult>
where
    F: FnOnce(Vec<(f64, f64)>, DeliveryMode) -> Option<ToolResult> + Send + 'static,
{
    if let Some(refusal) = protection {
        return Some(refusal);
    }
    let delivery = DeliveryMode::from_args(args);
    if delivery.is_foreground() {
        return None;
    }
    match tokio::task::spawn_blocking(move || user_window_check(points, delivery)).await {
        Ok(refusal) => refusal,
        Err(_) => Some(super::pre_dispatch_refusal(
            "background_unavailable",
            format!("{tool} was refused: the user-window protection check could not complete. Nothing was sent."),
        )),
    }
}

/// A browser holding an origin-scoped attachment is reachable only through
/// the browser tools, in any delivery mode: the refusal when any of
/// `points` (every point an event is delivered at) lands on one of its
/// windows.
pub(crate) async fn protected_refusal(tool: &str, points: &[(f64, f64)]) -> Option<ToolResult> {
    protected_refusal_with(tool, points, crate::windows::visible_windows).await
}

async fn protected_refusal_with<F>(
    tool: &str,
    points: &[(f64, f64)],
    inventory: F,
) -> Option<ToolResult>
where
    F: FnOnce() -> Vec<crate::windows::WindowInfo> + Send + 'static,
{
    let windows = tokio::task::spawn_blocking(inventory)
        .await
        .unwrap_or_default();
    cua_driver_core::browser::protection::desktop_point_refusal(tool, points, |x, y| {
        owner_at(&windows, x, y)
    })
}

/// The process owning the topmost ordinary window at `(x, y)`, from one
/// window enumeration. An empty enumeration means the list was unavailable.
fn owner_at(
    windows: &[crate::windows::WindowInfo],
    x: f64,
    y: f64,
) -> cua_driver_core::browser::protection::PointOwner {
    use cua_driver_core::browser::protection::PointOwner;
    if windows.is_empty() {
        return PointOwner::Unavailable;
    }
    let own = std::process::id() as i32;
    windows
        .iter()
        .filter(|w| w.layer == 0 && w.is_on_screen && w.pid != own)
        .filter(|w| {
            let b = &w.bounds;
            x >= b.x && x < b.x + b.width && y >= b.y && y < b.y + b.height
        })
        .max_by_key(|w| w.z_index)
        .map_or(PointOwner::Nothing, |w| {
            PointOwner::Process(i64::from(w.pid))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chrome() -> WindowUnderPoint {
        WindowUnderPoint {
            pid: 8601,
            window_id: 659,
            app_name: "Google Chrome".into(),
            bundle_id: "com.google.Chrome".into(),
        }
    }

    fn benchlab() -> WindowUnderPoint {
        WindowUnderPoint {
            pid: 4242,
            window_id: 77,
            app_name: "BenchLab".into(),
            bundle_id: "com.trycua.benchlab".into(),
        }
    }

    fn text(result: &ToolResult) -> String {
        serde_json::to_value(result).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn a_desktop_move_over_a_background_chrome_names_the_browser_route() {
        // Bench v2 PR-03: Chrome on the user's Space, another app in front.
        let refusal = refusal_for(
            "move_cursor",
            (384.0, 490.0),
            Some(&chrome()),
            Some(1),
            DeliveryMode::Background,
        )
        .expect("refuses");
        assert_eq!(refusal.is_error, Some(true));
        let message = text(&refusal);
        assert!(message.contains("Nothing was sent"));
        assert!(message.contains("get_browser_state with pid 8601 and window_id 659"));
        assert!(message.contains("browser_pointer (hover"));
        assert!(message.contains("detail.next_call"));
        assert!(!message.contains("delivery_mode:\"foreground\""));
        let structured = refusal.structured_content.unwrap();
        assert_eq!(structured["code"], "background_unavailable");
        assert_eq!(structured["event_kind"], "pointer_move");
        assert_eq!(structured["escalation"]["recommended"], "page");
        assert_eq!(structured["next_call"]["tool"], "get_browser_state");
        assert_eq!(structured["next_call"]["arguments"]["window_id"], 659);
    }

    #[test]
    fn a_desktop_pointer_action_over_another_background_app_offers_a_window_target() {
        for (tool, kind) in [
            ("move_cursor", "pointer_move"),
            ("click", "click"),
            ("drag", "drag"),
            ("scroll", "scroll"),
        ] {
            let refusal = refusal_for(
                tool,
                (10.0, 10.0),
                Some(&benchlab()),
                Some(1),
                DeliveryMode::Background,
            )
            .expect("refuses");
            let message = text(&refusal);
            assert!(message.starts_with(tool), "{message}");
            assert!(message.contains("target {kind:\"window\", pid:4242, window_id:77}"));
            assert!(message.contains("delivery_mode:\"foreground\" only when"));
            let structured = refusal.structured_content.unwrap();
            assert_eq!(structured["code"], "background_unavailable");
            assert_eq!(structured["event_kind"], kind);
            assert_eq!(structured["escalation"]["recommended"], "foreground");
            assert_eq!(structured["window"]["pid"], 4242);
        }
    }

    #[test]
    fn the_frontmost_app_foreground_delivery_and_bare_desktop_are_unchanged() {
        // The app the user is in: ordinary foreground work.
        assert!(refusal_for(
            "move_cursor",
            (1.0, 1.0),
            Some(&chrome()),
            Some(8601),
            DeliveryMode::Background,
        )
        .is_none());
        // The caller explicitly accepted using the real pointer.
        assert!(refusal_for(
            "move_cursor",
            (1.0, 1.0),
            Some(&chrome()),
            Some(1),
            DeliveryMode::Foreground,
        )
        .is_none());
        // No window under the point.
        assert!(refusal_for(
            "move_cursor",
            (1.0, 1.0),
            None,
            Some(1),
            DeliveryMode::Background,
        )
        .is_none());
    }

    #[test]
    fn an_unknown_frontmost_app_still_refuses_over_a_window() {
        assert!(refusal_for(
            "click",
            (1.0, 1.0),
            Some(&benchlab()),
            None,
            DeliveryMode::Background,
        )
        .is_some());
    }
}

#[cfg(test)]
mod task_failure_tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn a_failed_user_window_check_refuses() {
        let dispatched = Arc::new(AtomicUsize::new(0));
        let refusal = check_with("click", &json!({}), vec![(1.0, 2.0)], None, |_, _| {
            panic!("fake user-window check failed")
        })
        .await;
        if refusal.is_none() {
            dispatched.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(
            dispatched.load(Ordering::SeqCst),
            0,
            "no input should be dispatched"
        );
        let result = refusal.expect("a failed check must refuse");
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("refusal details");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
        assert!(format!("{:?}", result.content).contains("could not complete"));
    }
}
