//! Proof that a sheet a key or a button press addressed has closed.
//!
//! A sheet (Finder's Go to Folder, a save sheet) is its own WindowServer
//! window. It counts as dismissed by the action when it was listed right
//! before the action, and a complete snapshot of the app's window ids taken
//! within the observation window still lists the window it was attached to
//! (the app and that window are alive) but no longer lists the sheet, and no
//! person can have closed it: either no physical keyboard or mouse input
//! arrived in between, or another app stayed frontmost the whole time (keys
//! go to the frontmost app, and a click into the sheet activates its app).
//! "The whole time" is another app frontmost at both ends with no app
//! activation observed in between (see [`OtherAppFront`]). The app closing
//! the sheet on its own in that window cannot be told apart, nor can an
//! activation whose notification had not arrived by the final read; every
//! other case stays unverifiable.

use std::time::{Duration, Instant};

use crate::ax::bindings::{copy_element_attr, copy_string_attr, AXUIElementRef};
use crate::ax::scroll_area::{Clock, SystemClock};
use crate::windows::WindowIdSnapshot;
use core_foundation::base::{CFRelease, CFTypeRef};

/// How long after delivery a sheet's window is watched for. A watch returns
/// at the first snapshot that proves the window gone. Finder's Go to Folder
/// sheet left the window list about 330 ms after Escape was posted (its
/// close animation), so the bound sits well past that.
pub(crate) const SHEET_CLOSE_OBSERVATION: Duration = Duration::from_millis(600);
const SHEET_CLOSE_POLL: Duration = Duration::from_millis(15);
/// AX parents walked from a pressed element to its sheet.
const MAX_SHEET_ANCESTRY: usize = 24;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> i32;
}

/// What one snapshot says about the sheet's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SheetPresence {
    Present,
    /// A complete snapshot lists the parent window but not the sheet.
    Gone,
    /// Incomplete, or the parent window is gone too (the app quit or closed
    /// the whole window): nothing ties the disappearance to the action.
    Unknown,
}

pub(crate) fn presence_in(
    snapshot: &WindowIdSnapshot,
    sheet_window_id: u32,
    parent_window_id: u32,
) -> SheetPresence {
    let listed = |id: u32| snapshot.window_ids.binary_search(&id).is_ok();
    if !snapshot.complete {
        SheetPresence::Unknown
    } else if listed(sheet_window_id) {
        SheetPresence::Present
    } else if listed(parent_window_id) {
        SheetPresence::Gone
    } else {
        SheetPresence::Unknown
    }
}

/// Poll `read` from `delivered` until it proves the sheet gone, for at
/// most `window`. A read that returns after the deadline does not count.
fn closed_within<C: Clock>(
    clock: &C,
    delivered: Instant,
    window: Duration,
    mut read: impl FnMut() -> SheetPresence,
) -> bool {
    let deadline = delivered + window;
    loop {
        let seen = read();
        if clock.now() > deadline {
            return false;
        }
        if seen == SheetPresence::Gone {
            return true;
        }
        let left = deadline.saturating_duration_since(clock.now());
        if left.is_zero() {
            return false;
        }
        clock.sleep(left.min(SHEET_CLOSE_POLL));
    }
}

/// No physical input from the baseline on (see click's evidence rule): an
/// unknown read at either end never confirms.
fn input_quiet(idle_before: Option<u64>, idle_after: Option<u64>, elapsed_ms: u64) -> bool {
    idle_before.is_some() && idle_after.is_some_and(|after| after >= elapsed_ms)
}

fn other_app_front(pid: i32) -> bool {
    crate::apps::frontmost_pid().is_some_and(|front| front != pid)
}

/// Whether another app than `pid` stays frontmost over an action: checked
/// at the start and the end, with the process's app-activation counter
/// unchanged between, so the target app cannot have come to the front and
/// gone back in the meantime.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OtherAppFront {
    pid: i32,
    front_before: bool,
    activations_before: u64,
}

impl OtherAppFront {
    pub(crate) fn capture(pid: i32) -> Self {
        // The counter is read first: an activation after it shows up as a
        // changed count even if the front read below already saw it.
        let activations_before = crate::focus_steal::activation_count();
        Self {
            pid,
            front_before: other_app_front(pid),
            activations_before,
        }
    }

