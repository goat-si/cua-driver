//! Element clicks with `count` above 1.
//!
//! A double or triple click on an element is a gesture, so it is sent as one:
//! AXOpen when the element lists it (count 2 only, the same rule as
//! `double_click`), otherwise N left down/up pairs at the element's center
//! with clickCount 1..N. The pointer route posts only after the center is
//! proven to be on that element inside the requested window, and the element
//! and window frames read the same again right before posting; anything else
//! is refused with nothing sent. Nothing reads back what the clicks opened, so
//! the result is never `confirmed`.

use async_trait::async_trait;
use cua_driver_core::background_input::BackgroundAction;
use cua_driver_core::protocol::ToolResult;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::ax::bindings::{AXError, AXUIElementRef};
use crate::ax::snapshot::RetainedElement;
use crate::goat::action_latency as latency;

use super::double_click::{
    background_action_for_element, double_click_element, ElementDoubleClick,
};
use super::{BackgroundMutationLease, ToolState};

/// Largest click count a click accepts (the contract's `click_count_schema`).
pub(super) const MAX_CLICK_COUNT: u64 = 3;

/// The requested click count. Only an absent `count` defaults to 1; any value
/// that is not an integer from 1 to 3 (0, negative, too large, fractional,
/// a string, null) is refused before anything is sent.
pub(super) fn click_count(args: &Value) -> Result<usize, ToolResult> {
    let Some(value) = args.get("count") else {
        return Ok(1);
    };
    match value.as_u64() {
        Some(count) if (1..=MAX_CLICK_COUNT).contains(&count) => Ok(count as usize),
        _ => Err(super::invalid_arguments(format!(
            "click.count must be an integer from 1 to {MAX_CLICK_COUNT}; got {value}. Nothing was sent."
        ))),
    }
}

/// A refusal raised before any input was handed to the event transport or
/// the accessibility API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NotDispatched {
    pub(super) code: &'static str,
    pub(super) message: String,
}

impl std::fmt::Display for NotDispatched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NotDispatched {}

fn refuse(code: &'static str, message: impl Into<String>) -> NotDispatched {
    NotDispatched {
        code,
        message: message.into(),
    }
}

/// Where the clicks go: the element's center on screen and in the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ElementPoint {
    pub(super) screen: (f64, f64),
    pub(super) window_local: (f64, f64),
}

/// What the accessibility hit test found at a point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Hit {
    /// The target or one of its descendants, inside the requested window.
    Target,
    /// Something else (described for the refusal).
    Elsewhere(String),
    Nothing,
}

/// The element a multi-click addresses and every effect it can have. The
/// live target talks to the app; tests substitute one per pid.
#[async_trait]
pub(super) trait ElementTarget: Send + Sync {
    /// Take the per-pid background lease after gating `action` on the exact window.
    async fn gate(&self, action: BackgroundAction) -> Result<BackgroundMutationLease, ToolResult>;
    /// The background refusal for a Chromium `<select>`, which a click would open
    /// and raise.
    fn web_select_refusal(&self) -> Option<ToolResult>;
    fn lists_ax_open(&self) -> bool;
    fn ax_open(&self) -> AXError;
    /// Whether pointer events may go to the exact window now (always true
    /// in the foreground, where activation was proven first).
    fn pointer_permitted(&self, foreground: bool) -> bool;
    fn element_rect(&self) -> Option<[f64; 4]>;
    fn window_bounds(&self) -> Option<[f64; 4]>;
    fn hit_at(&self, point: (f64, f64)) -> Hit;
    /// Hand `count` down/up pairs to the event transport. Every event is built
    /// before the first is posted, so an error means nothing was handed over.
    fn post(&self, point: &ElementPoint, count: usize, foreground: bool) -> anyhow::Result<()>;
    /// Run `body` with the exact window frontmost and focused; refuse without
    /// running it when that cannot be proven.
    fn in_foreground(&self, body: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()>;
}

/// The element's center, only when it is on that element in the requested
/// window and nothing moved while it was checked.
pub(super) fn checked_point(
    target: &dyn ElementTarget,
    foreground: bool,
) -> Result<ElementPoint, NotDispatched> {
    if !target.pointer_permitted(foreground) {
        return Err(refuse(
            "pointer_not_permitted",
            "Pointer clicks cannot reach this exact window in the background (it is minimized, \
             hidden, off-Space or not the app's); nothing was sent.",
        ));
    }
    let rect = target
        .element_rect()
        .filter(|rect| rect[2] >= 1.0 && rect[3] >= 1.0)
        .ok_or_else(|| {
            refuse(
                "element_point_unresolved",
                "The element has no readable on-screen frame, so there is no point to click; \
                 nothing was sent.",
            )
        })?;
    let bounds = target.window_bounds().ok_or_else(|| {
        refuse(
            "window_bounds_unavailable",
            "The window's bounds cannot be read; nothing was sent.",
        )
    })?;
    let screen = (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0);
    let window_local = (screen.0 - bounds[0], screen.1 - bounds[1]);
    if window_local.0 < 0.0
        || window_local.1 < 0.0
        || window_local.0 >= bounds[2]
        || window_local.1 >= bounds[3]
    {
        return Err(refuse(
            "element_outside_window",
            format!(
                "The element's center ({:.1}, {:.1}) lies outside its window (scrolled away or \
                 clipped); nothing was sent.",
                screen.0, screen.1
            ),
        ));
    }
    match target.hit_at(screen) {
        Hit::Target => {}
        Hit::Elsewhere(what) => {
            return Err(refuse(
                "element_not_at_point",
                format!(
                    "The element's center ({:.1}, {:.1}) hits {what}, not the element; nothing \
                     was sent.",
                    screen.0, screen.1
                ),
            ))
        }
        Hit::Nothing => {
            return Err(refuse(
                "element_not_at_point",
                format!(
                    "Nothing answers the hit test at the element's center ({:.1}, {:.1}); \
                     nothing was sent.",
                    screen.0, screen.1
                ),
            ))
        }
    }
    if target.element_rect() != Some(rect) || target.window_bounds() != Some(bounds) {
        return Err(refuse(
            "element_moved",
            "The element or its window moved while the point was checked; nothing was sent.",
        ));
    }
    Ok(ElementPoint {
        screen,
        window_local,
    })
}

/// Pointer clicks handed to the event transport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct PointerClicks {
    pub(super) point: ElementPoint,
    /// Background delivery hands one off-screen primer pair over first
    /// (Chromium's activation gate); it lands on no element.
    pub(super) primer: bool,
}

