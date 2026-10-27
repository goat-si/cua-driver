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
    AXUIElementCreateApplication, AXUIElementRef,
};

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

/// Whether a scroll bar went from `before` to `after`.
pub fn moved(before: f64, after: f64) -> bool {
    (after - before).abs() > EPSILON
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
        if let Some(element) = element {
            CFRetain(element as CFTypeRef);
            if let Some(area) = ascend(element) {
                return Some(area);
            }
        }
        if let Some(hit) = element_at_screen_position(pid, point.0, point.1) {
            if let Some(area) = ascend(hit) {
                return Some(area);
            }
        }
        by_frame(pid, point, window_id)
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

    /// Reveals the content about `distance` points past the edge in
    /// `direction` with AXScrollToVisible, clipped to `window`. True when the
    /// reveal moved something.
    ///
    /// # Safety
    ///
    /// Call from a blocking context.
    pub unsafe fn reveal(&self, window: Option<[f64; 4]>, direction: &str, distance: f64) -> bool {
        super::reveal_scroll::reveal_in(self.element, "AXScrollArea", window, direction, distance)
            .is_some()
    }

    /// Moves the scroll bar about `distance` points in `direction` by setting
    /// its value, sized by the area's exposed contents. False when the bar is
    /// not settable or the content length cannot be read.
    ///
    /// # Safety
    ///
    /// Call from a blocking context.
    pub unsafe fn set_position(&self, direction: &str, distance: f64) -> bool {
        let horizontal = matches!(direction, "left" | "right");
        let (Some(position), Some(viewport), Some(content)) = (
            self.position(direction),
            self.extent(direction),
            content_length(self.element, horizontal),
        ) else {
            return false;
        };
        let Some(next) = position_after(direction, position, distance, content, viewport) else {
            return false;
        };
        let Some(bar) = copy_element_attr(self.element, bar_attribute(direction)) else {
            return false;
        };
        let set = is_attribute_settable(bar, "AXValue")
            && set_number_attr(bar, "AXValue", next) == kAXErrorSuccess;
        CFRelease(bar as CFTypeRef);
        set
    }

    /// Performs `direction`'s page action `pages` times. False when the
    /// area offers no such action or the first one fails.
    ///
    /// # Safety
    ///
    /// Call from a blocking context.
    pub unsafe fn page(&self, direction: &str, pages: usize) -> bool {
        let Some(action) = page_action(direction) else {
            return false;
        };
        if !copy_action_names(self.element)
            .iter()
            .any(|name| name == action)
        {
            return false;
        }
        let mut performed = false;
        for _ in 0..pages.max(1) {
            if perform_action(self.element, action) != kAXErrorSuccess {
                break;
            }
            performed = true;
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        performed
    }
}

/// The span of the area's direct children other than its scroll bars, along
/// one axis: the content's length, scrolled or not.
unsafe fn content_length(area: AXUIElementRef, horizontal: bool) -> Option<f64> {
    let mut span: Option<(f64, f64)> = None;
    for child in copy_children(area) {
        if copy_string_attr(child, "AXRole").as_deref() != Some("AXScrollBar") {
            if let Some([x, y, width, height]) = element_screen_rect(child) {
                let (start, length) = if horizontal { (x, width) } else { (y, height) };
                span = Some(match span {
                    Some((low, high)) => (low.min(start), high.max(start + length)),
                    None => (start, start + length),
                });
            }
        }
        CFRelease(child as CFTypeRef);
    }
    span.map(|(low, high)| high - low)
}

/// The nearest AXScrollArea at or above `element`, consuming its retain.
unsafe fn ascend(element: AXUIElementRef) -> Option<ScrollArea> {
    let mut current = element;
    for _ in 0..MAX_ASCENT {
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXScrollArea") => return Some(ScrollArea { element: current }),
            Some("AXWindow") | Some("AXApplication") | None => break,
            Some(_) => {}
        }
        let parent = copy_element_attr(current, "AXParent");
        CFRelease(current as CFTypeRef);
        current = parent?;
    }
    CFRelease(current as CFTypeRef);
    None
}

/// The smallest AXScrollArea in `pid`'s window whose frame holds `point`.
unsafe fn by_frame(pid: i32, point: (f64, f64), window_id: Option<u32>) -> Option<ScrollArea> {
    let application = AXUIElementCreateApplication(pid);
    if application.is_null() {
        return None;
    }
    let windows = copy_ax_windows(application);
    CFRelease(application as CFTypeRef);
    let mut best: Option<(AXUIElementRef, f64)> = None;
    let mut visited = 0;
    for window in windows {
        if window_id.is_none() || ax_get_window_id(window) == window_id {
            smallest_holding(window, point, 0, &mut visited, &mut best);
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
) {
    if depth > MAX_DEPTH || *visited >= MAX_NODES {
        return;
    }
    *visited += 1;
    if copy_string_attr(element, "AXRole").as_deref() == Some("AXScrollArea") {
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
    for child in copy_children(element) {
        smallest_holding(child, point, depth + 1, visited, best);
        CFRelease(child as CFTypeRef);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_direction_has_its_page_action() {
        assert_eq!(page_action("down"), Some("AXScrollDownByPage"));
        assert_eq!(page_action("up"), Some("AXScrollUpByPage"));
        assert_eq!(page_action("left"), Some("AXScrollLeftByPage"));
        assert_eq!(page_action("right"), Some("AXScrollRightByPage"));
        assert_eq!(page_action("sideways"), None);
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