    /// Still true now, and no activation happened since [`capture`](Self::capture).
    pub(crate) fn held_throughout(&self) -> bool {
        let front_after = other_app_front(self.pid);
        held_throughout(
            self.front_before,
            front_after,
            self.activations_before,
            crate::focus_steal::activation_count(),
        )
    }
}

fn held_throughout(
    front_before: bool,
    front_after: bool,
    activations_before: u64,
    activations_after: u64,
) -> bool {
    front_before && front_after && activations_before == activations_after
}

/// Whether a person could not have caused the observed change: no physical
/// input over the caller's window, or another app frontmost throughout.
pub(crate) fn no_person_could_act(input_quiet: bool, front: &OtherAppFront) -> bool {
    input_quiet || front.held_throughout()
}

/// A sheet window seen on screen right before an action, to be proven
/// closed after it.
#[derive(Debug)]
pub(crate) struct SheetWatch {
    pid: i32,
    sheet_window_id: u32,
    parent_window_id: u32,
    idle_before: Option<u64>,
    input_from: Instant,
    front: OtherAppFront,
}

impl SheetWatch {
    /// `window_id` is a sheet attached to one of `pid`'s windows and is
    /// listed right now.
    pub(crate) fn for_window(pid: i32, window_id: u32) -> Option<Self> {
        let parent = crate::ax::exact_target::sheet_parent_of(pid, window_id)?;
        Self::baseline(pid, window_id, parent)
    }

    /// The sheet that `element` sits in, when it sits in one that is listed
    /// right now.
    ///
    /// # Safety
    ///
    /// `element` must be a valid, live `AXUIElementRef`.
    pub(crate) unsafe fn for_element(element: AXUIElementRef) -> Option<Self> {
        let mut pid = 0;
        if AXUIElementGetPid(element, &mut pid) != 0 || pid <= 0 {
            return None;
        }
        let sheet = enclosing_sheet_window_id(element)?;
        let parent = crate::ax::exact_target::sheet_parent_of(pid, sheet)?;
        Self::baseline(pid, sheet, parent)
    }

    fn baseline(pid: i32, sheet_window_id: u32, parent_window_id: u32) -> Option<Self> {
        let idle_before = super::physical_input_idle_ms();
        let input_from = Instant::now();
        let front = OtherAppFront::capture(pid);
        let snapshot = crate::windows::pid_window_id_snapshot(pid);
        (presence_in(&snapshot, sheet_window_id, parent_window_id) == SheetPresence::Present)
            .then_some(Self {
                pid,
                sheet_window_id,
                parent_window_id,
                idle_before,
                input_from,
                front,
            })
    }

    pub(crate) fn sheet_window_id(&self) -> u32 {
        self.sheet_window_id
    }

    /// The front-app watch taken at the baseline, for a caller that judges
    /// physical input over its own window ([`no_person_could_act`]).
    pub(crate) fn front(&self) -> OtherAppFront {
        self.front
    }

    /// One read of the sheet's presence now.
    pub(crate) fn presence_now(&self) -> SheetPresence {
        presence_in(
            &crate::windows::pid_window_id_snapshot(self.pid),
            self.sheet_window_id,
            self.parent_window_id,
        )
    }

    /// Whether the sheet's window was proven gone within
    /// [`SHEET_CLOSE_OBSERVATION`] of `delivered`, with no physical input
    /// since the baseline.
    pub(crate) fn closed_after(&self, delivered: Instant) -> bool {
        let closed = closed_within(&SystemClock, delivered, SHEET_CLOSE_OBSERVATION, || {
            self.presence_now()
        });
        let idle_after = super::physical_input_idle_ms();
        let since_baseline_ms =
            u64::try_from(self.input_from.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::debug!(
            closed,
            sheet = self.sheet_window_id,
            idle_before = ?self.idle_before,
            ?idle_after,
            since_baseline_ms,
            "sheet close observation"
        );
        closed
            && no_person_could_act(
                input_quiet(self.idle_before, idle_after, since_baseline_ms),
                &self.front,
            )
    }
}

/// The CGWindowID of the `AXSheet` at or above `element`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef`.
unsafe fn enclosing_sheet_window_id(element: AXUIElementRef) -> Option<u32> {
    let mut current = element;
    let mut owned = false;
    let mut found = None;
    for _ in 0..MAX_SHEET_ANCESTRY {
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXSheet") => {
                found = crate::ax::bindings::ax_get_window_id(current);
                break;
            }
            Some("AXWindow") | Some("AXApplication") | None => break,
            _ => {}
        }
        let parent = copy_element_attr(current, "AXParent");
        if owned {
            CFRelease(current as CFTypeRef);
        }
        match parent {
            Some(parent) => {
                current = parent;
                owned = true;
            }
            None => {
                owned = false;
                break;
            }
        }
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct FakeClock {
        now: Cell<Instant>,
        sleeps: RefCell<Vec<Duration>>,
    }

