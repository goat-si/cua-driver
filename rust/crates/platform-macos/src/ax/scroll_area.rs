//! The scroll area under a background scroll target, for confirming that a
//! wheel moved it and for scrolling it through accessibility when it did not.
//!
//! Some windows expose a real AXScrollArea with page actions and a settable
//! scroll bar but ignore pid-targeted wheel events while in the background;
//! System Settings' SwiftUI panes are one (macOS 26). Reading the scroll bar
//! before and after the wheel tells the two cases apart, and the page actions
//! scroll without posting any event or fronting the window.

use core_foundation::base::{CFRelease, CFRetain, CFTypeRef};

use super::bindings::{
    ax_get_window_id, copy_action_names, copy_ax_windows, copy_children, copy_element_attr,
    copy_number_attr, copy_string_attr, element_at_screen_position, element_screen_rect,
    is_attribute_settable, kAXErrorSuccess, perform_action, set_number_attr,
    AXUIElementCopyElementAtPosition, AXUIElementCreateApplication, AXUIElementRef,
    AXUIElementSetMessagingTimeout,
};
use std::time::{Duration, Instant};

const MAX_ASCENT: usize = 40;
const MAX_NODES: usize = 4_000;
const MAX_DEPTH: usize = 60;
/// Scroll bar values are fractions of the scrollable range.
const EPSILON: f64 = 1e-4;

/// The page action that scrolls content in `direction`.
pub fn page_action(direction: &str) -> Option<&'static str> {
    match direction {
        "down" => Some("AXScrollDownByPage"),
        "up" => Some("AXScrollUpByPage"),
        "right" => Some("AXScrollRightByPage"),
        "left" => Some("AXScrollLeftByPage"),
        _ => None,
    }
}

/// The scroll bar attribute whose value moves when scrolling in `direction`.
fn bar_attribute(direction: &str) -> &'static str {
    if matches!(direction, "left" | "right") {
        "AXHorizontalScrollBar"
    } else {
        "AXVerticalScrollBar"
    }
}

/// How far the bar went from before to after in direction's sense: positive
/// toward the end direction scrolls to (down/right raise the value, up/left
/// lower it), negative the other way.
pub fn signed_progress(direction: &str, before: f64, after: f64) -> f64 {
    match direction {
        "up" | "left" => before - after,
        _ => after - before,
    }
}

/// Whether the bar moved toward where direction scrolls, beyond rounding.
/// Movement the other way is not this scroll.
pub fn moved_toward(direction: &str, before: f64, after: f64) -> bool {
    signed_progress(direction, before, after) > EPSILON
}

/// Whether a scroll bar went from `before` to `after`.
pub fn moved(before: f64, after: f64) -> bool {
    (after - before).abs() > EPSILON
}

/// The value, 0 to 1, of `area`'s scroll bar along `direction`'s axis, or
/// None when it has no bar there.
///
/// # Safety
///
/// `area` must be a live AX element. Call from a blocking context.
pub unsafe fn bar_position(area: AXUIElementRef, direction: &str) -> Option<f64> {
    let bar = copy_element_attr(area, bar_attribute(direction))?;
    let value = scrollbar_value(
        || copy_string_attr(bar, "AXRole"),
        || copy_number_attr(bar, "AXValue"),
    );
    CFRelease(bar as CFTypeRef);
    value.filter(|value| value.is_finite())
}

pub(super) fn scrollbar_value(
    role: impl FnOnce() -> Option<String>,
    value: impl FnOnce() -> Option<f64>,
) -> Option<f64> {
    (role().as_deref() == Some("AXScrollBar"))
        .then(value)
        .flatten()
}

/// How a scroll area shows in a tree row: where each of its bars sits. None
/// without a bar, which means it cannot scroll.
pub fn position_label(vertical: Option<f64>, horizontal: Option<f64>) -> Option<String> {
    let percent = |value: f64| (value.clamp(0.0, 1.0) * 100.0).round() as i64;
    match (vertical, horizontal) {
        (Some(v), Some(h)) => Some(format!(
            "scrolled {}% down, {}% right",
            percent(v),
            percent(h)
        )),
        (Some(v), None) => Some(format!("scrolled {}% down", percent(v))),
        (None, Some(h)) => Some(format!("scrolled {}% right", percent(h))),
        (None, None) => None,
    }
}

