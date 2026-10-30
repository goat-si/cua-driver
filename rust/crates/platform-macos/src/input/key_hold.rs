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
    Global,
    BareGlobal,
}

pub(crate) fn deliver_menu(
    pid: i32,
    key: &str,
    modifiers: &[&str],
    duration: Option<Duration>,
    control: &HoldControl,
) -> keyboard::ChordPost {
    deliver_menu_with(
        keyboard::chord_events(key, modifiers),
        |event| keyboard::post_chord_event_no_auth(pid, event),
        || control.is_cancelled(),
        || control.wait(duration.unwrap_or_else(keyboard::key_gap)),
    )
}

fn deliver_menu_with<E>(
    built: anyhow::Result<(E, E)>,
    mut post: impl FnMut(&E),
    cancelled: impl FnOnce() -> bool,
    wait: impl FnOnce() -> bool,
) -> keyboard::ChordPost {
    let (down, up) = match built {
        Ok(events) => events,
        Err(error) => return keyboard::ChordPost::NotPosted(error.to_string()),
    };
    if cancelled() {
        return keyboard::ChordPost::NotPosted("key delivery cancelled".into());
    }
    post(&down);
    let interrupted = wait();
    post(&up);
    if interrupted {
        keyboard::ChordPost::Uncertain("key delivery cancelled after posting".into())
    } else {
        keyboard::ChordPost::Posted
    }
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
    deliver_with_pacing(
        key,
        modifiers,
        duration.unwrap_or_else(keyboard::key_gap),
        (
            matches!(route, KeyRoute::Global | KeyRoute::BareGlobal),
            matches!(route, KeyRoute::BareGlobal),
        ),
        |code, down, flags| match route {
            KeyRoute::Pid(pid) => keyboard::post_key(pid, code, down, flags),
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
                post_hid(|| event.post(CGEventTapLocation::HID));
                Ok(())
            }
        },
        || control.is_cancelled(),
        |hold| control.wait(hold),
    )
}

/// A key posted to the HID queue. Recorded so a pending focus restoration
/// does not read the driver's own key as the person's input.
fn post_hid(send: impl FnOnce()) {
    crate::input::hid_history::post(send);
}

#[cfg(test)]
pub(crate) fn deliver_with(
    key: &str,
    modifiers: &[&str],
    duration: Duration,
    post: impl FnMut(u16, bool, CGEventFlags) -> anyhow::Result<()>,
    cancelled: impl FnMut() -> bool,
    wait: impl FnMut(Duration) -> bool,
) -> anyhow::Result<()> {
    deliver_with_pacing(
        key,
        modifiers,
        duration,
        (false, false),
        post,
        cancelled,
        wait,
    )
}

