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

/// Most ancestors walked to find the target's own cell.
const CELL_ANCESTOR_HOPS: usize = 8;

/// Classifies a hit-test answer for the target. `target_ancestors` walks up
/// from the target's parent, giving each ancestor's role and whether it is the
/// hit element. Besides the target and its descendants, the target's nearest
/// AXCell counts: list apps such as KakaoTalk answer the hit test with the
/// cell for every point of a row, and a click there lands on the target's row.
pub(super) fn hit_outcome(
    in_window: bool,
    target_or_descendant: bool,
    hit_role: Option<String>,
    target_ancestors: impl IntoIterator<Item = (Option<String>, bool)>,
) -> Hit {
    if in_window && target_or_descendant {
        return Hit::Target;
    }
    if in_window && hit_role.as_deref() == Some("AXCell") {
        for (role, is_hit) in target_ancestors.into_iter().take(CELL_ANCESTOR_HOPS) {
            match role.as_deref() {
                Some("AXCell") if is_hit => return Hit::Target,
                // An unreadable role may hide the nearest cell, so stop there too.
                None | Some("AXCell" | "AXWindow" | "AXApplication") => break,
                _ => {}
            }
        }
    }
    Hit::Elsewhere(hit_role.unwrap_or_else(|| "an unnamed element".to_owned()))
}

/// The target's ancestors from its parent up, each with its role and whether
/// it is `hit`. Owns every parent it copies.
struct AncestorWalk {
    current: AXUIElementRef,
    owns_current: bool,
    hit: AXUIElementRef,
}

impl Iterator for AncestorWalk {
    type Item = (Option<String>, bool);

    fn next(&mut self) -> Option<Self::Item> {
        use core_foundation::base::{CFEqual, CFRelease, CFTypeRef};
        let parent = unsafe { crate::ax::bindings::copy_element_attr(self.current, "AXParent") }?;
        if self.owns_current {
            unsafe { CFRelease(self.current as CFTypeRef) };
        }
        self.current = parent;
        self.owns_current = true;
        let role = unsafe { crate::ax::bindings::copy_string_attr(parent, "AXRole") };
        let is_hit = unsafe { CFEqual(parent as CFTypeRef, self.hit as CFTypeRef) } != 0;
        Some((role, is_hit))
    }
}