/// What a multi-click did.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Performed {
    AxOpen(String),
    Clicks(PointerClicks),
}

fn pointer_clicks(
    target: &dyn ElementTarget,
    count: usize,
    foreground: bool,
) -> anyhow::Result<PointerClicks> {
    let point = checked_point(target, foreground)?;
    target.post(&point, count, foreground).map_err(|error| {
        refuse(
            "pointer_events_unavailable",
            format!("The click events could not be built ({error}); nothing was sent."),
        )
    })?;
    Ok(PointerClicks {
        point,
        primer: !foreground,
    })
}

/// AXOpen or pointer clicks, decided as `double_click` decides for count 2.
fn dispatch(
    target: &dyn ElementTarget,
    idx: usize,
    count: usize,
    use_ax_open: bool,
    foreground: bool,
) -> anyhow::Result<Performed> {
    if count != 2 {
        return pointer_clicks(target, count, foreground).map(Performed::Clicks);
    }
    Ok(
        match double_click_element(
            idx,
            use_ax_open,
            foreground,
            || target.ax_open(),
            || pointer_clicks(target, count, foreground),
        )? {
            ElementDoubleClick::Ax(message) => Performed::AxOpen(message),
            ElementDoubleClick::Pixel(clicks) => Performed::Clicks(clicks),
        },
    )
}

/// The blocking part: in the foreground everything, the fallback included,
/// runs only after the exact window is proven frontmost.
pub(super) fn run_blocking(
    target: &dyn ElementTarget,
    idx: usize,
    count: usize,
    use_ax_open: bool,
    foreground: bool,
) -> anyhow::Result<Performed> {
    if !foreground {
        return dispatch(target, idx, count, use_ax_open, false);
    }
    let mut outcome = None;
    let activation = target.in_foreground(&mut || {
        outcome = Some(dispatch(target, idx, count, use_ax_open, true));
        Ok(())
    });
    match (outcome, activation) {
        (Some(outcome), _) => outcome,
        (None, Err(error)) => Err(refuse(
            "foreground_unavailable",
            format!("The window could not be brought to the front ({error}); nothing was sent."),
        )
        .into()),
        (None, Ok(())) => Err(anyhow::anyhow!("foreground element click did not run")),
    }
}

