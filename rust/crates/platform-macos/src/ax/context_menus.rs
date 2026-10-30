//! Open context menus that belong to a walked window.
//!
//! AppKit attaches a view's context menu to the view (Finder's list view
//! holds a row's menu), but the view does not list it in `AXChildren`, so a
//! window walk never reaches it. Each of the app's on-screen menu windows
//! (WindowServer layer 101) is tied to an `AXMenu` here, and the menu is
//! published only when that tie and its ancestry to the walked window are
//! both proven.

use super::bindings::*;
use core_foundation::base::{CFRelease, CFRetain, CFTypeRef};
use std::time::{Duration, Instant};

/// kCGPopUpMenuWindowLevel.
const MENU_WINDOW_LAYER: i32 = 101;
/// AX parents walked from a hit to its menu, and from the menu to its window.
const MAX_MENU_ANCESTRY: usize = 16;
/// How far, in points, an `AXMenu`'s frame may sit from its menu window's.
const FRAME_TOLERANCE: f64 = 4.0;
/// The most time menu discovery spends after a window walk, and the most
/// menu windows it looks at (an open submenu chain is a few).
const MAX_DISCOVERY: Duration = Duration::from_millis(250);
pub(crate) const MAX_MENU_WINDOWS: usize = 4;

/// The deadline for menu discovery after a walk, or `None` to skip it: the
/// walk was cut short, or its time budget has (almost) run out.
pub(crate) fn discovery_deadline(
    now: Instant,
    walk_truncated: bool,
    remaining: Option<Duration>,
) -> Option<Instant> {
    if walk_truncated {
        return None;
    }
    let window = remaining.map_or(MAX_DISCOVERY, |left| left.min(MAX_DISCOVERY));
    (window >= Duration::from_millis(10)).then(|| now + window)
}

/// The frames of `pid`'s on-screen menu windows, as `[x, y, w, h]`.
pub(crate) fn open_menu_window_frames(pid: i32) -> Vec<[f64; 4]> {
    crate::windows::pid_on_screen_windows_any_layer(pid)
        .into_iter()
        .filter(|window| window.layer == MENU_WINDOW_LAYER)
        .map(|window| {
            [
                window.bounds.x,
                window.bounds.y,
                window.bounds.width,
                window.bounds.height,
            ]
        })
        .collect()
}

/// An `AXMenu` frame and a menu window frame describe the same surface.
pub(crate) fn frames_match(menu: [f64; 4], window: [f64; 4]) -> bool {
    menu.iter()
        .zip(window.iter())
        .all(|(a, b)| a.is_finite() && b.is_finite() && (a - b).abs() <= FRAME_TOLERANCE)
}

/// Whether a menu found for one menu window is published under the walked
/// window: its frame matches that menu window, its AX ancestry ends at the
/// walked window, and no `AXMenu` with the same frame was already walked.
pub(crate) fn publish_menu(
    menu_frame: Option<[f64; 4]>,
    window_frame: [f64; 4],
    owner_window: Option<u32>,
    walked_window: u32,
    walked_menu_frames: &[[f64; 4]],
) -> bool {
    let Some(menu_frame) = menu_frame else {
        return false;
    };
    frames_match(menu_frame, window_frame)
        && owner_window == Some(walked_window)
        && !walked_menu_frames
            .iter()
            .any(|walked| frames_match(*walked, menu_frame))
}

/// The `AXMenu` shown in the menu window with `frame`, retained, with its
/// own frame and the window id its AX ancestry ends at. Every read checks
/// `deadline` first; a read that would start after it gives up.
///
/// # Safety
///
/// Calls into Accessibility for `pid`. The caller releases the element.
pub(crate) unsafe fn menu_in_window(
    pid: i32,
    frame: [f64; 4],
    deadline: Instant,
) -> Option<(AXUIElementRef, Option<[f64; 4]>, Option<u32>)> {
    let left = deadline.checked_duration_since(Instant::now())?;
    let [x, y, w, h] = frame;
    let hit = element_at_screen_position_bounded(
        pid,
        x + w / 2.0,
        y + h / 2.0,
        left.as_secs_f32().min(0.25),
    )?;
    let menu = ascend_to_role(hit, "AXMenu", deadline)?;
    let menu_frame = bounded_pair(
        || bound(menu, deadline),
        || copy_position_attr(menu),
        || copy_size_attr(menu),
    )
    .and_then(|(position, size)| rect_from_parts(Some(position), Some(size)));
    let owner = owning_window_by_parents(menu, deadline);
    Some((menu, menu_frame, owner))
}

/// Two reads, each started only when `prepare` (which re-arms the remaining
/// timeout) says time is left: a slow first read cannot let the second run
/// on the original timeout.
fn bounded_pair<A, B>(
    mut prepare: impl FnMut() -> bool,
    first: impl FnOnce() -> Option<A>,
    second: impl FnOnce() -> Option<B>,
) -> Option<(A, B)> {
    if !prepare() {
        return None;
    }
    let a = first()?;
    if !prepare() {
        return None;
    }
    Some((a, second()?))
}

/// Cap the next AX read on `element` at the time left before `deadline`
/// (its messaging timeout), or `false` when none is left. A check alone
/// cannot bound a read that then blocks on an unresponsive app.
unsafe fn bound(element: AXUIElementRef, deadline: Instant) -> bool {
    let Some(left) = deadline.checked_duration_since(Instant::now()) else {
        return false;
    };
    if left < Duration::from_millis(1) {
        return false;
    }
    AXUIElementSetMessagingTimeout(element, left.as_secs_f32().min(0.25));
    true
}