    impl FakeClock {
        fn new(start: Instant) -> Self {
            Self {
                now: Cell::new(start),
                sleeps: RefCell::new(Vec::new()),
            }
        }
        fn advance(&self, by: Duration) {
            self.now.set(self.now.get() + by);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.now.get()
        }
        fn sleep(&self, duration: Duration) {
            self.sleeps.borrow_mut().push(duration);
            self.advance(duration);
        }
    }

    fn snapshot(complete: bool, ids: &[u32]) -> WindowIdSnapshot {
        WindowIdSnapshot {
            complete,
            window_ids: ids.to_vec(),
        }
    }

    #[test]
    fn only_a_complete_snapshot_that_keeps_the_parent_proves_the_sheet_gone() {
        assert_eq!(
            presence_in(&snapshot(true, &[3, 9]), 9, 3),
            SheetPresence::Present
        );
        assert_eq!(
            presence_in(&snapshot(true, &[3]), 9, 3),
            SheetPresence::Gone
        );
        assert_eq!(
            presence_in(&snapshot(false, &[]), 9, 3),
            SheetPresence::Unknown
        );
        // The app quit (no windows) or closed the whole parent window.
        assert_eq!(
            presence_in(&snapshot(true, &[]), 9, 3),
            SheetPresence::Unknown
        );
        assert_eq!(
            presence_in(&snapshot(true, &[4]), 9, 3),
            SheetPresence::Unknown
        );
    }

    #[test]
    fn a_sheet_gone_on_a_later_read_is_closed_without_waiting_out_the_window() {
        let start = Instant::now();
        let clock = FakeClock::new(start);
        let mut reads = vec![
            SheetPresence::Present,
            SheetPresence::Present,
            SheetPresence::Gone,
        ]
        .into_iter();
        assert!(closed_within(
            &clock,
            start,
            SHEET_CLOSE_OBSERVATION,
            || reads.next().unwrap()
        ));
        assert_eq!(*clock.sleeps.borrow(), [SHEET_CLOSE_POLL, SHEET_CLOSE_POLL]);
    }

    #[test]
    fn unknown_reads_never_prove_a_close() {
        let start = Instant::now();
        let clock = FakeClock::new(start);
        assert!(!closed_within(
            &clock,
            start,
            SHEET_CLOSE_OBSERVATION,
            || SheetPresence::Unknown
        ));
        assert_eq!(clock.now(), start + SHEET_CLOSE_OBSERVATION);
    }

    #[test]
    fn a_close_read_after_the_deadline_does_not_count() {
        let start = Instant::now();
        let clock = FakeClock::new(start);
        // The one read takes past the deadline and then sees the sheet gone.
        let read = || {
            clock.advance(SHEET_CLOSE_OBSERVATION + Duration::from_millis(1));
            SheetPresence::Gone
        };
        assert!(!closed_within(&clock, start, SHEET_CLOSE_OBSERVATION, read));
    }

    #[test]
    fn physical_input_during_the_watch_disqualifies_the_close() {
        assert!(input_quiet(Some(5_000), Some(400), 300));
        assert!(!input_quiet(Some(5_000), Some(100), 300));
        assert!(!input_quiet(None, Some(5_000), 300));
        assert!(!input_quiet(Some(5_000), None, 300));
    }

    #[test]
    fn another_app_must_stay_in_front_with_no_activation_between() {
        // A person working in another app the whole time.
        assert!(held_throughout(true, true, 7, 7));
        // The target app was or became frontmost at either end.
        assert!(!held_throughout(false, true, 7, 7));
        assert!(!held_throughout(true, false, 7, 7));
        // Another app at both ends, but an activation in between: the
        // target may have come to the front, taken input and gone back.
        assert!(!held_throughout(true, true, 7, 8));
    }
}