/// The public result of a multi-click.
pub(super) fn performed_result(
    idx: usize,
    window_id: u32,
    count: usize,
    performed: &Performed,
    suffix: &str,
) -> ToolResult {
    match performed {
        Performed::AxOpen(message) => ToolResult::text(format!(
            "{message} count {count} was sent as AXOpen; no mouse clicks were posted. Nothing \
             reads back what it opened.{suffix}"
        ))
        .with_structured(json!({
            "path": "ax",
            "verified": false,
            "effect": "unverifiable",
            "count": count,
            "performed": "ax_open",
            "clicks_posted": 0,
            "primer_clicks": 0,
        })),
        Performed::Clicks(clicks) => {
            let (x, y) = clicks.point.screen;
            let primer = if clicks.primer {
                ", after one off-screen primer click"
            } else {
                ""
            };
            ToolResult::text(format!(
                "✅ Handed {count} left clicks (clickCount 1→{count}) to the event transport at \
                 element [{idx}]'s center ({x:.1}, {y:.1}) in window {window_id}{primer}, after \
                 checking that point is on the element. Nothing reads back what they opened; \
                 check the window.{suffix}"
            ))
            .with_structured(json!({
                "path": "cgevent",
                "verified": false,
                "effect": "unverifiable",
                "count": count,
                "performed": "coordinate_clicks",
                "clicks_posted": count,
                "primer_clicks": u8::from(clicks.primer),
                "screen_point": { "x": x, "y": y },
            }))
        }
    }
}

fn error_result(count: usize, error: anyhow::Error) -> ToolResult {
    if let Some(refusal) = error.downcast_ref::<NotDispatched>() {
        let mut result = super::pre_dispatch_refusal(refusal.code, format!("click: {refusal}"));
        if let Some(structured) = result.structured_content.as_mut() {
            structured["count"] = json!(count);
            structured["clicks_posted"] = json!(0);
        }
        return result;
    }
    ToolResult::error(format!("click (count {count}) failed: {error}"))
        .with_structured(json!({ "count": count, "clicks_posted": 0 }))
}

/// An element click with count 2 or 3.
pub(super) struct ElementClick {
    pub(super) pid: i32,
    pub(super) window_id: u32,
    pub(super) idx: usize,
    pub(super) element: RetainedElement,
    pub(super) count: usize,
    pub(super) foreground: bool,
    pub(super) cursor_key: String,
}

pub(super) async fn click_element(state: &ToolState, request: ElementClick) -> ToolResult {
    let ElementClick {
        pid,
        window_id,
        idx,
        element,
        count,
        foreground,
        cursor_key,
    } = request;
    let target = target_for(pid, window_id, idx, element);

    if !foreground {
        let probe = target.clone();
        if let Ok(Some(refusal)) =
            latency::spawn_action_blocking(move || probe.web_select_refusal()).await
        {
            return refusal;
        }
    }
    let use_ax_open = count == 2 && {
        let probe = target.clone();
        latency::spawn_action_blocking(move || probe.lists_ax_open())
            .await
            .unwrap_or(false)
    };
    let lease = if foreground {
        None
    } else {
        match target
            .gate(background_action_for_element(use_ax_open))
            .await
        {
            Ok(lease) => Some(lease),
            Err(refusal) => return refusal,
        }
    };

    let prior_front = crate::apps::frontmost_pid();
    let snapshot = if foreground {
        crate::window_change_detector::WindowChangeDetector::snapshot_without_suppression(
            prior_front,
        )
    } else {
        crate::window_change_detector::WindowChangeDetector::snapshot(prior_front)
    };
    let action_target = target.clone();
    // The lease rides into the blocking action and comes back with its
    // result, so a cancelled caller cannot release it while input still runs.
    let result = crate::focus_guard::with_focus_suppressed(
        (!foreground).then_some(pid),
        prior_front,
        "click.element_count",
        || async move {
            latency::spawn_action_blocking(move || {
                let held = lease;
                let outcome = run_blocking(&*action_target, idx, count, use_ax_open, foreground);
                (outcome, held)
            })
            .await
        },
    )
    .await;
    let changes = super::finish_window_observation(snapshot).await;
    match result {
        Ok((Ok(performed), _held)) => {
            if let Performed::Clicks(clicks) = &performed {
                let (x, y) = clicks.point.screen;
                state.cursor_registry.update_position(&cursor_key, x, y);
                state.cursor_registry.note_press(&cursor_key, x, y);
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::ClickPulse { x, y },
                );
            }
            performed_result(idx, window_id, count, &performed, &changes.result_suffix())
        }
        Ok((Err(error), _held)) => error_result(count, error),
        Err(error) => ToolResult::error(format!(
            "The click task failed ({error}); the clicks may have been sent. Read fresh state \
             before clicking again."
        ))
        .with_structured(json!({ "count": count, "effect": "unverifiable" })),
    }
}

fn target_for(
    pid: i32,
    window_id: u32,
    idx: usize,
    element: RetainedElement,
) -> Arc<dyn ElementTarget> {
    #[cfg(test)]
    if let Some(target) = test_targets::get(pid) {
        return target;
    }
    Arc::new(LiveTarget {
        pid,
        window_id,
        idx,
        element,
    })
}