/// Whether `position` already sits at the end `direction` scrolls toward,
/// so a scroll that changes nothing is expected rather than dropped.
pub fn at_end(direction: &str, position: f64) -> bool {
    match direction {
        "down" | "right" => position >= 1.0 - EPSILON,
        _ => position <= EPSILON,
    }
}

/// How many page actions stand in for a wheel of `distance` points over a
/// viewport `extent` points long: at least one, never more than `cap`.
pub fn pages_for(distance: f64, extent: f64, cap: usize) -> usize {
    let positive = |value: f64| value.is_finite() && value > 0.0;
    if !positive(extent) || !positive(distance) {
        return 1;
    }
    ((distance / extent).ceil() as usize).clamp(1, cap.max(1))
}

/// The scroll bar value `distance` points further in `direction`, for
/// content `content` points long shown in a viewport `viewport` points long.
/// `None` when nothing is scrollable along that axis.
pub fn position_after(
    direction: &str,
    position: f64,
    distance: f64,
    content: f64,
    viewport: f64,
) -> Option<f64> {
    let scrollable = content - viewport;
    if scrollable.is_nan() || scrollable <= 1.0 {
        return None;
    }
    let step = distance / scrollable;
    let next = match direction {
        "down" | "right" => position + step,
        _ => position - step,
    };
    Some(next.clamp(0.0, 1.0))
}

/// A retained AXScrollArea, released on drop.
pub struct ScrollArea {
    element: AXUIElementRef,
}

// The reference is only used from blocking AX contexts, one call at a time.
unsafe impl Send for ScrollArea {}

impl Drop for ScrollArea {
    fn drop(&mut self) {
        unsafe { CFRelease(self.element as CFTypeRef) };
    }
}

impl ScrollArea {
    /// The scroll area holding `element`, else the one under `point` in
    /// `pid`'s window `window_id`: hit-testing first, then by frame, since
    /// a background window can be covered at that point by another app.
    ///
    /// # Safety
    ///
    /// `element`, when given, must be a live AX element of `pid`. Call from a
    /// blocking context.
    pub unsafe fn find(
        pid: i32,
        element: Option<AXUIElementRef>,
        point: (f64, f64),
        window_id: Option<u32>,
    ) -> Option<Self> {
        Self::find_inner(pid, element, point, window_id, None)
    }

    unsafe fn find_inner(
        pid: i32,
        element: Option<AXUIElementRef>,
        point: (f64, f64),
        window_id: Option<u32>,
        budget: Option<&Budget>,
    ) -> Option<Self> {
        let steps: Vec<Box<dyn FnOnce() -> Option<Self> + '_>> = vec![
            Box::new(move || {
                let element = element?;
                CFRetain(element as CFTypeRef);
                ascend(element, budget)
            }),
            Box::new(move || {
                let hit = hit_within(pid, point, budget)?;
                ascend(hit, budget)
            }),
            Box::new(move || by_frame(pid, point, window_id, budget)),
        ];
        first_found(|| budget.is_some_and(|budget| budget.expired()), steps)
    }

    /// The scroll bar's value along `direction`'s axis, from 0 to 1.
    ///
    /// # Safety
    ///
    /// Call from a blocking context.
    pub unsafe fn position(&self, direction: &str) -> Option<f64> {
        let bar = copy_element_attr(self.element, bar_attribute(direction))?;
        let value = copy_number_attr(bar, "AXValue");
        CFRelease(bar as CFTypeRef);
        value.filter(|value| value.is_finite())
    }

    /// The visible extent along `direction`'s axis, in points.
    ///
    /// # Safety
    ///
    /// Call from a blocking context.
    pub unsafe fn extent(&self, direction: &str) -> Option<f64> {
        let [_, _, width, height] = element_screen_rect(self.element)?;
        Some(if matches!(direction, "left" | "right") {
            width
        } else {
            height
        })
    }
}

/// The nearest AXScrollArea at or above `element`, consuming its retain.
unsafe fn ascend(element: AXUIElementRef, budget: Option<&Budget>) -> Option<ScrollArea> {
    climb(
        element,
        MAX_ASCENT,
        || budget.is_some_and(|budget| budget.expired()),
        |current| {
            if budget.is_some() {
                bound(*current);
            }
            copy_string_attr(*current, "AXRole")
        },
        |current| copy_element_attr(*current, "AXParent"),
        |current| CFRelease(current as CFTypeRef),
    )
    .map(|element| ScrollArea { element })
}

