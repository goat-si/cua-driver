//! Scrolling a native scroll area through its scroll bar, with the bar's
//! value as the read-back.
//!
//! AppKit exposes an `NSScrollView` as an `AXScrollArea` whose
//! `AXVerticalScrollBar` / `AXHorizontalScrollBar` carry a settable
//! `AXValue` from 0 (top or left) to 1 (bottom or right). Writing that value
//! scrolls the view without an event, a fronted window or a moved pointer, and
//! reading it again says whether the content moved. Background wheel events,
//! by contrast, are dropped by several system apps (System Settings among
//! them) and give no read-back at all.

use core_foundation::base::{CFRelease, CFRetain, CFTypeRef};

use super::bindings::{
    copy_children, copy_element_attr, copy_number_attr, copy_string_attr,
    element_at_screen_position, element_screen_rect, is_attribute_settable, perform_action,
    set_number_attr, AXUIElementRef,
};

/// Ancestors searched from the target for its scroll area.
const MAX_ASCENT: usize = 40;
/// Pixels one wheel notch scrolls, matching the wheel path.
pub const LINE_STEP_PX: f64 = 120.0;
/// Share of the viewport one page scrolls, leaving a little overlap.
const PAGE_SHARE: f64 = 0.9;
/// A value change smaller than this is read as no movement.
const MOVED_EPSILON: f64 = 0.0005;

/// What scrolling a scroll area through accessibility did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AxScroll {
    /// The scroll bar's value moved from `before` to `after`.
    Moved { before: f64, after: f64 },
    /// The bar already sits at the end the direction scrolls towards, so
    /// nothing was written.
    AtEdge { value: f64 },
    /// The bar did not move; the caller tries its other paths.
    Unchanged { before: f64 },
    /// No scroll area with a bar on that axis contains the target.
    NoScrollArea,
}

fn vertical(direction: &str) -> bool {
    matches!(direction, "up" | "down")
}

fn towards_end(direction: &str) -> bool {
    matches!(direction, "down" | "right")
}

/// Whether `value` is already at the end `direction` scrolls towards.
pub fn at_edge(value: f64, direction: &str) -> bool {
    if towards_end(direction) {
        value >= 1.0 - MOVED_EPSILON
    } else {
        value <= MOVED_EPSILON
    }
}

/// Whether the bar moved between two reads.
pub fn moved(before: f64, after: f64) -> bool {
    (after - before).abs() > MOVED_EPSILON
}

/// The scroll bar value after scrolling `amount` lines or pages from
/// `before`. `viewport` and `content` are the visible and total extents
/// along the axis, in points; without a usable content extent a line is a
/// tenth of the range and a page half of it.
pub fn target_value(
    before: f64,
    direction: &str,
    by: &str,
    amount: usize,
    viewport: Option<f64>,
    content: Option<f64>,
) -> f64 {
    let amount = amount.max(1) as f64;
    let share = match (viewport, content) {
        (Some(viewport), Some(content)) if content > viewport + 1.0 => {
            let step = if by == "page" {
                viewport * PAGE_SHARE
            } else {
                LINE_STEP_PX
            };
            step * amount / (content - viewport)
        }
        _ => amount * if by == "page" { 0.5 } else { 0.1 },
    };
    let signed = if towards_end(direction) {
        share
    } else {
        -share
    };
    (before + signed).clamp(0.0, 1.0)
}

/// The named AX action that pages a scroll area in `direction`.
pub fn page_action(direction: &str) -> Option<&'static str> {
    Some(match direction {
        "down" => "AXScrollDownByPage",
        "up" => "AXScrollUpByPage",
        "right" => "AXScrollRightByPage",
        "left" => "AXScrollLeftByPage",
        _ => return None,
    })
}

/// The scroll bar attribute for an axis.
pub fn bar_attribute(vertical: bool) -> &'static str {
    if vertical {
        "AXVerticalScrollBar"
    } else {
        "AXHorizontalScrollBar"
    }
}