impl Drop for AncestorWalk {
    fn drop(&mut self) {
        if self.owns_current {
            unsafe { core_foundation::base::CFRelease(self.current as _) };
        }
    }
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
    /// Hand `count` down/up pairs to the event transport, running
    /// `before_target` right before the first target down (after the pointer
    /// move and any primer). Every event is built before the first is posted,
    /// so any other error means nothing was handed over.
    fn post(
        &self,
        point: &ElementPoint,
        count: usize,
        foreground: bool,
        before_target: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<()>;
    /// Run `body` with the exact window frontmost and focused; refuse without
    /// running it when that cannot be proven.
    fn in_foreground(&self, body: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()>;
    /// The background pixel-click prologue: guard the user's key focus,
    /// remember their key window, and make the target AppKit-active without
    /// raising it (Chromium gates and remote-HID proxies drop clicks to an
    /// inactive window). Refuses, with nothing sent, while a key hand-back is
    /// pending or WindowServer refuses the connection.
    async fn background_pointer_prologue(
        &self,
        prior_front: Option<i32>,
    ) -> Result<PointerPrologue, ToolResult>;
    /// Hand focus back to the user's app and key window after the clicks.
    async fn restore_after_pointer(&self, prior_front: Option<i32>, prologue: PointerPrologue);
}

/// Whether the background prologue may go on, from its activation attempt:
/// Ok(None) when a pending key hand-back stopped it before any attempt,
/// Ok(Some(activated)) after an attempt, Err when its task failed.
///
/// Only the hand-back case proves nothing was sent; it is the one clean
/// refusal. An attempt that did not activate is uncertain, since part of the
/// focus record may have reached the window server: with the WindowServer
/// connection marked invalid (a process-wide flag that a previous call may
/// have set) the call stops as unverifiable, otherwise it goes on as an
/// uncertain activation. A failed task stops as unverifiable too.
pub(super) fn prologue_outcome(
    pid: i32,
    window_id: u32,
    attempt: Result<Option<bool>, String>,
    connection_invalid: impl FnOnce() -> bool,
) -> Result<bool, ToolResult> {
    let uncertain = |code: &str, reason: String| {
        ToolResult::error(format!(
            "click: {reason}. No click was sent.{} Read the window again before retrying.",
            Activation::BackgroundUncertain.note()
        ))
        .with_structured(json!({
            "code": code,
            "effect": "unverifiable",
            "clicks_posted": 0,
            "activation": Activation::BackgroundUncertain.label(),
            "pid": pid,
            "window_id": window_id,
        }))
    };
    match attempt {
        Ok(None) => Err(super::key_restore_pending_refusal(
            "click",
            Some(pid),
            Some(window_id),
        )),
        Ok(Some(true)) => Ok(true),
        Ok(Some(false)) if connection_invalid() => Err(uncertain(
            "windowserver_connection_invalid",
            "The WindowServer connection is marked invalid, so the clicks were not sent".to_owned(),
        )),
        Ok(Some(false)) => Ok(false),
        Err(error) => Err(uncertain(
            "background_unavailable",
            format!("The background activation task failed ({error})"),
        )),
    }
}

/// What the background prologue armed; held until focus is restored.
pub(super) struct PointerPrologue {
    pub(super) activated: bool,
    pub(super) prior_key_wid: Option<u32>,
    pub(super) key_focus: Option<crate::key_focus_guard::Lease>,
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
    // The pointer move (and the background primer) can change what is under
    // the point: prove it again right before the first target click.
    let mut recheck = || -> anyhow::Result<()> {
        match checked_point(target, foreground) {
            Ok(now) if now == point => Ok(()),
            Ok(_) => Err(refuse("element_moved", "the element moved").into()),
            Err(refusal) => Err(refusal.into()),
        }
    };
    if let Err(error) = target.post(&point, count, foreground, &mut recheck) {
        return Err(
            match error.downcast::<crate::input::mouse::TargetCheckFailed>() {
                Ok(failed) => TargetChanged {
                    code: failed
                        .source
                        .downcast_ref::<NotDispatched>()
                        .map_or("element_not_at_point", |refusal| refusal.code),
                    events_sent: failed.events_sent,
                    primer: !foreground,
                }
                .into(),
                Err(error) => refuse(
                    "pointer_events_unavailable",
                    format!("The click events could not be built ({error}); nothing was sent."),
                )
                .into(),
            },
        );
    }
    Ok(PointerClicks {
        point,
        primer: !foreground,
    })
}

/// The point stopped being on the element between the first check and the
/// first target click. The pointer move (and background primer) were handed
/// over; no click on the element was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TargetChanged {
    pub(super) code: &'static str,
    pub(super) events_sent: usize,
    pub(super) primer: bool,
}

impl std::fmt::Display for TargetChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sent = match (self.events_sent, self.primer) {
            (0, _) => "Nothing had been handed to the event transport yet",
            (_, true) => {
                "The pointer move and an off-screen primer click had already been handed to the \
                 event transport"
            }
            (_, false) => "The pointer move had already been handed to the event transport",
        };
        write!(
            f,
            "What is under the element's center changed right before the clicks ({}). {sent}; \
             no click on the element was sent. Read the window again before retrying.",
            self.code
        )
    }
}

impl std::error::Error for TargetChanged {}

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
            target,
            idx,
            use_ax_open,
            foreground,
            |target| target.ax_open(),
            |target| pointer_clicks(target, count, foreground),
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
    // Prove the point before fronting the window, so a bad point is refused
    // with nothing sent. It is proven again after activation.
    if !use_ax_open {
        checked_point(target, true)?;
    }
    let mut outcome = None;
    let activation = target.in_foreground(&mut || {
        outcome = Some(dispatch(target, idx, count, use_ax_open, true));
        Ok(())
    });
    // From here the window may have been fronted, keyed or raised, even when
    // activation then failed: no failure can claim that nothing was sent.
    let after = |source: anyhow::Error| -> anyhow::Error {
        AfterActivation {
            activation: Activation::Foreground,
            source,
        }
        .into()
    };
    match (outcome, activation) {
        (Some(Ok(performed)), _) => Ok(performed),
        (Some(Err(error)), _) => Err(after(error)),
        (None, Err(error)) => Err(after(
            refuse(
                "foreground_unavailable",
                format!("The window could not be proven frontmost ({error})"),
            )
            .into(),
        )),
        (None, Ok(())) => Err(after(anyhow::anyhow!(
            "foreground element click did not run"
        ))),
    }
}

