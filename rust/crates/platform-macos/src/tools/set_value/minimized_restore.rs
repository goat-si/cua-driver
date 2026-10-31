//! A minimized window restored for one `set_value` write and minimized
//! again after it. Private to `set_value`: no key-posting tool may restore a
//! window temporarily, since minimizing it again cannot be ordered after a
//! posted key (goat-si/cua-driver#125).

use core_foundation::base::CFRelease;

/// The live reads and writes of a temporary restore (fakes in tests).
pub(super) trait MinimizeOps {
    /// Write AXMinimized; `Err` carries the AX error code.
    fn set_minimized(&self, minimized: bool) -> Result<(), i32>;
    fn read_minimized(&self) -> Option<bool>;
    /// Wait for the app's key hand-backs that are still running; `false`
    /// when one is still running when the wait ends. Minimizing the window
    /// while a hand-back's records are in flight lets AppKit pick the key
    /// window under them (goat-si/cua-driver#125).
    fn await_hand_backs(&self) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestoreState {
    /// Nothing was written.
    Untouched,
    /// AXMinimized=false may have been applied (written, not yet read back).
    MaybeUnminimized,
    Restored,
    /// Cleanup ran.
    Finished,
}

/// What became of the window after the action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RestoreReport {
    /// Nothing was written to the window.
    NotTouched,
    /// AXMinimized reads true again.
    Reminimized,
    /// AXMinimized=true was written but reads back as given.
    ReminimizeNotConfirmed(Option<bool>),
    /// Not minimized again: the app's key hand-back had not finished.
    LeftRestored,
}

impl RestoreReport {
    /// A stable name for structured results.
    pub(super) fn code(self) -> &'static str {
        match self {
            RestoreReport::NotTouched => "not_touched",
            RestoreReport::Reminimized => "reminimized",
            RestoreReport::ReminimizeNotConfirmed(_) => "reminimize_not_confirmed",
            RestoreReport::LeftRestored => "left_restored",
        }
    }

    pub(super) fn describe(self) -> String {
        match self {
            RestoreReport::NotTouched => String::new(),
            RestoreReport::Reminimized => {
                "The window was restored from the Dock for the action and minimized again.".into()
            }
            RestoreReport::ReminimizeNotConfirmed(read) => format!(
                "The window was restored from the Dock for the action; minimizing it again was \
                 requested but AXMinimized reads {read:?}."
            ),
            RestoreReport::LeftRestored => {
                "The window was restored from the Dock for the action and left restored: the \
                 app's key focus was still being handed back, and minimizing the window then \
                 could leave another window key."
                    .into()
            }
        }
    }
}

/// Owns a temporary restore from creation, before the first write: every
/// failure after that write (an AX error after the attribute was applied, a
/// readback timeout, an unreadable state) still re-minimizes or reports.
pub(super) struct MinimizedRestore<O: MinimizeOps> {
    ops: O,
    state: RestoreState,
}

const RESTORE_POLLS: usize = 60;
const RESTORE_POLL: std::time::Duration = std::time::Duration::from_millis(25);
/// The longest a re-minimize waits for the app's key hand-back, as long as
/// a new key scope waits for one.
const HAND_BACK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

impl<O: MinimizeOps> MinimizedRestore<O> {
    pub(super) fn new(ops: O) -> Self {
        Self {
            ops,
            state: RestoreState::Untouched,
        }
    }

    /// Restore the window and prove it: readback false. `Err` when not; no
    /// input may follow then.
    pub(super) fn begin(&mut self, mut pause: impl FnMut()) -> Result<(), String> {
        self.state = RestoreState::MaybeUnminimized;
        if let Err(code) = self.ops.set_minimized(false) {
            return Err(format!(
                "restoring the minimized window returned AX error {code}"
            ));
        }
        for _ in 0..RESTORE_POLLS {
            if self.ops.read_minimized() == Some(false) {
                self.state = RestoreState::Restored;
                break;
            }
            pause();
        }
        if self.state != RestoreState::Restored {
            return Err("the window did not read back as restored".into());
        }
        Ok(())
    }

    pub(super) fn finish(mut self) -> RestoreReport {
        self.cleanup(|| std::thread::sleep(RESTORE_POLL))
    }

