//! Interruptible physical key delivery and balanced transition cleanup.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use core_graphics::event::{CGEventFlags, CGEventTapLocation};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

use super::keyboard;

#[derive(Default)]
pub(crate) struct HoldControl {
    cancelled: Mutex<bool>,
    wake: Condvar,
}

impl HoldControl {
    pub(crate) fn cancel(&self) {
        *self.cancelled.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        *self.cancelled.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn wait(&self, duration: Duration) -> bool {
        let state = self.cancelled.lock().unwrap_or_else(|e| e.into_inner());
        let (state, _) = self
            .wake
            .wait_timeout_while(state, duration, |cancelled| !*cancelled)
            .unwrap_or_else(|e| e.into_inner());
        *state
    }
}

pub(crate) struct CancelOnDrop(pub(crate) Arc<HoldControl>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Clone, Copy)]
pub(crate) enum KeyRoute {
    Pid(i32),
    PidNoAuth(i32),
    Global,
    BareGlobal,
}

pub(crate) fn deliver(
    route: KeyRoute,
    key: &str,
    modifiers: &[&str],
    duration: Option<Duration>,
    control: &HoldControl,
) -> anyhow::Result<()> {
    let source = if matches!(route, KeyRoute::Global) {
        Some(
            CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?,
        )
    } else {
        None
    };
    deliver_with(
        key,
        modifiers,
        duration.unwrap_or_else(keyboard::key_gap),
        |code, down, flags| match route {
            KeyRoute::Pid(pid) => keyboard::post_key(pid, code, down, flags),
            KeyRoute::PidNoAuth(pid) => keyboard::post_key_no_auth(pid, code, down, flags),
            KeyRoute::Global => keyboard::post_global_key(
                source.as_ref().expect("global source"),
                code,
                down,
                flags,
                CGEventTapLocation::HID,
            ),
            KeyRoute::BareGlobal => {
                let event = keyboard::create_bare_keyboard_event(code, down)?;
                event.set_flags(flags);
                event.post(CGEventTapLocation::HID);
                Ok(())
            }
        },
        || control.is_cancelled(),
        |hold| control.wait(hold),
    )
}

pub(crate) fn deliver_with(
    key: &str,
    modifiers: &[&str],
    duration: Duration,
    mut post: impl FnMut(u16, bool, CGEventFlags) -> anyhow::Result<()>,
    mut cancelled: impl FnMut() -> bool,
    mut wait: impl FnMut(Duration) -> bool,
) -> anyhow::Result<()> {
    let plus = key == "+" || key.eq_ignore_ascii_case("plus");
    let base = keyboard::key_name_to_code(if plus { "=" } else { key })?;
    let mut keys = Vec::new();
    for modifier in modifiers.iter().copied().chain(plus.then_some("shift")) {
        if let Some((code, flag)) = keyboard::modifier_key_code_and_flag(modifier) {
            if !keys.iter().any(|(old, _)| *old == code) {
                keys.push((code, flag));
            }
        }
    }
    let mut pressed = Vec::new();
    let mut flags = CGEventFlags::CGEventFlagNull;
    let mut base_attempted = false;
    let mut result: anyhow::Result<()> = Ok(());
    for (code, flag) in keys {
        if cancelled() {
            break;
        }
        flags |= flag;
        pressed.push((code, flag));
        if let Err(error) = post(code, true, flags) {
            result = Err(error);
            break;
        }
    }
    if result.is_ok() && !cancelled() {
        base_attempted = true;
        if let Err(error) = post(base, true, flags) {
            result = Err(error);
        } else if wait(duration) {
            result = Err(anyhow::anyhow!("key delivery cancelled"));
        }
    }
    if base_attempted {
        if let Err(error) = post(base, false, flags) {
            let _ = post(base, false, flags);
            if result.is_ok() {
                result = Err(error);
            }
        }
    }
    for (code, flag) in pressed.into_iter().rev() {
        flags.remove(flag);
        if let Err(error) = post(code, false, flags) {
            let _ = post(code, false, flags);
            if result.is_ok() {
                result = Err(error);
            }
        }
    }
    if result.is_ok() && cancelled() {
        return Err(anyhow::anyhow!("key delivery cancelled"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_hold_is_passed_unclamped_to_the_waiter() {
        for ms in [0, 30_000] {
            let waited = std::cell::Cell::new(None);
            deliver_with(
                "a",
                &[],
                Duration::from_millis(ms),
                |_, _, _| Ok(()),
                || false,
                |duration| {
                    waited.set(Some(duration));
                    false
                },
            )
            .unwrap();
            assert_eq!(waited.get(), Some(Duration::from_millis(ms)));
        }
    }

    #[test]
    fn cancellation_after_modifier_down_releases_without_base_down() {
        let events = std::cell::RefCell::new(Vec::new());
        let cancelled = std::cell::Cell::new(false);
        let result = deliver_with(
            "a",
            &["cmd", "shift"],
            Duration::from_millis(8),
            |code, down, _| {
                events.borrow_mut().push((code, down));
                if code == 55 && down {
                    cancelled.set(true);
                }
                Ok(())
            },
            || cancelled.get(),
            |_| panic!("hold should not start"),
        );
        assert!(result.is_err());
        assert_eq!(*events.borrow(), vec![(55, true), (55, false)]);
    }
}