/// The scroll bar's current value on one axis of a scroll area, if it has a
/// bar there.
///
/// # Safety
///
/// `area` must be a valid, live `AXUIElementRef`.
pub unsafe fn bar_value(area: AXUIElementRef, vertical: bool) -> Option<f64> {
    let bar = copy_element_attr(area, bar_attribute(vertical))?;
    let value = copy_number_attr(bar, "AXValue");
    CFRelease(bar as CFTypeRef);
    value.filter(|value| value.is_finite())
}

/// The row a scroll area shows in a tree: where each of its bars sits, as a
/// percentage. `None` when it has no bar, which means it cannot scroll.
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

/// The nearest scroll area at or above `element` with a bar on the axis,
/// retained. Stops at the window.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef`. The caller releases the
/// returned element.
pub unsafe fn nearest_scroll_area(
    element: AXUIElementRef,
    vertical: bool,
) -> Option<AXUIElementRef> {
    CFRetain(element as CFTypeRef);
    let mut current = element;
    for _ in 0..MAX_ASCENT {
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXScrollArea") if bar_value(current, vertical).is_some() => {
                return Some(current);
            }
            Some("AXWindow") | Some("AXApplication") | None => break,
            _ => {}
        }
        let parent = copy_element_attr(current, "AXParent");
        CFRelease(current as CFTypeRef);
        current = parent?;
    }
    CFRelease(current as CFTypeRef);
    None
}

/// The visible and total extents of a scroll area along an axis: its own
/// frame, and the largest frame among its contents (`AXContents`, or its
/// children apart from the bars).
unsafe fn extents(area: AXUIElementRef, vertical: bool) -> (Option<f64>, Option<f64>) {
    let along = |rect: [f64; 4]| if vertical { rect[3] } else { rect[2] };
    let viewport = element_screen_rect(area).map(along);
    let mut content: Option<f64> = None;
    for child in copy_children(area) {
        let role = copy_string_attr(child, "AXRole");
        if role.as_deref() != Some("AXScrollBar") {
            if let Some(extent) = element_screen_rect(child).map(along) {
                content = Some(content.map_or(extent, |seen: f64| seen.max(extent)));
            }
        }
        CFRelease(child as CFTypeRef);
    }
    (viewport, content)
}

/// Scroll the scroll area that holds `element` (or, without one, the element
/// under screen point `point`) by `amount` lines or pages, writing its bar's
/// value or, when the bar is read-only, paging it with the area's named
/// action. Reads the bar again afterwards.
///
/// # Safety
///
/// Performs AX calls on `pid`'s elements; `element`, when given, must be a
/// live `AXUIElementRef`. Call from a blocking context.
pub unsafe fn scroll(
    pid: i32,
    element: Option<usize>,
    point: (f64, f64),
    direction: &str,
    by: &str,
    amount: usize,
) -> AxScroll {
    let vertical = vertical(direction);
    let start = match element {
        Some(ptr) => {
            CFRetain(ptr as CFTypeRef);
            ptr as AXUIElementRef
        }
        None => match element_at_screen_position(pid, point.0, point.1) {
            Some(hit) => hit,
            None => return AxScroll::NoScrollArea,
        },
    };
    let area = nearest_scroll_area(start, vertical);
    CFRelease(start as CFTypeRef);
    let Some(area) = area else {
        return AxScroll::NoScrollArea;
    };
    let outcome = scroll_area(area, vertical, direction, by, amount);
    CFRelease(area as CFTypeRef);
    outcome
}