    fn cleanup(&mut self, mut pause: impl FnMut()) -> RestoreReport {
        match self.state {
            RestoreState::Untouched | RestoreState::Finished => return RestoreReport::NotTouched,
            RestoreState::MaybeUnminimized | RestoreState::Restored => {}
        }
        self.state = RestoreState::Finished;
        // Every key scope of this action has ended by now (finished, failed
        // on entry or dropped while unwinding); its hand-back may still be
        // running on its own thread.
        if !self.ops.await_hand_backs() {
            return RestoreReport::LeftRestored;
        }
        let _ = self.ops.set_minimized(true);
        let mut read = None;
        for _ in 0..RESTORE_POLLS {
            read = self.ops.read_minimized();
            if read == Some(true) {
                return RestoreReport::Reminimized;
            }
            pause();
        }
        RestoreReport::ReminimizeNotConfirmed(read)
    }
}

impl<O: MinimizeOps> Drop for MinimizedRestore<O> {
    fn drop(&mut self) {
        let report = self.cleanup(|| std::thread::sleep(RESTORE_POLL));
        if !matches!(
            report,
            RestoreReport::NotTouched | RestoreReport::Reminimized
        ) {
            tracing::warn!(
                ?report,
                "a temporarily restored window was not minimized again"
            );
        }
    }
}

/// The real AXMinimized reads and writes for one exact window.
pub(super) struct AxMinimizeOps {
    pid: i32,
    window: crate::ax::snapshot::RetainedElement,
}

impl AxMinimizeOps {
    pub(super) fn of(pid: i32, window_id: u32) -> Option<Self> {
        use crate::ax::bindings::{
            ax_get_window_id, copy_ax_windows_including, AXUIElementCreateApplication,
        };
        unsafe {
            let app = AXUIElementCreateApplication(pid);
            if app.is_null() {
                return None;
            }
            let mut found = None;
            for window in copy_ax_windows_including(app, pid, window_id) {
                if found.is_none() && ax_get_window_id(window) == Some(window_id) {
                    found = Some(crate::ax::snapshot::RetainedElement::retain(
                        window as usize,
                    ));
                }
                CFRelease(window as _);
            }
            CFRelease(app as _);
            found.map(|window| Self { pid, window })
        }
    }
}

impl MinimizeOps for AxMinimizeOps {
    fn set_minimized(&self, minimized: bool) -> Result<(), i32> {
        use core_foundation::{base::TCFType, boolean::CFBoolean, string::CFString};
        let attr = CFString::new("AXMinimized");
        let value = if minimized {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };
        let err = unsafe {
            crate::ax::bindings::AXUIElementSetAttributeValue(
                self.window.as_ptr() as crate::ax::bindings::AXUIElementRef,
                attr.as_concrete_TypeRef(),
                value.as_CFTypeRef(),
            )
        };
        if err == 0 {
            Ok(())
        } else {
            Err(err)
        }
    }

    fn await_hand_backs(&self) -> bool {
        crate::input::skylight::await_key_restores_for_pid(
            Some(self.pid as libc::pid_t),
            HAND_BACK_WAIT,
        )
    }