struct LiveTarget {
    pid: i32,
    window_id: u32,
    idx: usize,
    element: RetainedElement,
}

impl LiveTarget {
    fn element(&self) -> AXUIElementRef {
        self.element.as_ptr() as AXUIElementRef
    }
}

#[async_trait]
impl ElementTarget for LiveTarget {
    async fn gate(&self, action: BackgroundAction) -> Result<BackgroundMutationLease, ToolResult> {
        super::gate_background_window_action(
            self.pid,
            self.window_id,
            Some(self.element.as_ptr()),
            action,
        )
        .await
    }

    fn web_select_refusal(&self) -> Option<ToolResult> {
        super::set_value::is_chromium_web_select(self.pid, self.window_id, self.element()).then(
            || {
                super::set_value::chromium_select_open_refusal(
                    self.pid,
                    self.window_id,
                    Some(self.idx),
                    self.element(),
                )
            },
        )
    }

    fn lists_ax_open(&self) -> bool {
        unsafe { crate::ax::bindings::copy_action_names(self.element()) }
            .iter()
            .any(|action| action == "AXOpen")
    }

    fn ax_open(&self) -> AXError {
        unsafe { crate::ax::bindings::perform_action(self.element(), "AXOpen") }
    }

    fn pointer_permitted(&self, foreground: bool) -> bool {
        use cua_driver_core::background_input::{
            decide_background_input, BackgroundInputDecision, ExactWindowTarget,
        };
        if foreground {
            return true;
        }
        let facts = crate::ax::exact_target::gather_background_facts(
            self.pid,
            self.window_id,
            Some(self.element.as_ptr()),
        );
        matches!(
            decide_background_input(
                ExactWindowTarget {
                    pid: self.pid,
                    window_id: self.window_id,
                },
                &facts,
                BackgroundAction::WindowPointer,
            ),
            BackgroundInputDecision::Execute { .. }
        )
    }

    fn element_rect(&self) -> Option<[f64; 4]> {
        unsafe { crate::ax::bindings::element_screen_rect(self.element()) }
    }

    fn window_bounds(&self) -> Option<[f64; 4]> {
        crate::windows::window_bounds_by_id(self.window_id)
            .map(|bounds| [bounds.x, bounds.y, bounds.width, bounds.height])
    }

    fn hit_at(&self, (x, y): (f64, f64)) -> Hit {
        let Some(hit) = (unsafe {
            crate::ax::bindings::element_at_screen_position_bounded(self.pid, x, y, 0.5)
        }) else {
            return Hit::Nothing;
        };
        let on_target =
            super::type_text::element_in_requested_window(self.pid, self.window_id, hit as usize)
                && unsafe { super::type_text::element_or_bounded_descendant(hit, self.element()) };
        let outcome = if on_target {
            Hit::Target
        } else {
            let role = unsafe { crate::ax::bindings::copy_string_attr(hit, "AXRole") };
            Hit::Elsewhere(role.unwrap_or_else(|| "an unnamed element".to_owned()))
        };
        unsafe { core_foundation::base::CFRelease(hit as _) };
        outcome
    }

    fn post(&self, point: &ElementPoint, count: usize, foreground: bool) -> anyhow::Result<()> {
        crate::input::mouse::click_at_xy_with_window_local(
            self.pid,
            point.screen.0,
            point.screen.1,
            point.window_local.0,
            point.window_local.1,
            self.window_id,
            count,
            &[],
            crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
        )
    }

    fn in_foreground(&self, body: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        crate::input::skylight::with_foreground_hid_activation(self.pid, self.window_id, body)
    }
}

#[cfg(test)]
pub(super) mod test_targets {
    use super::ElementTarget;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    fn targets() -> &'static Mutex<HashMap<i32, Arc<dyn ElementTarget>>> {
        static TARGETS: OnceLock<Mutex<HashMap<i32, Arc<dyn ElementTarget>>>> = OnceLock::new();
        TARGETS.get_or_init(Default::default)
    }

    pub(in crate::tools) fn get(pid: i32) -> Option<Arc<dyn ElementTarget>> {
        targets().lock().unwrap().get(&pid).cloned()
    }

    /// Serve `target` for `pid` until the guard drops.
    pub(in crate::tools) struct Registered(i32);

    pub(in crate::tools) fn register(pid: i32, target: Arc<dyn ElementTarget>) -> Registered {
        targets().lock().unwrap().insert(pid, target);
        Registered(pid)
    }

    impl Drop for Registered {
        fn drop(&mut self) {
            targets().lock().unwrap().remove(&self.0);
        }
    }
}

#[cfg(test)]
#[path = "element_pointer_tests.rs"]
mod tests;