/// What a multi-click did to the target window's activation before it
/// failed. Every failure result is built from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Activation {
    /// Nothing was attempted.
    None,
    /// The background prologue made the target window active (a focus
    /// record was posted).
    Background,
    /// The background prologue tried and could not confirm it; part of it
    /// may have reached the window server.
    BackgroundUncertain,
    /// The foreground helper ran: the window may have been fronted, keyed or
    /// raised.
    Foreground,
}

impl Activation {
    fn label(self) -> &'static str {
        match self {
            Activation::None => "none",
            Activation::Background => "background",
            Activation::BackgroundUncertain => "background_uncertain",
            Activation::Foreground => "foreground_attempted",
        }
    }

    /// The sentence a failure result adds about it.
    fn note(self) -> &'static str {
        match self {
            Activation::None => "",
            Activation::Background => {
                " The target window had already been made active in the background."
            }
            Activation::BackgroundUncertain => {
                " An attempt to make the target window active in the background had already \
                 been made and may have reached it."
            }
            Activation::Foreground => " The window may already have been brought to the front.",
        }
    }

    /// Whether something was certainly sent (true), certainly not (false),
    /// or cannot be known (None).
    fn dispatched(self) -> Option<bool> {
        match self {
            Activation::None => Some(false),
            Activation::Background => Some(true),
            Activation::BackgroundUncertain | Activation::Foreground => None,
        }
    }
}

/// A failure that happened after an activation attempt.
#[derive(Debug)]
pub(super) struct AfterActivation {
    pub(super) activation: Activation,
    pub(super) source: anyhow::Error,
}

impl std::fmt::Display for AfterActivation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (activation: {})",
            self.source,
            self.activation.label()
        )
    }
}

impl std::error::Error for AfterActivation {}

/// The public result of a multi-click.
pub(super) fn performed_result(
    idx: usize,
    window_id: u32,
    count: usize,
    foreground: bool,
    performed: &Performed,
    suffix: &str,
) -> ToolResult {
    use cua_driver_core::action_record::{
        ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
    };
    let (requested, actual) = if foreground {
        (RequestedDelivery::Foreground, ActualDelivery::Foreground)
    } else {
        (RequestedDelivery::Background, ActualDelivery::Background)
    };
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
        }))
        .with_action_record(
            ActionExecutionRecord::builder(
                ActionEffect::Unverifiable,
                ActionTransport::MacosAxAction,
                requested,
            )
            .actual_delivery(actual)
            .build()
            .expect("AXOpen record is valid"),
        ),
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
            // The public wire reads the target clicks from delivered_count.
            .with_action_record(
                ActionExecutionRecord::builder(
                    ActionEffect::Unverifiable,
                    ActionTransport::MacosCgEventPid,
                    requested,
                )
                .actual_delivery(actual)
                .delivered_count(count as u32)
                .build()
                .expect("pointer click record is valid"),
            )
        }
    }
}