    fn read_minimized(&self) -> Option<bool> {
        unsafe {
            crate::ax::bindings::copy_bool_attr(
                self.window.as_ptr() as crate::ax::bindings::AXUIElementRef,
                "AXMinimized",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Scripted AX: each read pops the next value (the last one repeats).
    struct Fake {
        writes: RefCell<Vec<bool>>,
        write_error_on: Option<bool>,
        reads: RefCell<Vec<Option<bool>>>,
        /// What `await_hand_backs` answers.
        hand_backs_done: bool,
        /// Calls in order: "wait", "min:<bool>".
        log: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(reads: Vec<Option<bool>>) -> Self {
            Self {
                writes: RefCell::new(Vec::new()),
                write_error_on: None,
                reads: RefCell::new(reads),
                hand_backs_done: true,
                log: RefCell::new(Vec::new()),
            }
        }
    }

    fn pop<T: Copy>(values: &RefCell<Vec<T>>) -> T {
        let mut values = values.borrow_mut();
        if values.len() > 1 {
            values.remove(0)
        } else {
            values[0]
        }
    }

    impl MinimizeOps for &Fake {
        fn set_minimized(&self, minimized: bool) -> Result<(), i32> {
            self.writes.borrow_mut().push(minimized);
            self.log.borrow_mut().push(format!("min:{minimized}"));
            if self.write_error_on == Some(minimized) {
                Err(-25200)
            } else {
                Ok(())
            }
        }
        fn read_minimized(&self) -> Option<bool> {
            pop(&self.reads)
        }
        fn await_hand_backs(&self) -> bool {
            self.log.borrow_mut().push("wait".into());
            self.hand_backs_done
        }
    }

    /// The restore does not read which app is frontmost: a minimized window
    /// of a frontmost app is restored for the action and minimized again
    /// like any other.
    #[test]
    fn a_minimized_window_is_restored_without_reading_the_front_app() {
        let fake = Fake::new(vec![Some(false), Some(true)]);
        let mut restore = MinimizedRestore::new(&fake);
        restore.begin(|| {}).expect("restored");
        assert_eq!(restore.finish(), RestoreReport::Reminimized);
        assert_eq!(*fake.writes.borrow(), vec![false, true]);
    }

    #[test]
    fn a_restore_is_minimized_again_after_the_action() {
        let fake = Fake::new(vec![Some(false), Some(true)]);
        let mut restore = MinimizedRestore::new(&fake);
        restore.begin(|| {}).expect("restored");
        // read: false (begin) then true (finish)
        assert_eq!(restore.finish(), RestoreReport::Reminimized);
        assert_eq!(*fake.writes.borrow(), vec![false, true]);
    }

    #[test]
    fn a_write_error_after_the_attribute_applied_is_still_cleaned_up() {
        let mut fake = Fake::new(vec![Some(false), Some(true)]);
        fake.write_error_on = Some(false);
        let mut restore = MinimizedRestore::new(&fake);
        assert!(restore.begin(|| {}).is_err());
        assert_eq!(restore.finish(), RestoreReport::Reminimized);
        assert_eq!(*fake.writes.borrow(), vec![false, true]);
    }

    #[test]
    fn a_readback_timeout_or_unreadable_state_is_cleaned_up_and_reported() {
        let fake = Fake::new(vec![None]);
        let mut restore = MinimizedRestore::new(&fake);
        assert!(restore.begin(|| {}).is_err(), "never read back as restored");
        assert_eq!(
            restore.finish(),
            RestoreReport::ReminimizeNotConfirmed(None)
        );
        assert_eq!(*fake.writes.borrow(), vec![false, true]);
    }

    #[test]
    fn dropping_a_restore_without_finish_cleans_up() {
        let fake = Fake::new(vec![Some(false), Some(true)]);
        {
            let mut restore = MinimizedRestore::new(&fake);
            restore.begin(|| {}).expect("restored");
        }
        assert_eq!(*fake.writes.borrow(), vec![false, true]);
    }

    /// The window is minimized again only after the app's key hand-back has
    /// finished (#125).
    #[test]
    fn the_window_is_minimized_again_only_after_the_hand_back_finished() {
        let fake = Fake::new(vec![Some(false), Some(true)]);
        let mut restore = MinimizedRestore::new(&fake);
        restore.begin(|| {}).expect("restored");
        assert_eq!(restore.finish(), RestoreReport::Reminimized);
        assert_eq!(*fake.log.borrow(), vec!["min:false", "wait", "min:true"]);
    }

    /// A hand-back still running when the wait ends leaves the window
    /// restored, and says so.
    #[test]
    fn a_hand_back_still_running_leaves_the_window_restored() {
        let mut fake = Fake::new(vec![Some(false)]);
        fake.hand_backs_done = false;
        let mut restore = MinimizedRestore::new(&fake);
        restore.begin(|| {}).expect("restored");
        let report = restore.finish();
        assert_eq!(report, RestoreReport::LeftRestored);
        assert_eq!(report.code(), "left_restored");
        assert_eq!(*fake.writes.borrow(), vec![false]);
    }

    /// Unwinding drops the action's key scope before the restore guard
    /// (reverse declaration order), so the guard's wait sees the hand-back
    /// the scope started, and a running one keeps the window restored.
    #[test]
    fn a_restore_dropped_while_unwinding_waits_for_the_hand_back_too() {
        let mut fake = Fake::new(vec![Some(false)]);
        fake.hand_backs_done = false;
        let order = RefCell::new(Vec::new());
        struct Scope<'a>(&'a RefCell<Vec<&'static str>>);
        impl Drop for Scope<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().push("scope");
            }
        }
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut restore = MinimizedRestore::new(&fake);
            restore.begin(|| {}).expect("restored");
            let _scope = Scope(&order);
            order.borrow_mut().push("write");
            panic!("the write failed mid-way");
        }));
        assert!(unwound.is_err());
        assert_eq!(*order.borrow(), vec!["write", "scope"]);
        assert_eq!(*fake.log.borrow(), vec!["min:false", "wait"]);
        assert_eq!(*fake.writes.borrow(), vec![false]);
    }
}