/// Climbs from start (owned) to the nearest AXScrollArea, releasing every
/// element it leaves; none past an AXWindow/AXApplication, a role it cannot
/// read, max steps, or the deadline, which is checked before each read.
pub fn climb<E>(
    start: E,
    max: usize,
    expired: impl Fn() -> bool,
    role: impl Fn(&E) -> Option<String>,
    parent: impl Fn(&E) -> Option<E>,
    release: impl Fn(E),
) -> Option<E> {
    let mut current = start;
    for _ in 0..max {
        if expired() {
            break;
        }
        match role(&current).as_deref() {
            Some("AXScrollArea") => return Some(current),
            Some("AXWindow") | Some("AXApplication") | None => break,
            Some(_) => {}
        }
        if expired() {
            break;
        }
        let next = parent(&current);
        release(current);
        current = next?;
    }
    release(current);
    None
}

/// The element of pid at point, with the application element bounded by
/// WHEEL_AX_TIMEOUT_S when a budget applies.
unsafe fn hit_within(
    pid: i32,
    point: (f64, f64),
    budget: Option<&Budget>,
) -> Option<AXUIElementRef> {
    if budget.is_none() {
        return element_at_screen_position(pid, point.0, point.1);
    }
    let application = AXUIElementCreateApplication(pid);
    if application.is_null() {
        return None;
    }
    bound(application);
    let mut element = std::ptr::null_mut();
    let error =
        AXUIElementCopyElementAtPosition(application, point.0 as f32, point.1 as f32, &mut element);
    CFRelease(application as CFTypeRef);
    (error == kAXErrorSuccess && !element.is_null()).then_some(element)
}

/// Runs the probe steps in order until one finds something; none starts
/// once expired is true.
pub fn first_found<'a, T>(
    expired: impl Fn() -> bool,
    steps: Vec<Box<dyn FnOnce() -> Option<T> + 'a>>,
) -> Option<T> {
    for step in steps {
        if expired() {
            return None;
        }
        if let Some(found) = step() {
            return Some(found);
        }
    }
    None
}

/// The smallest AXScrollArea in `pid`'s window whose frame holds `point`.
unsafe fn by_frame(
    pid: i32,
    point: (f64, f64),
    window_id: Option<u32>,
    budget: Option<&Budget>,
) -> Option<ScrollArea> {
    let application = AXUIElementCreateApplication(pid);
    if application.is_null() {
        return None;
    }
    if budget.is_some() {
        bound(application);
    }
    let windows = copy_ax_windows(application);
    CFRelease(application as CFTypeRef);
    let mut best: Option<(AXUIElementRef, f64)> = None;
    let mut visited = 0;
    for window in windows {
        let expired = budget.is_some_and(|budget| budget.expired());
        if !expired {
            if budget.is_some() {
                bound(window);
            }
            if window_id.is_none() || ax_get_window_id(window) == window_id {
                smallest_holding(window, point, 0, &mut visited, &mut best, budget);
            }
        }
        CFRelease(window as CFTypeRef);
    }
    best.map(|(element, _)| ScrollArea { element })
}

unsafe fn smallest_holding(
    element: AXUIElementRef,
    point: (f64, f64),
    depth: usize,
    visited: &mut usize,
    best: &mut Option<(AXUIElementRef, f64)>,
    budget: Option<&Budget>,
) {
    if depth > MAX_DEPTH || *visited >= MAX_NODES {
        return;
    }
    if let Some(budget) = budget {
        if budget.expired() {
            return;
        }
        bound(element);
    }
    *visited += 1;
    let expired = || budget.is_some_and(|budget| budget.expired());
    if copy_string_attr(element, "AXRole").as_deref() == Some("AXScrollArea") && !expired() {
        if let Some([x, y, width, height]) = element_screen_rect(element) {
            let holds =
                point.0 >= x && point.0 <= x + width && point.1 >= y && point.1 <= y + height;
            let area = width * height;
            if holds && best.as_ref().is_none_or(|(_, smallest)| area < *smallest) {
                CFRetain(element as CFTypeRef);
                if let Some((previous, _)) = best.replace((element, area)) {
                    CFRelease(previous as CFTypeRef);
                }
            }
        }
    }
    if expired() {
        return;
    }
    for child in copy_children(element) {
        smallest_holding(child, point, depth + 1, visited, best, budget);
        CFRelease(child as CFTypeRef);
    }
}