/// The result of a multi-click that sent no click on the element, built
/// from the events and the activation that had already gone out.
fn error_result(count: usize, error: anyhow::Error) -> ToolResult {
    let (activation, error) = match error.downcast::<AfterActivation>() {
        Ok(after) => (after.activation, after.source),
        Err(error) => (Activation::None, error),
    };
    let with_dispatched = |mut structured: Value, dispatched: Option<bool>| {
        if let Some(dispatched) = dispatched {
            structured["dispatched"] = json!(dispatched);
        }
        structured
    };
    if let Some(changed) = error.downcast_ref::<TargetChanged>() {
        let dispatched = if changed.events_sent > 0 {
            Some(true)
        } else {
            activation.dispatched()
        };
        return ToolResult::error(format!("click: {changed}{}", activation.note()))
            .with_structured(with_dispatched(
                json!({
                    "code": "element_changed_before_click",
                    "reason": changed.code,
                    "effect": if dispatched == Some(false) { "refused" } else { "unverifiable" },
                    "count": count,
                    "clicks_posted": 0,
                    "primer_clicks": if changed.events_sent > 0 { u8::from(changed.primer) } else { 0 },
                    "events_sent": changed.events_sent,
                    "activation": activation.label(),
                }),
                dispatched,
            ));
    }
    if let Some(refusal) = error.downcast_ref::<NotDispatched>() {
        if activation == Activation::None {
            let mut result = super::pre_dispatch_refusal(refusal.code, format!("click: {refusal}"));
            if let Some(structured) = result.structured_content.as_mut() {
                structured["count"] = json!(count);
                structured["clicks_posted"] = json!(0);
                structured["activation"] = json!(activation.label());
            }
            return result;
        }
        return ToolResult::error(format!(
            "click: {}. No click was sent.{} Read the window again before retrying.",
            refusal
                .message
                .trim_end_matches("; nothing was sent.")
                .trim_end_matches('.'),
            activation.note()
        ))
        .with_structured(with_dispatched(
            json!({
                "code": refusal.code,
                "effect": "unverifiable",
                "count": count,
                "clicks_posted": 0,
                "activation": activation.label(),
            }),
            activation.dispatched(),
        ));
    }
    ToolResult::error(format!(
        "click (count {count}) failed: {error}.{}",
        activation.note()
    ))
    .with_structured(json!({
        "count": count,
        "clicks_posted": 0,
        "activation": activation.label(),
    }))
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
    let probe_target = element.clone();
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
    let use_ax_open = super::actionability::may_use_semantic_route(
        &cua_driver_core::actionability::Evidence::Known(use_ax_open),
    );
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

    // The action runs on its own task: a cancelled caller drops only the
    // handle, while the lease, the focus-steal guards and the window
    // observation stay armed until the input has finished.
    let action_target = target.clone();
    let cursor_registry = state.cursor_registry.clone();
    // A background pointer gesture lets the target become AppKit-active
    // without raising (the pixel-click prologue); AXOpen keeps it suppressed.
    let background_pointer = !foreground && !use_ax_open;
    let clock = super::actionability::semantic_clock();
    let perform = |lease| async move {
        crate::input::worker_context::spawn_async(async move {
            use crate::window_change_detector::WindowChangeDetector;
            let prior_front = crate::apps::frontmost_pid();
            let snapshot = if foreground {
                WindowChangeDetector::snapshot_without_suppression(prior_front)
            } else if background_pointer {
                WindowChangeDetector::snapshot_allowing_activation(prior_front, pid)
            } else {
                WindowChangeDetector::snapshot(prior_front)
            };
            let prologue = if background_pointer {
                // Prove the point before the prologue sends a focus record, so a
                // bad point is refused with nothing sent; it is proven again
                // after activation and right before the first click.
                let preflight_target = action_target.clone();
                match latency::spawn_action_blocking(move || {
                    checked_point(&*preflight_target, false).map(|_| ())
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(refusal)) => return Err(error_result(count, refusal.into())),
                    Err(error) => {
                        return Err(super::pre_dispatch_refusal(
                            "background_unavailable",
                            format!("The point check task failed ({error}); nothing was sent."),
                        ))
                    }
                }
                match action_target.background_pointer_prologue(prior_front).await {
                    Ok(prologue) => Some(prologue),
                    Err(mut refusal) => {
                        if let Some(structured) = refusal.structured_content.as_mut() {
                            structured["count"] = json!(count);
                            structured["clicks_posted"] = json!(0);
                        }
                        return Err(refusal);
                    }
                }
            } else {
                None
            };
            let activation = match &prologue {
                None => Activation::None,
                Some(prologue) if prologue.activated => Activation::Background,
                Some(_) => Activation::BackgroundUncertain,
            };
            let blocking_target = action_target.clone();
            let result = crate::focus_guard::with_focus_suppressed(
                (!foreground && !background_pointer).then_some(pid),
                prior_front,
                "click.element_count",
                || async move {
                    latency::spawn_action_blocking(move || {
                        let held = lease;
                        let outcome =
                            run_blocking(&*blocking_target, idx, count, use_ax_open, foreground);
                        (outcome, held)
                    })
                    .await
                },
            )
            .await;
            if let Some(prologue) = prologue {
                action_target
                    .restore_after_pointer(prior_front, prologue)
                    .await;
            }
            let changes = super::finish_window_observation(snapshot).await;
            // The cursor follows clicks that went out, even for a cancelled
            // caller; it moves before the lease is released below.
            if let Ok((Ok(Performed::Clicks(clicks)), _held)) = &result {
                let (x, y) = clicks.point.screen;
                cursor_registry.update_position(&cursor_key, x, y);
                cursor_registry.note_press(&cursor_key, x, y);
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::ClickPulse { x, y },
                );
            }
            // Background failures after the prologue carry what it sent; the
            // foreground route tags its own.
            let result = result.map(|(outcome, _held)| {
                outcome.map_err(|source| {
                    if activation == Activation::None || source.is::<AfterActivation>() {
                        source
                    } else {
                        AfterActivation { activation, source }.into()
                    }
                })
            });
            Ok((result, changes.result_suffix()))
        })
        .await
    };
    let action = super::double_click::dispatch_open(
        &clock,
        cua_driver_core::actionability::DEFAULT_BUDGET,
        use_ax_open,
        lease,
        |lease, _| {
            super::actionability::semantic_probe(
                lease.as_ref(),
                probe_target.clone(),
                pid,
                window_id,
                super::actionability::semantic::SemanticIntent::Open,
            )
        },
        perform,
    )
    .await;
    let action = match action {
        Ok(action) => action,
        Err(refusal) => return refusal,
    };
    let (result, suffix) = match action {
        Ok(Ok(done)) => done,
        Ok(Err(refusal)) => return refusal,
        Err(error) => {
            return ToolResult::error(format!(
                "The click task failed ({error}); the clicks may have been sent. Read fresh \
                 state before clicking again."
            ))
            .with_structured(json!({ "count": count, "effect": "unverifiable" }))
        }
    };
    match result {
        Ok(Ok(performed)) => {
            performed_result(idx, window_id, count, foreground, &performed, &suffix)
        }
        Ok(Err(error)) => error_result(count, error),
        Err(error) => ToolResult::error(format!(
            "The click task failed ({error}); the clicks may have been sent. Read fresh state \
             before clicking again."
        ))
        .with_structured(json!({ "count": count, "effect": "unverifiable" })),
    }
}