fn deliver_with_pacing(
    key: &str,
    modifiers: &[&str],
    duration: Duration,
    pacing: (bool, bool),
    mut post: impl FnMut(u16, bool, CGEventFlags) -> anyhow::Result<()>,
    mut cancelled: impl FnMut() -> bool,
    mut wait: impl FnMut(Duration) -> bool,
) -> anyhow::Result<()> {
    let (pace_modifiers, pace_base_up) = pacing;
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
        if pace_modifiers && wait(keyboard::key_gap()) {
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
    if base_attempted && pace_base_up {
        wait(keyboard::key_gap());
    }
    for (code, flag) in pressed.into_iter().rev() {
        flags.remove(flag);
        if let Err(error) = post(code, false, flags) {
            let _ = post(code, false, flags);
            if result.is_ok() {
                result = Err(error);
            }
        }
        if pace_modifiers {
            wait(keyboard::key_gap());
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

    /// A bare-global key reaches the HID queue: it must be in the HID history,
    /// or a pending pixel focus restoration abandons with reason user_input.
    #[test]
    fn bare_global_hid_post_is_recorded_as_the_drivers_own() {
        let since = std::time::Instant::now();
        let mut sent = false;
        post_hid(|| sent = true);
        assert!(sent);
        assert!(
            crate::input::hid_history::recorded_since(since) >= 1,
            "bare-global HID post missing from the HID history"
        );
    }

    #[test]
    fn global_chord_paces_each_modifier_transition() {
        let waits = std::cell::RefCell::new(Vec::new());
        deliver_with_pacing(
            "a",
            &["cmd", "shift"],
            Duration::from_millis(99),
            (true, false),
            |_, _, _| Ok(()),
            || false,
            |duration| {
                waits.borrow_mut().push(duration);
                false
            },
        )
        .unwrap();
        assert_eq!(
            *waits.borrow(),
            vec![
                keyboard::key_gap(),
                keyboard::key_gap(),
                Duration::from_millis(99),
                keyboard::key_gap(),
                keyboard::key_gap()
            ]
        );
    }

    #[test]
    fn bare_global_paces_after_base_up() {
        let events = std::cell::RefCell::new(Vec::new());
        deliver_with_pacing(
            "a",
            &["cmd"],
            Duration::from_millis(9),
            (true, true),
            |_, down, _| {
                events.borrow_mut().push(if down { "down" } else { "up" });
                Ok(())
            },
            || false,
            |_| {
                events.borrow_mut().push("wait");
                false
            },
        )
        .unwrap();
        assert_eq!(
            *events.borrow(),
            vec!["down", "wait", "down", "wait", "up", "wait", "up", "wait"]
        );
    }

    #[test]
    fn menu_construction_failure_is_clean_and_interruption_releases() {
        let events = std::cell::RefCell::new(Vec::new());
        let failed = deliver_menu_with::<u8>(
            Err(anyhow::anyhow!("build failed")),
            |event| events.borrow_mut().push(*event),
            || false,
            || false,
        );
        assert_eq!(
            failed,
            keyboard::ChordPost::NotPosted("build failed".into())
        );
        assert!(events.borrow().is_empty());
        let interrupted = deliver_menu_with(
            Ok((1, 2)),
            |event| events.borrow_mut().push(*event),
            || false,
            || true,
        );
        assert!(matches!(interrupted, keyboard::ChordPost::Uncertain(_)));
        assert_eq!(*events.borrow(), vec![1, 2]);
    }

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
    #[test]
    fn physical_chord_posts_modifiers_and_releases_in_reverse_order() {
        let events = std::cell::RefCell::new(Vec::new());
        deliver_with(
            "a",
            &["cmd", "shift"],
            std::time::Duration::ZERO,
            |code, down, _| {
                events.borrow_mut().push((code, down));
                Ok(())
            },
            || false,
            |_| false,
        )
        .unwrap();
        assert_eq!(
            *events.borrow(),
            vec![
                (55, true),
                (56, true),
                (0, true),
                (0, false),
                (56, false),
                (55, false)
            ]
        );
    }

    #[test]
    fn failed_base_key_down_releases_pressed_modifiers() {
        let events = std::cell::RefCell::new(Vec::new());
        let result = deliver_with(
            "a",
            &["cmd", "shift"],
            std::time::Duration::ZERO,
            |code, down, _| {
                events.borrow_mut().push((code, down));
                if code == 0 && down {
                    anyhow::bail!("injected failure");
                }
                Ok(())
            },
            || false,
            |_| false,
        );
        assert!(result.is_err());
        assert_eq!(
            *events.borrow(),
            vec![
                (55, true),
                (56, true),
                (0, true),
                (0, false),
                (56, false),
                (55, false)
            ]
        );
    }

    #[test]
    fn failed_base_key_up_still_releases_modifiers_in_reverse_order() {
        let events = std::cell::RefCell::new(Vec::new());
        let result = deliver_with(
            "a",
            &["cmd", "shift"],
            std::time::Duration::ZERO,
            |code, down, _| {
                events.borrow_mut().push((code, down));
                if code == 0 && !down {
                    anyhow::bail!("injected failure");
                }
                Ok(())
            },
            || false,
            |_| false,
        );
        assert!(result.is_err());
        assert_eq!(
            *events.borrow(),
            vec![
                (55, true),
                (56, true),
                (0, true),
                (0, false),
                (0, false),
                (56, false),
                (55, false)
            ]
        );
    }
}