// ── The background wheel's verdict path: bounded in time ─────────────────────

/// Per-element accessibility messaging timeout on the background wheel's
/// verdict path. The system default is 6 s, so one read of a busy app's
/// element could hold a scroll for that long.
pub const WHEEL_AX_TIMEOUT_S: f32 = 0.25;

/// The time source of the wheel verdict, faked in tests.
pub trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

/// The real clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// A deadline that every accessibility call and pause on the wheel's verdict
/// path checks first.
#[derive(Clone, Copy)]
pub struct Budget<'a> {
    clock: &'a dyn Clock,
    deadline: Instant,
}

impl<'a> Budget<'a> {
    /// A budget of `length` from now.
    pub fn new(clock: &'a dyn Clock, length: Duration) -> Self {
        Self {
            clock,
            deadline: clock.now() + length,
        }
    }

    /// This budget, ending no later than `length` from now.
    pub fn sooner(&self, length: Duration) -> Self {
        Self {
            clock: self.clock,
            deadline: self.deadline.min(self.clock.now() + length),
        }
    }

    /// Time left before the deadline.
    pub fn left(&self) -> Duration {
        self.deadline.saturating_duration_since(self.clock.now())
    }

    /// The deadline has passed.
    pub fn expired(&self) -> bool {
        self.left().is_zero()
    }

    /// Sleeps for `duration`, or for what is left when that is shorter.
    pub fn pause(&self, duration: Duration) {
        let duration = duration.min(self.left());
        if !duration.is_zero() {
            self.clock.sleep(duration);
        }
    }
}

/// What a scroll mutation through accessibility did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// Nothing was dispatched: the area does not offer it, a precondition
    /// failed, or the budget ran out first.
    NotSent,
    /// Dispatched; the app reported success. `count` is how many actions
    /// went out (page actions), 1 otherwise.
    Sent { count: usize },
    /// Dispatched; the app reported an error or did not answer in time. It
    /// may still apply it later.
    Failed { count: usize },
}

/// The operations the wheel verdict needs from a scroll area. Every one
/// checks `budget` before each accessibility call and gives up (`None`,
/// `false`, [`Mutation::NotSent`]) when it has run out.
pub trait WheelArea {
    /// The bar's value along `direction`'s axis, 0 to 1.
    fn position(&self, direction: &str, budget: &Budget) -> Option<f64>;
    /// The bar along `direction`'s axis takes an AXValue write.
    fn bar_settable(&self, direction: &str, budget: &Budget) -> bool;
    /// Moves the bar about `distance` points by writing its value.
    fn set_position(&self, direction: &str, distance: f64, budget: &Budget) -> Mutation;
    /// Reveals the content about `distance` points past the edge.
    fn reveal(
        &self,
        window: Option<[f64; 4]>,
        direction: &str,
        distance: f64,
        budget: &Budget,
    ) -> Mutation;
    /// Performs `direction`'s page action up to `pages` times.
    fn page(&self, direction: &str, pages: usize, budget: &Budget) -> Mutation;
    /// The visible extent along the axis, in points.
    fn extent(&self, direction: &str, budget: &Budget) -> Option<f64>;
    /// `distance` points as a change of the bar's 0..1 value: distance over
    /// the scrollable length (content minus viewport). `None` when either
    /// cannot be measured or nothing scrolls along that axis.
    fn bar_units(&self, direction: &str, distance: f64, budget: &Budget) -> Option<f64>;
}

/// `distance` points as a change of a bar's 0..1 value, for content
/// `content` points long in a viewport `viewport` points long.
pub fn bar_units_for(distance: f64, content: f64, viewport: f64) -> Option<f64> {
    let scrollable = content - viewport;
    (scrollable.is_finite() && scrollable > 1.0 && distance.is_finite() && distance > 0.0)
        .then(|| distance / scrollable)
}