pub(super) fn target_for(
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
        let in_window =
            super::type_text::element_in_requested_window(self.pid, self.window_id, hit as usize);
        let target_or_descendant = in_window
            && unsafe { super::type_text::element_or_bounded_descendant(hit, self.element()) };
        let hit_role = if target_or_descendant {
            None
        } else {
            unsafe { crate::ax::bindings::copy_string_attr(hit, "AXRole") }
        };
        let ancestors = AncestorWalk {
            current: self.element(),
            owns_current: false,
            hit,
        };
        let outcome = hit_outcome(in_window, target_or_descendant, hit_role, ancestors);
        unsafe { core_foundation::base::CFRelease(hit as _) };
        outcome
    }

    fn post(
        &self,
        point: &ElementPoint,
        count: usize,
        foreground: bool,
        before_target: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        crate::input::mouse::click_at_xy_with_window_local_checked(
            self.pid,
            point.screen,
            point.window_local,
            self.window_id,
            count,
            &[],
            crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
            before_target,
        )
    }

    fn in_foreground(&self, body: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        crate::input::skylight::with_foreground_hid_activation(self.pid, self.window_id, body)
    }

    async fn background_pointer_prologue(
        &self,
        prior_front: Option<i32>,
    ) -> Result<PointerPrologue, ToolResult> {
        let (pid, wid) = (self.pid, self.window_id);
        // The same prologue as a background pixel click (click.rs).
        let key_focus = latency::spawn_blocking(move || crate::key_focus_guard::protect(pid))
            .await
            .ok()
            .flatten();
        let prior_key_wid = match prior_front {
            Some(prior) if prior != pid => latency::spawn_action_blocking(move || {
                crate::input::skylight::key_window_of_pid(prior)
            })
            .await
            .ok()
            .flatten(),
            _ => None,
        };
        let attempt = latency::spawn_action_blocking(move || {
            crate::input::skylight::await_key_restores()
                .then(|| crate::input::mouse::prepare_background_pixel_click(pid, wid))
        })
        .await
        .map_err(|error| error.to_string());
        let activated = prologue_outcome(pid, wid, attempt, || {
            crate::input::skylight::windowserver_connection_invalid()
        })?;
        Ok(PointerPrologue {
            activated,
            prior_key_wid,
            key_focus,
        })
    }

    async fn restore_after_pointer(&self, prior_front: Option<i32>, prologue: PointerPrologue) {
        super::pixel_focus::restore_user_focus_after_background_pixel(
            self.pid,
            prior_front,
            prologue.prior_key_wid,
            Some(self.window_id),
            prologue.activated,
        )
        .await;
        drop(prologue.key_focus);
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
