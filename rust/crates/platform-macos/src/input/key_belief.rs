//! Process-incarnation-bound, short-lived evidence for window-only keys.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::ax::enablement::ProcessStartStamp;

const FRESH_FOR: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BeliefMatch {
    FreshKeyAndActive,
    FreshKeyInactive,
    Stale,
}

#[derive(Clone, Copy)]
struct Belief {
    focused_window: u32,
    app_active: bool,
    confirmed_at: Instant,
    generation: u64,
    focus_generation: u64,
}

struct Entry {
    stamp: ProcessStartStamp,
    generation: u64,
    observing: bool,
    active: Option<bool>,
    focus_generation: Option<u64>,
    belief: Option<Belief>,
}

#[derive(Clone, Copy)]
pub(super) struct Ticket {
    pub(super) pid: i32,
    pub(super) stamp: ProcessStartStamp,
    generation: u64,
    focus_generation: u64,
}

#[derive(Default)]
pub(super) struct BeliefStore(Mutex<HashMap<i32, Entry>>);

pub(super) static BELIEFS: LazyLock<BeliefStore> = LazyLock::new(BeliefStore::default);

impl BeliefStore {
    fn entry(map: &mut HashMap<i32, Entry>, pid: i32, stamp: ProcessStartStamp) -> &mut Entry {
        let entry = map.entry(pid).or_insert_with(|| Entry {
            stamp,
            generation: 0,
            observing: false,
            active: None,
            focus_generation: None,
            belief: None,
        });
        if entry.stamp != stamp {
            *entry = Entry {
                stamp,
                generation: 0,
                observing: false,
                active: None,
                focus_generation: None,
                belief: None,
            };
        }
        entry
    }