/// The span along one axis of child frames read lazily from `frames`:
/// the content's length. `None` when `expired` turns true before every
/// frame was read, or no child had a frame.
pub fn span_of(
    mut frames: impl Iterator<Item = Option<[f64; 4]>>,
    horizontal: bool,
    mut expired: impl FnMut() -> bool,
) -> Option<f64> {
    let mut span: Option<(f64, f64)> = None;
    loop {
        if expired() {
            return None;
        }
        let Some(frame) = frames.next() else {
            break;
        };
        if let Some([x, y, width, height]) = frame {
            let (start, length) = if horizontal { (x, width) } else { (y, height) };
            span = Some(match span {
                Some((low, high)) => (low.min(start), high.max(start + length)),
                None => (start, start + length),
            });
        }
    }
    span.map(|(low, high)| high - low)
}

/// A scroll bar write, step by step: read the bar, the viewport and the
/// content, compute the target, check the bar takes a value, write. The
/// budget is checked before every step, the write last, so nothing is
/// written once it has run out.
pub fn write_bar(
    expired: &dyn Fn() -> bool,
    position: impl FnOnce() -> Option<f64>,
    viewport: impl FnOnce() -> Option<f64>,
    content: impl FnOnce() -> Option<f64>,
    target: impl FnOnce(f64, f64, f64) -> Option<f64>,
    settable: impl FnOnce() -> bool,
    write: impl FnOnce(f64) -> bool,
) -> Mutation {
    if expired() {
        return Mutation::NotSent;
    }
    let Some(position) = position() else {
        return Mutation::NotSent;
    };
    if expired() {
        return Mutation::NotSent;
    }
    let Some(viewport) = viewport() else {
        return Mutation::NotSent;
    };
    if expired() {
        return Mutation::NotSent;
    }
    let Some(content) = content() else {
        return Mutation::NotSent;
    };
    let Some(next) = target(position, viewport, content) else {
        return Mutation::NotSent;
    };
    if expired() || !settable() || expired() {
        return Mutation::NotSent;
    }
    if write(next) {
        Mutation::Sent { count: 1 }
    } else {
        Mutation::Failed { count: 1 }
    }
}

/// Up to pages page actions, each only with budget left; an action the app
/// answers with an error ends the ladder as Failed (it may still apply).
pub fn page_ladder(
    expired: impl Fn() -> bool,
    pages: usize,
    mut perform: impl FnMut() -> bool,
    mut pause: impl FnMut(),
) -> Mutation {
    let mut count = 0;
    for _ in 0..pages.max(1) {
        if expired() {
            break;
        }
        count += 1;
        if !perform() {
            return Mutation::Failed { count };
        }
        pause();
    }
    if count == 0 {
        Mutation::NotSent
    } else {
        Mutation::Sent { count }
    }
}

/// Bounds `element`'s accessibility calls on the wheel's verdict path.
unsafe fn bound(element: AXUIElementRef) {
    AXUIElementSetMessagingTimeout(element, WHEEL_AX_TIMEOUT_S);
}

impl ScrollArea {
    /// [`ScrollArea::find`] for the background wheel: every element it
    /// touches gets [`WHEEL_AX_TIMEOUT_S`] and the frame walk stops at
    /// `budget`, so a busy app cannot hold the probe. The area returned is
    /// bounded the same way.
    ///
    /// # Safety
    ///
    /// As [`ScrollArea::find`].
    pub unsafe fn find_within(
        pid: i32,
        element: Option<AXUIElementRef>,
        point: (f64, f64),
        window_id: Option<u32>,
        budget: &Budget,
    ) -> Option<Self> {
        let area = Self::find_inner(pid, element, point, window_id, Some(budget))?;
        bound(area.element);
        (!budget.expired()).then_some(area)
    }

    /// The bar's value, bounded by `budget`.
    unsafe fn position_within(&self, direction: &str, budget: &Budget) -> Option<f64> {
        if budget.expired() {
            return None;
        }
        let bar = copy_element_attr(self.element, bar_attribute(direction))?;
        bound(bar);
        let value = (!budget.expired())
            .then(|| copy_number_attr(bar, "AXValue"))
            .flatten();
        CFRelease(bar as CFTypeRef);
        value.filter(|value| value.is_finite())
    }