unsafe fn scroll_area(
    area: AXUIElementRef,
    vertical: bool,
    direction: &str,
    by: &str,
    amount: usize,
) -> AxScroll {
    let Some(before) = bar_value(area, vertical) else {
        return AxScroll::NoScrollArea;
    };
    if at_edge(before, direction) {
        return AxScroll::AtEdge { value: before };
    }
    let settle = || std::thread::sleep(std::time::Duration::from_millis(80));
    if let Some(bar) = copy_element_attr(area, bar_attribute(vertical)) {
        if is_attribute_settable(bar, "AXValue") {
            let (viewport, content) = extents(area, vertical);
            let target = target_value(before, direction, by, amount, viewport, content);
            set_number_attr(bar, "AXValue", target);
        }
        CFRelease(bar as CFTypeRef);
        settle();
        if let Some(after) = bar_value(area, vertical).filter(|after| moved(before, *after)) {
            return AxScroll::Moved { before, after };
        }
    }
    // A read-only bar: page through the area's own action, only for page
    // scrolls so a line request never jumps a screenful.
    if by == "page" {
        if let Some(action) = page_action(direction) {
            for _ in 0..amount.max(1) {
                if perform_action(area, action) != super::bindings::kAXErrorSuccess {
                    break;
                }
            }
            settle();
            if let Some(after) = bar_value(area, vertical).filter(|after| moved(before, *after)) {
                return AxScroll::Moved { before, after };
            }
        }
    }
    AxScroll::Unchanged { before }
}

/// The bar value of the scroll area at `point` (or holding `element`) on the
/// axis of `direction`, for reading a scroll back after another path.
///
/// # Safety
///
/// As [`scroll`].
pub unsafe fn position(
    pid: i32,
    element: Option<usize>,
    point: (f64, f64),
    direction: &str,
) -> Option<f64> {
    let vertical = vertical(direction);
    let start = match element {
        Some(ptr) => {
            CFRetain(ptr as CFTypeRef);
            ptr as AXUIElementRef
        }
        None => element_at_screen_position(pid, point.0, point.1)?,
    };
    let area = nearest_scroll_area(start, vertical);
    CFRelease(start as CFTypeRef);
    let area = area?;
    let value = bar_value(area, vertical);
    CFRelease(area as CFTypeRef);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_moves_the_bar_by_one_notch_of_the_scrollable_range() {
        // 600 pt visible of 1800 pt: 1200 pt of range, a notch is 120 pt.
        let after = target_value(0.0, "down", "line", 3, Some(600.0), Some(1800.0));
        assert!((after - 0.3).abs() < 1e-9, "{after}");
        let back = target_value(0.3, "up", "line", 1, Some(600.0), Some(1800.0));
        assert!((back - 0.2).abs() < 1e-9, "{back}");
    }

    #[test]
    fn a_page_moves_most_of_a_viewport_and_stops_at_the_ends() {
        let after = target_value(0.0, "down", "page", 1, Some(600.0), Some(1800.0));
        assert!((after - 0.45).abs() < 1e-9, "{after}");
        assert_eq!(
            target_value(0.9, "down", "page", 5, Some(600.0), Some(1800.0)),
            1.0
        );
        assert_eq!(target_value(0.1, "left", "page", 5, None, None), 0.0);
    }

    #[test]
    fn without_a_content_extent_the_step_is_a_share_of_the_range() {
        assert!((target_value(0.5, "right", "line", 2, Some(400.0), None) - 0.7).abs() < 1e-9);
        // Content that fits the viewport gives no usable range either.
        assert!((target_value(0.5, "up", "page", 1, Some(400.0), Some(400.0)) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn a_bar_at_the_end_it_scrolls_towards_is_at_the_edge() {
        assert!(at_edge(1.0, "down"));
        assert!(at_edge(0.0, "up"));
        assert!(at_edge(0.0, "left"));
        assert!(!at_edge(0.0, "down"));
        assert!(!at_edge(0.5, "up"));
        assert!(at_edge(0.9999, "right"));
    }

    #[test]
    fn only_a_real_change_counts_as_movement() {
        assert!(moved(0.0, 1.0));
        assert!(!moved(0.25, 0.2502));
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
    fn every_direction_has_a_page_action() {
        for direction in ["up", "down", "left", "right"] {
            assert!(page_action(direction).is_some());
        }
        assert_eq!(page_action("sideways"), None);
    }
}