/// `start` or its nearest AX ancestor with `role`, retained; `start` is
/// consumed (released when it is not the result).
unsafe fn ascend_to_role(
    start: AXUIElementRef,
    role: &str,
    deadline: Instant,
) -> Option<AXUIElementRef> {
    let mut current = start;
    for _ in 0..MAX_MENU_ANCESTRY {
        if !bound(current, deadline) {
            break;
        }
        if copy_string_attr(current, "AXRole").as_deref() == Some(role) {
            return Some(current);
        }
        if !bound(current, deadline) {
            break;
        }
        let parent = copy_element_attr(current, "AXParent");
        CFRelease(current as CFTypeRef);
        current = parent?;
    }
    CFRelease(current as CFTypeRef);
    None
}

/// The CGWindowID of the first `AXWindow` or `AXSheet` reached through
/// `AXParent` from `element` (the `AXWindow` attribute is not trusted for
/// a menu).
unsafe fn owning_window_by_parents(element: AXUIElementRef, deadline: Instant) -> Option<u32> {
    if !bound(element, deadline) {
        return None;
    }
    let mut current = copy_element_attr(element, "AXParent")?;
    for _ in 0..MAX_MENU_ANCESTRY {
        if !bound(current, deadline) {
            break;
        }
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXWindow") | Some("AXSheet") => {
                if !bound(current, deadline) {
                    CFRelease(current as CFTypeRef);
                    return None;
                }
                let id = ax_get_window_id(current);
                CFRelease(current as CFTypeRef);
                return id;
            }
            Some("AXApplication") | None => break,
            _ => {}
        }
        if !bound(current, deadline) {
            break;
        }
        let parent = copy_element_attr(current, "AXParent");
        CFRelease(current as CFTypeRef);
        current = parent?;
    }
    CFRelease(current as CFTypeRef);
    None
}

/// Whether `element` sits in an `AXMenu` whose `AXParent` chain reaches
/// the window `window_id` before any other window or sheet: a menu that
/// hangs off one of that window's views, such as a context menu or a pop-up
/// button's menu. It does not prove the menu is open (pressing an item of a
/// closed menu fails like any dead element). The menu bar's menus end at the
/// application and another window's menus at that window, so neither passes.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef`.
pub(crate) unsafe fn element_in_context_menu_of(element: AXUIElementRef, window_id: u32) -> bool {
    let deadline = Instant::now() + Duration::from_millis(250);
    CFRetain(element as CFTypeRef);
    let Some(menu) = ascend_to_role(element, "AXMenu", deadline) else {
        return false;
    };
    let owner = owning_window_by_parents(menu, deadline);
    CFRelease(menu as CFTypeRef);
    owner == Some(window_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: [f64; 4] = [2002.0, 198.0, 391.0, 552.0];

    #[test]
    fn a_menu_is_published_only_with_a_matching_frame_and_window_ancestry() {
        assert!(publish_menu(
            Some([2003.0, 199.0, 390.0, 551.0]),
            WINDOW,
            Some(7),
            7,
            &[]
        ));
        // Another window's menu, or ancestry that could not be read.
        assert!(!publish_menu(Some(WINDOW), WINDOW, Some(8), 7, &[]));
        assert!(!publish_menu(Some(WINDOW), WINDOW, None, 7, &[]));
        // The hit landed on a different menu (a submenu, an occluding one).
        assert!(!publish_menu(
            Some([2002.0, 198.0, 391.0, 300.0]),
            WINDOW,
            Some(7),
            7,
            &[]
        ));
        assert!(!publish_menu(None, WINDOW, Some(7), 7, &[]));
        // Already in the walked tree.
        assert!(!publish_menu(Some(WINDOW), WINDOW, Some(7), 7, &[WINDOW]));
    }

    #[test]
    fn frames_must_be_finite_and_close() {
        assert!(frames_match(WINDOW, [2006.0, 194.0, 391.0, 552.0]));
        assert!(!frames_match(WINDOW, [2006.5, 198.0, 391.0, 552.0]));
        assert!(!frames_match([f64::NAN, 198.0, 391.0, 552.0], WINDOW));
    }

    #[test]
    fn a_slow_first_frame_read_stops_the_second() {
        use std::cell::Cell;
        let start = Instant::now();
        let deadline = start + Duration::from_millis(20);
        let now = Cell::new(start);
        let second_ran = Cell::new(false);
        let frame = bounded_pair(
            || now.get() < deadline,
            || {
                // The position read takes past the deadline.
                now.set(start + Duration::from_millis(25));
                Some((1.0, 2.0))
            },
            || {
                second_ran.set(true);
                Some((3.0, 4.0))
            },
        );
        assert_eq!(frame, None);
        assert!(!second_ran.get());
        // In time: both reads.
        assert_eq!(bounded_pair(|| true, || Some(1), || Some(2)), Some((1, 2)));
    }

    #[test]
    fn discovery_stays_inside_the_walk_budget() {
        let now = Instant::now();
        // A walk that was cut short or has no time left skips discovery.
        assert_eq!(
            discovery_deadline(now, true, Some(Duration::from_secs(1))),
            None
        );
        assert_eq!(discovery_deadline(now, false, Some(Duration::ZERO)), None);
        assert_eq!(
            discovery_deadline(now, false, Some(Duration::from_millis(9))),
            None
        );
        // Otherwise it gets what is left, capped.
        assert_eq!(
            discovery_deadline(now, false, Some(Duration::from_millis(40))),
            Some(now + Duration::from_millis(40))
        );
        assert_eq!(
            discovery_deadline(now, false, Some(Duration::from_secs(5))),
            Some(now + MAX_DISCOVERY)
        );
        assert_eq!(
            discovery_deadline(now, false, None),
            Some(now + MAX_DISCOVERY)
        );
    }
}