    unsafe fn content_length_within(&self, horizontal: bool, budget: &Budget) -> Option<f64> {
        if budget.expired() {
            return None;
        }
        let children = copy_children(self.element);
        // span_of checks the budget before each child; the frame read after a
        // child's role read checks it again.
        let frames = children.iter().map(|&child| {
            bound(child);
            if copy_string_attr(child, "AXRole").as_deref() == Some("AXScrollBar")
                || budget.expired()
            {
                None
            } else {
                element_screen_rect(child)
            }
        });
        let span = span_of(frames, horizontal, || budget.expired());
        for child in children {
            CFRelease(child as CFTypeRef);
        }
        span
    }
}

impl WheelArea for ScrollArea {
    fn position(&self, direction: &str, budget: &Budget) -> Option<f64> {
        unsafe { self.position_within(direction, budget) }
    }

    fn bar_settable(&self, direction: &str, budget: &Budget) -> bool {
        if budget.expired() {
            return false;
        }
        unsafe {
            let Some(bar) = copy_element_attr(self.element, bar_attribute(direction)) else {
                return false;
            };
            bound(bar);
            let settable = !budget.expired() && is_attribute_settable(bar, "AXValue");
            CFRelease(bar as CFTypeRef);
            settable
        }
    }

    fn set_position(&self, direction: &str, distance: f64, budget: &Budget) -> Mutation {
        let horizontal = matches!(direction, "left" | "right");
        if budget.expired() {
            return Mutation::NotSent;
        }
        unsafe {
            let Some(bar) = copy_element_attr(self.element, bar_attribute(direction)) else {
                return Mutation::NotSent;
            };
            bound(bar);
            let mutation = write_bar(
                &|| budget.expired(),
                || copy_number_attr(bar, "AXValue").filter(|value| value.is_finite()),
                || WheelArea::extent(self, direction, budget),
                || self.content_length_within(horizontal, budget),
                |position, viewport, content| {
                    position_after(direction, position, distance, content, viewport)
                },
                || is_attribute_settable(bar, "AXValue"),
                |next| set_number_attr(bar, "AXValue", next) == kAXErrorSuccess,
            );
            CFRelease(bar as CFTypeRef);
            mutation
        }
    }

    fn reveal(
        &self,
        window: Option<[f64; 4]>,
        direction: &str,
        distance: f64,
        budget: &Budget,
    ) -> Mutation {
        unsafe {
            super::reveal_scroll::reveal_attempt(
                self.element,
                "AXScrollArea",
                window,
                direction,
                distance,
                super::reveal_scroll::RevealLimits {
                    budget: Some(*budget),
                    element_timeout: Some(WHEEL_AX_TIMEOUT_S),
                    proceed: None,
                },
            )
        }
    }

    fn page(&self, direction: &str, pages: usize, budget: &Budget) -> Mutation {
        let Some(action) = page_action(direction) else {
            return Mutation::NotSent;
        };
        if budget.expired() {
            return Mutation::NotSent;
        }
        unsafe {
            if !copy_action_names(self.element)
                .iter()
                .any(|name| name == action)
            {
                return Mutation::NotSent;
            }
            page_ladder(
                || budget.expired(),
                pages,
                || perform_action(self.element, action) == kAXErrorSuccess,
                || budget.pause(Duration::from_millis(30)),
            )
        }
    }

    fn extent(&self, direction: &str, budget: &Budget) -> Option<f64> {
        if budget.expired() {
            return None;
        }
        unsafe { self.extent(direction) }
    }