    pub(super) fn set_observing(&self, pid: i32, stamp: ProcessStartStamp, observing: bool) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if map.get(&pid).is_some_and(|entry| entry.stamp != stamp) {
            return;
        }
        let entry = Self::entry(&mut map, pid, stamp);
        if entry.observing != observing {
            entry.generation = entry.generation.wrapping_add(1);
            entry.belief = None;
        }
        entry.observing = observing;
        if !observing {
            entry.belief = None;
        }
    }

    pub(super) fn begin(
        &self,
        pid: i32,
        stamp: ProcessStartStamp,
        focus_generation: u64,
    ) -> Ticket {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let entry = Self::entry(&mut map, pid, stamp);
        if entry
            .focus_generation
            .is_some_and(|last| last != focus_generation)
        {
            entry.active = None;
            entry.belief = None;
        }
        entry.focus_generation = Some(focus_generation);
        Ticket {
            pid,
            stamp,
            generation: entry.generation,
            focus_generation,
        }
    }

    pub(super) fn publish(
        &self,
        ticket: Ticket,
        wid: u32,
        active: bool,
        now: Instant,
        focus_generation: u64,
    ) -> bool {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = map.get_mut(&ticket.pid) else {
            return false;
        };
        if !entry.observing
            || entry.stamp != ticket.stamp
            || entry.generation != ticket.generation
            || ticket.focus_generation != focus_generation
        {
            return false;
        }
        let active = entry.active.unwrap_or(active);
        entry.active = Some(active);
        entry.belief = Some(Belief {
            focused_window: wid,
            app_active: active,
            confirmed_at: now,
            generation: entry.generation,
            focus_generation,
        });
        true
    }

    pub(super) fn invalidate(&self, pid: i32, stamp: ProcessStartStamp) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get_mut(&pid).filter(|entry| entry.stamp == stamp) {
            entry.generation = entry.generation.wrapping_add(1);
            entry.belief = None;
            entry.active = None;
        }
    }

    pub(super) fn note_activation(&self, pid: i32, stamp: ProcessStartStamp, active: bool) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get_mut(&pid).filter(|entry| entry.stamp == stamp) {
            entry.generation = entry.generation.wrapping_add(1);
            entry.belief = None;
            entry.active = Some(active);
        }
    }

    pub(super) fn forget(&self, pid: i32) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pid);
    }

    pub(super) fn forget_if_stamp(&self, pid: i32, stamp: ProcessStartStamp) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if map.get(&pid).is_some_and(|entry| entry.stamp == stamp) {
            map.remove(&pid);
        }
    }

    pub(super) fn lookup(
        &self,
        pid: i32,
        stamp: Option<ProcessStartStamp>,
        wid: u32,
        now: Instant,
        focus_generation: u64,
    ) -> BeliefMatch {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(stamp) = stamp else {
            map.remove(&pid);
            return BeliefMatch::Stale;
        };
        let Some(entry) = map.get_mut(&pid) else {
            return BeliefMatch::Stale;
        };
        if entry.stamp != stamp {
            map.remove(&pid);
            return BeliefMatch::Stale;
        }
        let Some(belief) = entry.belief else {
            return BeliefMatch::Stale;
        };
        if !entry.observing
            || belief.generation != entry.generation
            || belief.focus_generation != focus_generation
            || belief.focused_window != wid
            || now.saturating_duration_since(belief.confirmed_at) >= FRESH_FOR
        {
            return BeliefMatch::Stale;
        }
        if belief.app_active {
            BeliefMatch::FreshKeyAndActive
        } else {
            BeliefMatch::FreshKeyInactive
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_and_incarnation_fence_publication_and_lookup() {
        let store = BeliefStore::default();
        let stamp = (1, 1);
        let now = Instant::now();
        store.set_observing(4, stamp, true);
        let stale_ticket = store.begin(4, stamp, 0);
        store.invalidate(4, stamp);
        assert!(!store.publish(stale_ticket, 700, true, now, 0));
        let ticket = store.begin(4, stamp, 0);
        assert!(store.publish(ticket, 700, true, now, 0));
        assert_eq!(
            store.lookup(4, Some(stamp), 700, now + Duration::from_millis(249), 0),
            BeliefMatch::FreshKeyAndActive
        );
        assert_eq!(
            store.lookup(4, Some(stamp), 700, now + Duration::from_millis(250), 0),
            BeliefMatch::Stale
        );
        assert_eq!(
            store.lookup(4, Some(stamp), 888, now, 0),
            BeliefMatch::Stale
        );
        assert_eq!(
            store.lookup(4, Some((1, 2)), 700, now, 0),
            BeliefMatch::Stale
        );
        store.invalidate(4, stamp);
        assert_eq!(
            store.lookup(4, Some(stamp), 700, now, 0),
            BeliefMatch::Stale
        );
    }

    #[test]
    fn observer_failure_and_driver_focus_mutation_are_fail_closed() {
        let store = BeliefStore::default();
        let stamp = (1, 1);
        let now = Instant::now();
        let ticket = store.begin(9, stamp, 0);
        assert!(!store.publish(ticket, 7, true, now, 0));
        store.set_observing(9, stamp, true);
        let ticket = store.begin(9, stamp, 0);
        assert!(!store.publish(ticket, 7, true, now, 1));
        let ticket = store.begin(9, stamp, 1);
        assert!(store.publish(ticket, 7, false, now, 1));
        assert_eq!(
            store.lookup(9, Some(stamp), 7, now, 1),
            BeliefMatch::FreshKeyInactive
        );
        assert_eq!(store.lookup(9, Some(stamp), 7, now, 2), BeliefMatch::Stale);
        assert_eq!(store.lookup(9, None, 7, now, 1), BeliefMatch::Stale);
    }

    #[test]
    fn activation_notifications_are_incarnation_bound_and_fence_tickets() {
        let store = BeliefStore::default();
        let stamp = (1, 2);
        let now = Instant::now();
        store.set_observing(9, stamp, true);
        let ticket = store.begin(9, stamp, 0);
        store.note_activation(9, (1, 1), false);
        assert!(
            store.publish(ticket, 7, true, now, 0),
            "old callback must be ignored"
        );
        store.note_activation(9, stamp, false);
        assert_eq!(store.lookup(9, Some(stamp), 7, now, 0), BeliefMatch::Stale);
        let ticket = store.begin(9, stamp, 0);
        assert!(store.publish(ticket, 7, true, now, 0));
        assert_eq!(
            store.lookup(9, Some(stamp), 7, now, 0),
            BeliefMatch::FreshKeyInactive
        );
        store.note_activation(9, stamp, true);
        let ticket = store.begin(9, stamp, 0);
        assert!(store.publish(ticket, 7, false, now, 0));
        assert_eq!(
            store.lookup(9, Some(stamp), 7, now, 0),
            BeliefMatch::FreshKeyAndActive
        );
    }

    #[test]
    fn focus_invalidation_does_not_reuse_old_activation_without_new_evidence() {
        let store = BeliefStore::default();
        let stamp = (1, 2);
        let now = Instant::now();
        store.set_observing(9, stamp, true);
        store.note_activation(9, stamp, true);
        store.invalidate(9, stamp);
        let ticket = store.begin(9, stamp, 0);
        assert!(store.publish(ticket, 7, false, now, 0));
        assert_eq!(
            store.lookup(9, Some(stamp), 7, now, 0),
            BeliefMatch::FreshKeyInactive
        );
    }
    #[test]
    fn focus_generation_change_discards_previous_activation() {
        let store = BeliefStore::default();
        let stamp = (1, 2);
        let now = Instant::now();
        store.set_observing(9, stamp, true);
        store.note_activation(9, stamp, true);
        let ticket = store.begin(9, stamp, 1);
        assert!(store.publish(ticket, 7, false, now, 1));
        let ticket = store.begin(9, stamp, 2);
        assert!(store.publish(ticket, 7, false, now, 2));
        assert_eq!(
            store.lookup(9, Some(stamp), 7, now, 2),
            BeliefMatch::FreshKeyInactive
        );
    }
}