    fn bar_units(&self, direction: &str, distance: f64, budget: &Budget) -> Option<f64> {
        let horizontal = matches!(direction, "left" | "right");
        let viewport = WheelArea::extent(self, direction, budget)?;
        let content = unsafe { self.content_length_within(horizontal, budget)? };
        bar_units_for(distance, content, viewport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StepClock(std::cell::Cell<Instant>);
    impl Clock for StepClock {
        fn now(&self) -> Instant {
            self.0.get()
        }
        fn sleep(&self, duration: Duration) {
            self.0.set(self.0.get() + duration);
        }
    }

    #[test]
    fn a_bar_write_is_never_sent_after_the_deadline() {
        let expired = std::cell::Cell::new(false);
        let written = std::cell::Cell::new(false);
        let target = |p: f64, v: f64, c: f64| position_after("down", p, 100.0, c, v);
        // The settable check uses up the budget: no write.
        let got = write_bar(
            &|| expired.get(),
            || Some(0.5),
            || Some(500.0),
            || Some(2500.0),
            target,
            || {
                expired.set(true);
                true
            },
            |_| {
                written.set(true);
                true
            },
        );
        assert_eq!(got, Mutation::NotSent);
        assert!(!written.get());
        // The content read runs out: nothing after it.
        expired.set(false);
        let got = write_bar(
            &|| expired.get(),
            || Some(0.5),
            || {
                expired.set(true);
                Some(500.0)
            },
            || panic!("no content read after the deadline"),
            target,
            || true,
            |_| panic!("no write"),
        );
        assert_eq!(got, Mutation::NotSent);
        // With budget: an accepted write is Sent, a refused one Failed.
        expired.set(false);
        assert_eq!(
            write_bar(
                &|| false,
                || Some(0.5),
                || Some(500.0),
                || Some(2500.0),
                target,
                || true,
                |next| {
                    assert_eq!(next, 0.55);
                    true
                }
            ),
            Mutation::Sent { count: 1 }
        );
        assert_eq!(
            write_bar(
                &|| false,
                || Some(0.5),
                || Some(500.0),
                || Some(2500.0),
                target,
                || true,
                |_| false
            ),
            Mutation::Failed { count: 1 }
        );
    }

    #[test]
    fn page_actions_stop_at_the_deadline_and_at_a_failure() {
        let left = std::cell::Cell::new(2);
        let got = page_ladder(
            || left.get() == 0,
            5,
            || {
                left.set(left.get() - 1);
                true
            },
            || {},
        );
        assert_eq!(got, Mutation::Sent { count: 2 });
        let mut calls = 0;
        let got = page_ladder(
            || false,
            5,
            || {
                calls += 1;
                calls < 2
            },
            || {},
        );
        assert_eq!(got, Mutation::Failed { count: 2 });
        assert_eq!(
            page_ladder(|| true, 5, || panic!("none sent"), || {}),
            Mutation::NotSent
        );
    }

    #[test]
    fn a_blocked_probe_step_stops_the_later_ones() {
        let clock = StepClock(std::cell::Cell::new(Instant::now()));
        let budget = Budget::new(&clock, Duration::from_millis(400));
        let steps: Vec<Box<dyn FnOnce() -> Option<u32>>> = vec![
            Box::new(|| {
                // A hit test on a busy app: its 0.25 s timeout, twice.
                clock.sleep(Duration::from_millis(500));
                None
            }),
            Box::new(|| panic!("no step starts after the probe budget")),
        ];
        assert_eq!(first_found(|| budget.expired(), steps), None);
        let steps: Vec<Box<dyn FnOnce() -> Option<u32>>> = vec![
            Box::new(|| None),
            Box::new(|| Some(7)),
            Box::new(|| panic!("found already")),
        ];
        assert_eq!(first_found(|| false, steps), Some(7));
    }

    #[test]
    fn climbing_stops_between_the_role_and_parent_reads_at_the_deadline() {
        let expired = std::cell::Cell::new(false);
        let released = std::cell::RefCell::new(Vec::new());
        let got = climb(
            1u32,
            40,
            || expired.get(),
            |_| {
                // A busy app's role read uses up the budget.
                expired.set(true);
                Some("AXGroup".to_owned())
            },
            |_| panic!("no parent read after the deadline"),
            |element| released.borrow_mut().push(element),
        );
        assert_eq!(got, None);
        assert_eq!(*released.borrow(), vec![1]);
        let released = std::cell::RefCell::new(Vec::new());
        let roles = ["AXGroup", "AXGroup", "AXScrollArea"];
        let got = climb(
            0usize,
            40,
            || false,
            |element| Some(roles[*element].to_owned()),
            |element| Some(element + 1),
            |element| released.borrow_mut().push(element),
        );
        assert_eq!(got, Some(2));
        assert_eq!(
            *released.borrow(),
            vec![0, 1],
            "every element left behind is released"
        );
    }

    #[test]
    fn progress_is_signed_by_direction() {
        assert!(moved_toward("down", 0.5, 0.6));
        assert!(!moved_toward("down", 0.5, 0.4));
        assert!(moved_toward("up", 0.5, 0.4));
        assert!(!moved_toward("up", 0.5, 0.6));
        assert!(moved_toward("left", 0.5, 0.4));
        assert!(moved_toward("right", 0.5, 0.6));
        assert!(!moved_toward("down", 0.5, 0.5));
        assert_eq!(signed_progress("up", 0.5, 0.7), -0.19999999999999996);
    }

    #[test]
    fn a_budget_clamps_pauses_to_what_is_left() {
        let clock = StepClock(std::cell::Cell::new(Instant::now()));
        let start = clock.now();
        let budget = Budget::new(&clock, Duration::from_millis(100));
        budget.pause(Duration::from_millis(60));
        budget.pause(Duration::from_millis(60));
        assert_eq!(clock.now() - start, Duration::from_millis(100));
        assert!(budget.expired());
        let shorter =
            Budget::new(&clock, Duration::from_millis(500)).sooner(Duration::from_millis(50));
        assert_eq!(shorter.left(), Duration::from_millis(50));
    }

    #[test]
    fn the_content_span_stops_when_the_deadline_passes_mid_walk() {
        let frames = vec![
            Some([0.0, 0.0, 10.0, 100.0]),
            None,
            Some([0.0, 100.0, 10.0, 300.0]),
        ];
        assert_eq!(
            span_of(frames.clone().into_iter(), false, || false),
            Some(400.0)
        );
        assert_eq!(
            span_of(frames.clone().into_iter(), true, || false),
            Some(10.0)
        );
        let mut reads = 0;
        let lazy = frames.into_iter().inspect(|_| reads += 1);
        let mut checks = 0;
        assert_eq!(
            span_of(lazy, false, || {
                checks += 1;
                checks > 2
            }),
            None
        );
        assert_eq!(reads, 2, "no frame is read after the deadline");
    }

    #[test]
    fn points_become_bar_units_over_the_scrollable_length() {
        assert_eq!(bar_units_for(360.0, 2400.0, 600.0), Some(0.2));
        assert_eq!(bar_units_for(360.0, 500.0, 600.0), None);
        assert_eq!(bar_units_for(0.0, 2400.0, 600.0), None);
        assert_eq!(bar_units_for(360.0, f64::NAN, 600.0), None);
    }

    #[test]
    fn each_direction_has_its_page_action() {
        assert_eq!(page_action("down"), Some("AXScrollDownByPage"));
        assert_eq!(page_action("up"), Some("AXScrollUpByPage"));
        assert_eq!(page_action("left"), Some("AXScrollLeftByPage"));
        assert_eq!(page_action("right"), Some("AXScrollRightByPage"));
        assert_eq!(page_action("sideways"), None);
    }

    #[test]
    fn a_scroll_area_row_names_where_its_bars_sit() {
        assert_eq!(position_label(None, None), None);
        assert_eq!(
            position_label(Some(0.0), None).as_deref(),
            Some("scrolled 0% down")
        );
        assert_eq!(
            position_label(Some(0.426), Some(1.2)).as_deref(),
            Some("scrolled 43% down, 100% right")
        );
        assert_eq!(
            position_label(None, Some(0.5)).as_deref(),
            Some("scrolled 50% right")
        );
    }

    #[test]
    fn a_scroll_at_its_end_is_not_a_dropped_one() {
        assert!(at_end("down", 1.0));
        assert!(!at_end("down", 0.4));
        assert!(at_end("up", 0.0));
        assert!(!at_end("up", 0.4));
        assert!(!moved(0.25, 0.25));
        assert!(moved(0.25, 0.5));
    }

    #[test]
    fn a_bar_moves_by_the_distance_over_the_scrollable_length() {
        assert_eq!(position_after("down", 0.0, 360.0, 2400.0, 600.0), Some(0.2));
        assert_eq!(position_after("up", 0.5, 360.0, 2400.0, 600.0), Some(0.3));
        assert_eq!(
            position_after("down", 0.9, 3600.0, 2400.0, 600.0),
            Some(1.0)
        );
        assert_eq!(position_after("down", 0.0, 360.0, 500.0, 600.0), None);
    }

    #[test]
    fn wheel_distance_becomes_whole_pages_within_the_cap() {
        assert_eq!(pages_for(360.0, 560.0, 50), 1);
        assert_eq!(pages_for(1200.0, 560.0, 50), 3);
        assert_eq!(pages_for(30_000.0, 560.0, 50), 50);
        assert_eq!(pages_for(120.0, 0.0, 50), 1);
    }
}
