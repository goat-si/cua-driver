use super::{RetiredTickets, Snapshot, SnapshotPayload, SnapshotStore, StoreState, TicketState};
use crate::protocol::ToolResult;
use crate::window_basis::{basis_refusal, format_basis_id, parse_basis_id, WindowBasis};
use std::sync::{Arc, Mutex};

/// A capture's ordering ticket, owned by the code doing the capture for as
/// long as it may still publish. Every ticketed publication, refresh and
/// retirement borrows it, so it cannot be dropped before the last of them;
/// dropping it retires the ticket if it never landed (after a failed
/// capture, an early return, or a cancelled request), so an abandoned
/// capture cannot keep its window's entry (goat-si/cua-driver#133).
pub struct CaptureTicket<S> {
    state: Arc<Mutex<StoreState<S>>>,
    retired: RetiredTickets,
    pid: i32,
    window_id: u64,
    ticket: u64,
}

impl<S> CaptureTicket<S> {
    /// The ticket number: 0 once the store's counter is spent, which every
    /// publication refuses.
    pub fn ticket(&self) -> u64 {
        self.ticket
    }
}

impl<S> Drop for CaptureTicket<S> {
    fn drop(&mut self) {
        let (pid, window_id, ticket) = (self.pid, self.window_id, self.ticket);
        match self.state.try_lock() {
            Ok(mut inner) => inner.retire_ticket(pid, window_id, ticket),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                poisoned.into_inner().retire_ticket(pid, window_id, ticket)
            }
            // The store is locked, possibly by this thread (a ticket dropped
            // inside a `with_latest` visitor): never wait for it. The next
            // ticket operation retires it.
            Err(std::sync::TryLockError::WouldBlock) => self
                .retired
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((pid, window_id, ticket)),
        }
    }
}

impl<S> StoreState<S> {
    /// Whether `ticket` was issued for the window and no newer capture or plain
    /// publication has landed since.
    pub(super) fn ticket_current(&self, pid: i32, window_id: u64, ticket: u64) -> bool {
        self.tickets
            .get(&(pid, window_id))
            .is_some_and(|state| ticket != 0 && ticket <= state.issued && ticket > state.applied)
    }

    /// Record `ticket` as landed, dropping the window's entry once no newer
    /// capture is in flight. Call only after [`Self::ticket_current`].
    pub(super) fn apply_ticket(&mut self, pid: i32, window_id: u64, ticket: u64) {
        if let Some(state) = self.tickets.get_mut(&(pid, window_id)) {
            state.applied = ticket;
            if state.applied >= state.issued {
                self.tickets.remove(&(pid, window_id));
            }
        }
    }

    /// Drop the window's entry when `ticket` is the newest one issued and has
    /// not landed: nothing can land after it. A newer capture keeps the
    /// entry, and a landed ticket has already left it.
    pub(super) fn retire_ticket(&mut self, pid: i32, window_id: u64, ticket: u64) {
        if self
            .tickets
            .get(&(pid, window_id))
            .is_some_and(|state| ticket != 0 && state.issued == ticket && state.applied < ticket)
        {
            self.tickets.remove(&(pid, window_id));
        }
    }

    /// Make every capture in flight for the window obsolete. Tickets come from
    /// a counter that never restarts, so dropping the entry is enough: a later
    /// [`SnapshotStore::begin_capture`] starts a fresh entry above them.
    pub(super) fn supersede_captures(&mut self, pid: i32, window_id: u64) {
        self.tickets.remove(&(pid, window_id));
    }
}

impl<S> Snapshot<S> {
    pub(super) fn advance_basis(&mut self, basis: Option<WindowBasis>, session: Option<&str>) {
        let identical = self.screenshot_owner.as_deref() == session
            && match (&self.basis, &basis) {
                (Some(old), Some(new)) => old.same_frame(new),
                _ => false,
            };
        if !identical && !self.basis_exhausted {
            if let Some(next) = self.frame_generation.checked_add(1) {
                self.frame_generation = next;
            } else {
                self.basis_exhausted = true;
            }
        }
        self.basis = if self.basis_exhausted {
            None
        } else {
            basis.map(|mut b| {
                b.snapshot_id = format_basis_id(self.id, self.frame_generation);
                b
            })
        };
    }
}

impl<S: SnapshotPayload> SnapshotStore<S> {
    /// Lock the store for a ticket operation, first retiring the tickets that
    /// were dropped while it was locked. Lock order: `inner`, then `retired`.
    pub(super) fn lock_for_tickets(&self) -> std::sync::MutexGuard<'_, StoreState<S>> {
        let mut inner = self.inner.lock().unwrap();
        let retired = std::mem::take(&mut *self.retired.lock().unwrap_or_else(|e| e.into_inner()));
        for (pid, window_id, ticket) in retired {
            inner.retire_ticket(pid, window_id, ticket);
        }
        inner
    }

    /// Whether `ticket` was issued by this store for this window.
    pub(super) fn owns(&self, ticket: &CaptureTicket<S>, pid: i32, window_id: u64) -> bool {
        Arc::ptr_eq(&self.inner, &ticket.state)
            && ticket.pid == pid
            && ticket.window_id == window_id
    }

    /// Reserve the ordering ticket before starting a capture. Keep it until
    /// the capture's last publication; dropping it retires a ticket that
    /// never landed. Tickets never wrap: once the store's 64-bit counter is
    /// spent the ticket is 0, which every publication and refresh refuses.
    pub fn begin_capture(&self, pid: i32, window_id: u64) -> CaptureTicket<S> {
        let ticket = self.reserve_ticket(pid, window_id);
        CaptureTicket {
            state: self.inner.clone(),
            retired: self.retired.clone(),
            pid,
            window_id,
            ticket,
        }
    }

    /// A ticket with any number, for tests of tickets no capture was issued.
    #[cfg(test)]
    pub(super) fn forge_ticket(&self, pid: i32, window_id: u64, ticket: u64) -> CaptureTicket<S> {
        CaptureTicket {
            state: self.inner.clone(),
            retired: self.retired.clone(),
            pid,
            window_id,
            ticket,
        }
    }

    fn reserve_ticket(&self, pid: i32, window_id: u64) -> u64 {
        let mut inner = self.lock_for_tickets();
        let Some(ticket) = inner.last_ticket.checked_add(1) else {
            return 0;
        };
        inner.last_ticket = ticket;
        // A new entry starts just below this ticket, so tickets issued before
        // the window's previous entry was dropped stay obsolete.
        inner
            .tickets
            .entry((pid, window_id))
            .or_insert(TicketState {
                issued: ticket,
                applied: ticket - 1,
            })
            .issued = ticket;
        ticket
    }

    /// Atomically publish a semantic snapshot, screenshot and optional basis.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_basis_for_session(
        &self,
        pid: i32,
        window_id: u64,
        payload: S,
        session: Option<&str>,
        screenshot_scale: Option<f64>,
        basis: Option<WindowBasis>,
        ticket: &CaptureTicket<S>,
    ) -> Option<(u32, Vec<u32>, Option<WindowBasis>)> {
        if !self.owns(ticket, pid, window_id) {
            return None;
        }
        self.publish_snapshot(
            pid,
            window_id,
            payload,
            session,
            (screenshot_scale, None),
            (true, Some((ticket.ticket(), basis))),
        )
    }

    /// Publish the first screenshot-only snapshot and its basis in one critical section.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_capture_basis_for_session(
        &self,
        pid: i32,
        window_id: u64,
        payload: S,
        session: Option<&str>,
        screenshot_scale: Option<f64>,
        basis: Option<WindowBasis>,
        ticket: &CaptureTicket<S>,
    ) -> Option<(u32, Vec<u32>, Option<WindowBasis>)> {
        if !self.owns(ticket, pid, window_id) {
            return None;
        }
        self.publish_snapshot(
            pid,
            window_id,
            payload,
            session,
            (screenshot_scale, None),
            (false, Some((ticket.ticket(), basis))),
        )
    }

    /// Atomically refresh an existing frame; an obsolete ticket changes nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn refresh_basis_for_session(
        &self,
        pid: i32,
        window_id: u64,
        session: Option<&str>,
        screenshot_scale: f64,
        window_size: Option<(f64, f64)>,
        basis: Option<WindowBasis>,
        ticket: &CaptureTicket<S>,
    ) -> Option<(u32, Option<WindowBasis>)> {
        if !self.owns(ticket, pid, window_id) {
            return None;
        }
        let ticket = ticket.ticket();
        let mut inner = self.lock_for_tickets();
        if session.is_some_and(crate::session::is_session_ended) {
            // No publication can use this capture's ticket any more.
            inner.retire_ticket(pid, window_id, ticket);
            return None;
        }
        if basis
            .as_ref()
            .is_some_and(|b| b.pid != pid || b.window_id != window_id)
        {
            return None;
        }
        if !inner.ticket_current(pid, window_id, ticket) {
            return None;
        }
        let snapshot = inner
            .get_mut(&pid)?
            .iter_mut()
            .find(|s| s.window_id == window_id)
            // The ticket must postdate the snapshot and its current frame:
            // an older capture would attach an older frame to newer state.
            .filter(|s| ticket > s.ticket_floor)?;
        snapshot.advance_basis(basis, session);
        snapshot.screenshot_owner = session.map(str::to_owned);
        snapshot.screenshot_scale = Some(screenshot_scale);
        snapshot.screenshot_window_size = window_size;
        snapshot.zoom = None;
        snapshot.ticket_floor = ticket;
        let id = snapshot.id;
        let stored = snapshot.basis.clone();
        inner.apply_ticket(pid, window_id, ticket);
        Some((id, stored))
    }

    pub fn basis_for(
        &self,
        pid: i32,
        window_id: u64,
        basis_id: &str,
        session: Option<&str>,
    ) -> Result<WindowBasis, ToolResult> {
        let inner = self.inner.lock().unwrap();
        let Some((id, generation)) = parse_basis_id(basis_id) else {
            return Err(basis_refusal("stale_basis", "unknown_snapshot", None, None));
        };
        let Some(snapshot) = inner
            .get(&pid)
            .and_then(|lane| lane.iter().find(|s| s.id == id))
        else {
            return Err(basis_refusal("stale_basis", "unknown_snapshot", None, None));
        };
        let reason = if snapshot.window_id != window_id {
            Some("other_window")
        } else if snapshot.screenshot_owner.as_deref() != session {
            Some("other_session")
        } else if snapshot.basis_exhausted {
            Some("no_basis")
        } else if snapshot.frame_generation != generation {
            Some("frame_replaced")
        } else if snapshot.basis.is_none() {
            Some("no_basis")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(basis_refusal("stale_basis", reason, None, None));
        }
        Ok(snapshot.basis.clone().expect("basis checked"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) struct Payload;
    impl SnapshotPayload for Payload {
        type Element = ();
        fn len(&self) -> usize {
            0
        }
        fn retain(&self, _: usize) -> Option<()> {
            None
        }
    }
    pub(super) fn frame() -> WindowBasis {
        WindowBasis {
            window_id: 2,
            pid: 1,
            snapshot_id: String::new(),
            x: 0,
            y: 0,
            width: 10,
            height: 10,
            scale_x: 1.0,
            scale_y: 1.0,
            capture_width: 10,
            capture_height: 10,
            display_scale: None,
            captured_at_ms: 0,
        }
    }
    #[test]
    fn capture_removal_preserves_order_and_retires_old_snapshot() {
        let store = SnapshotStore::new();
        let (old_id, _) = store
            .publish_for_session(1, 2, Payload, None, Some(1.0))
            .unwrap();
        let older = store.begin_capture(1, 2);
        let own = store.begin_capture(1, 2);
        let newer = store.begin_capture(1, 2);
        assert_eq!(store.remove_for_capture(1, 2, &own), Some(old_id));
        assert!(store.screenshot_context(1, Some(2), None).is_err());
        assert!(store
            .publish_capture_basis_for_session(
                1,
                2,
                Payload,
                None,
                Some(1.0),
                Some(frame()),
                &older
            )
            .is_none());
        let (id, _, basis) = store
            .publish_capture_basis_for_session(1, 2, Payload, None, Some(2.0), Some(frame()), &own)
            .unwrap();
        let basis = basis.unwrap();
        assert_ne!(id, old_id);
        assert_eq!(
            store
                .basis_for(1, 2, &basis.snapshot_id, None)
                .unwrap()
                .snapshot_id,
            basis.snapshot_id
        );
        assert_eq!(
            store.screenshot_context(1, Some(2), None).unwrap().scale,
            2.0
        );
        assert!(store
            .refresh_basis_for_session(1, 2, None, 3.0, None, Some(frame()), &newer)
            .is_some());
        assert_eq!(store.remove_for_capture(1, 2, &own), None);
        assert_eq!(
            store.screenshot_context(1, Some(2), None).unwrap().scale,
            3.0
        );
        assert!(store
            .publish_capture_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &own)
            .is_none());
    }
    #[test]
    fn publication_returns_the_stored_frame_and_rejects_stale_tickets() {
        let store = SnapshotStore::new();
        let stale = store.begin_capture(1, 2);
        let ticket = store.begin_capture(1, 2);
        let (id, _, stored) = store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &ticket)
            .unwrap();
        assert_eq!(stored.unwrap().snapshot_id, format_basis_id(id, 0));
        assert!(store
            .refresh_basis_for_session(1, 2, None, 1.0, None, None, &stale)
            .is_none());
        let ticket = store.begin_capture(1, 2);
        let (_, stored) = store
            .refresh_basis_for_session(1, 2, None, 1.0, None, Some(frame()), &ticket)
            .unwrap();
        assert_eq!(stored.unwrap().snapshot_id, format_basis_id(id, 0));
        let ticket = store.begin_capture(1, 3);
        let (_, _, stored) = store
            .publish_capture_basis_for_session(1, 3, Payload, None, None, None, &ticket)
            .unwrap();
        assert!(stored.is_none());
        let ticket = store.begin_capture(1, 2);
        let (id, _, stored) = store
            .publish_capture_basis_for_session(
                1,
                2,
                Payload,
                None,
                Some(1.0),
                Some(frame()),
                &ticket,
            )
            .unwrap();
        assert_eq!(stored.unwrap().snapshot_id, format_basis_id(id, 0));
    }
    #[test]
    fn generation_overflow_permanently_clears_basis() {
        let store = SnapshotStore::new();
        let t = store.begin_capture(1, 2);
        let id = store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &t)
            .unwrap()
            .0;
        store.inner.lock().unwrap().get_mut(&1).unwrap()[0].frame_generation = u32::MAX;
        let t = store.begin_capture(1, 2);
        store
            .refresh_basis_for_session(1, 2, None, 1.0, None, None, &t)
            .unwrap();
        let t = store.begin_capture(1, 2);
        store
            .refresh_basis_for_session(1, 2, None, 1.0, None, Some(frame()), &t)
            .unwrap();
        let result = store
            .basis_for(1, 2, &format_basis_id(id, u32::MAX), None)
            .unwrap_err();
        assert_eq!(result.structured_content.unwrap()["reason"], "no_basis");
    }
    #[test]
    fn ticket_state_is_pruned_and_never_reused() {
        let store = SnapshotStore::new();
        let t = store.begin_capture(1, 2);
        store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &t)
            .unwrap();
        // Nothing is in flight once the newest ticket lands.
        assert!(store.inner.lock().unwrap().tickets.is_empty());

        // A capture abandoned before clear() can neither land nor match a
        // ticket issued afterwards.
        let abandoned = store.begin_capture(1, 2);
        assert_eq!(store.clear(), 1);
        assert!(store.inner.lock().unwrap().tickets.is_empty());
        let fresh = store.begin_capture(1, 2);
        assert!(fresh.ticket() > abandoned.ticket());
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &abandoned)
            .is_none());
        let (id, _, _) = store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &fresh)
            .unwrap();

        // remove() drops the window's in-flight captures with its snapshot.
        let in_flight = store.begin_capture(1, 2);
        assert_eq!(store.remove(1, 2), Some(id));
        assert!(store.inner.lock().unwrap().tickets.is_empty());
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &in_flight)
            .is_none());
    }

    #[test]
    fn exhausted_ticket_counter_refuses_instead_of_panicking() {
        let store = SnapshotStore::new();
        store.inner.lock().unwrap().last_ticket = u64::MAX;
        let t = store.begin_capture(1, 2);
        assert_eq!(t.ticket(), 0);
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &t)
            .is_none());
        assert!(store.inner.lock().unwrap().tickets.is_empty());
    }
}

#[cfg(test)]
mod review_regressions {
    use super::tests::{frame, Payload};
    use super::*;

    #[test]
    fn legacy_publications_fence_older_captures() {
        for kind in 0..4 {
            let store = SnapshotStore::new();
            store
                .publish_for_session(1, 2, Payload, None, Some(1.0))
                .unwrap();
            let old = store.begin_capture(1, 2);
            let id = match kind {
                0 => {
                    store
                        .publish_for_session(1, 2, Payload, None, Some(2.0))
                        .unwrap()
                        .0
                }
                1 => {
                    store
                        .publish_sized_for_session(
                            1,
                            2,
                            Payload,
                            None,
                            Some(2.0),
                            Some((10.0, 10.0)),
                        )
                        .unwrap()
                        .0
                }
                2 => {
                    store
                        .publish_capture_for_session(1, 2, Payload, None, Some(2.0))
                        .unwrap()
                        .0
                }
                _ => store
                    .refresh_screenshot_for_session(1, 2, None, 2.0, None)
                    .unwrap(),
            };
            assert!(
                store
                    .publish_basis_for_session(1, 2, Payload, None, Some(3.0), Some(frame()), &old)
                    .is_none(),
                "kind {kind}"
            );
            assert!(
                store
                    .refresh_basis_for_session(1, 2, None, 3.0, None, Some(frame()), &old)
                    .is_none(),
                "kind {kind}"
            );
            assert_eq!(
                store
                    .screenshot_context(1, Some(2), None)
                    .unwrap()
                    .snapshot_id,
                id
            );
            assert_eq!(
                store.screenshot_context(1, Some(2), None).unwrap().scale,
                2.0
            );
        }
    }

    #[test]
    fn retired_tickets_cannot_reappear_after_new_capture() {
        for kind in 0..3 {
            let store = SnapshotStore::new();
            store
                .publish_for_session(1, 2, Payload, Some("review-session"), Some(1.0))
                .unwrap();
            let old = store.begin_capture(1, 2);
            match kind {
                0 => {
                    store.remove(1, 2);
                }
                1 => {
                    store.clear();
                }
                _ => {
                    store.retire_session_screenshots("review-session");
                }
            }
            assert!(!store.inner.lock().unwrap().tickets.contains_key(&(1, 2)));
            let fresh = store.begin_capture(1, 2);
            assert_ne!(old.ticket(), fresh.ticket());
            assert!(store
                .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &old)
                .is_none());
            assert!(store
                .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &fresh)
                .is_some());
        }
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::tests::{frame, Payload};
    use super::*;

    #[test]
    fn invalid_and_out_of_order_tickets_and_wrong_basis_are_rejected() {
        let store = SnapshotStore::new();
        let first = store.begin_capture(1, 2);
        let second = store.begin_capture(1, 2);
        for ticket in [0, second.ticket() + 1] {
            let ticket = store.forge_ticket(1, 2, ticket);
            assert!(store
                .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &ticket)
                .is_none());
        }
        for wrong_pid in [true, false] {
            let mut wrong = frame();
            if wrong_pid {
                wrong.pid = 3;
            } else {
                wrong.window_id = 3;
            }
            assert!(store
                .publish_basis_for_session(
                    1,
                    2,
                    Payload,
                    None,
                    Some(1.0),
                    Some(wrong.clone()),
                    &first
                )
                .is_none());
            assert!(store
                .publish_capture_basis_for_session(
                    1,
                    2,
                    Payload,
                    None,
                    Some(1.0),
                    Some(wrong),
                    &first
                )
                .is_none());
        }
        let (id, _, _) = store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &first)
            .unwrap();
        for ticket in [0, second.ticket() + 1] {
            let ticket = store.forge_ticket(1, 2, ticket);
            assert!(store
                .refresh_basis_for_session(1, 2, None, 2.0, None, Some(frame()), &ticket)
                .is_none());
        }
        for wrong_pid in [true, false] {
            let mut wrong = frame();
            if wrong_pid {
                wrong.pid = 3;
            } else {
                wrong.window_id = 3;
            }
            assert!(store
                .refresh_basis_for_session(1, 2, None, 2.0, None, Some(wrong), &second)
                .is_none());
        }
        let third = store.begin_capture(1, 2);
        assert!(store
            .refresh_basis_for_session(1, 2, None, 3.0, None, Some(frame()), &third)
            .is_some());
        assert!(store
            .refresh_basis_for_session(1, 2, None, 2.0, None, Some(frame()), &second)
            .is_none());
        let current = store.screenshot_context(1, Some(2), None).unwrap();
        assert_eq!((current.snapshot_id, current.scale), (id, 3.0));
    }

    #[test]
    fn evicted_window_loses_its_ticket_state() {
        let store = SnapshotStore::new();
        store
            .publish_for_session(1, 2, Payload, None, Some(1.0))
            .unwrap();
        let old = store.begin_capture(1, 2);
        for window in 3..3 + crate::element_token::LRU_CAP_PER_PID as u64 {
            store
                .publish_for_session(1, window, Payload, None, Some(1.0))
                .unwrap();
        }
        assert!(!store.inner.lock().unwrap().tickets.contains_key(&(1, 2)));
        let fresh = store.begin_capture(1, 2);
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &old)
            .is_none());
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &fresh)
            .is_some());
    }
}

#[cfg(test)]
mod capture_ticket_tests {
    //! An abandoned capture cannot keep its window's ticket entry
    //! (goat-si/cua-driver#133).
    use super::tests::{frame, Payload};
    use super::*;

    fn entries(store: &SnapshotStore<Payload>) -> usize {
        store.inner.lock().unwrap().tickets.len()
    }

    #[test]
    fn a_dropped_unpublished_ticket_leaves_no_entry() {
        let store = SnapshotStore::new();
        let ticket = store.begin_capture(1, 2);
        assert_eq!(entries(&store), 1);
        drop(ticket);
        assert_eq!(entries(&store), 0);
    }

    #[test]
    fn dropping_a_landed_ticket_does_not_touch_a_newer_capture() {
        let store = SnapshotStore::new();
        let landed = store.begin_capture(1, 2);
        store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &landed)
            .unwrap();
        let newer = store.begin_capture(1, 2);
        drop(landed);
        assert_eq!(entries(&store), 1);
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &newer)
            .is_some());
        drop(newer);
        assert_eq!(entries(&store), 0);
    }

    /// Linux's screenshot-only read: a refresh that finds no snapshot, then a
    /// capture-only publication with the same ticket.
    #[test]
    fn a_refresh_without_a_snapshot_then_a_capture_only_publication_lands() {
        let store = SnapshotStore::new();
        let ticket = store.begin_capture(1, 2);
        assert!(store
            .refresh_basis_for_session(1, 2, None, 1.0, None, Some(frame()), &ticket)
            .is_none());
        assert_eq!(entries(&store), 1, "a refused refresh keeps the ticket");
        assert!(store
            .publish_capture_basis_for_session(
                1,
                2,
                Payload,
                None,
                Some(1.0),
                Some(frame()),
                &ticket
            )
            .is_some());
        drop(ticket);
        assert_eq!(entries(&store), 0);
    }

    #[test]
    fn remove_for_capture_then_a_publication_with_the_same_ticket_lands() {
        let store = SnapshotStore::new();
        let old = store.publish(1, 2, Payload);
        let ticket = store.begin_capture(1, 2);
        assert_eq!(store.remove_for_capture(1, 2, &ticket), Some(old));
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &ticket)
            .is_some());
        drop(ticket);
        assert_eq!(entries(&store), 0);
    }

    #[test]
    fn an_older_dropped_ticket_keeps_the_newer_capture_publishable() {
        let store = SnapshotStore::new();
        let older = store.begin_capture(1, 2);
        let newer = store.begin_capture(1, 2);
        drop(older);
        assert_eq!(entries(&store), 1);
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &newer)
            .is_some());
    }

    /// Dropping the newest ticket retires the entry: an older capture still
    /// in flight then refuses (fail closed).
    #[test]
    fn a_dropped_newest_ticket_makes_older_captures_refuse() {
        let store = SnapshotStore::new();
        let older = store.begin_capture(1, 2);
        drop(store.begin_capture(1, 2));
        assert_eq!(entries(&store), 0);
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &older)
            .is_none());
        drop(older);
        assert_eq!(entries(&store), 0);
    }

    /// A cancelled request drops the ticket it held.
    #[tokio::test]
    async fn a_cancelled_capture_drops_its_ticket() {
        let store = SnapshotStore::<Payload>::new();
        let ticket = store.begin_capture(1, 2);
        let capture = tokio::spawn(async move {
            let _ticket = ticket;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(entries(&store), 1);
        capture.abort();
        assert!(capture.await.unwrap_err().is_cancelled());
        assert_eq!(entries(&store), 0);
    }

    #[test]
    fn an_ended_session_retires_the_ticket_on_publication_and_refresh() {
        let store = SnapshotStore::new();
        let session = "drv-133-ended-session";
        crate::session::end_session(session);
        let ticket = store.begin_capture(1, 2);
        let _keep = store.begin_capture(1, 3);
        assert!(store
            .publish_basis_for_session(
                1,
                2,
                Payload,
                Some(session),
                Some(1.0),
                Some(frame()),
                &ticket
            )
            .is_none());
        assert_eq!(entries(&store), 1, "window 2 retired, window 3 kept");
        let ticket = store.begin_capture(1, 2);
        assert!(store
            .refresh_basis_for_session(1, 2, Some(session), 1.0, None, Some(frame()), &ticket)
            .is_none());
        assert_eq!(entries(&store), 1);
    }

    /// A ticket from another store with the same number, or for another
    /// window of the same process whose number falls in this window's
    /// in-flight range, is refused by every ticketed call and changes
    /// nothing. Each call here would succeed without its ownership check.
    #[test]
    fn a_ticket_is_refused_by_another_store_and_another_window() {
        let store = SnapshotStore::new();
        let other_store = SnapshotStore::new();
        let id = store.publish(1, 2, Payload);
        let _older = store.begin_capture(1, 2);
        let other_window = store.begin_capture(1, 3);
        let own = store.begin_capture(1, 2);
        let foreign = other_store.begin_capture(1, 2);
        assert!(store
            .inner
            .lock()
            .unwrap()
            .ticket_current(1, 2, foreign.ticket()));
        assert!(store
            .inner
            .lock()
            .unwrap()
            .ticket_current(1, 2, other_window.ticket()));
        for wrong in [&foreign, &other_window] {
            assert!(store
                .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), wrong)
                .is_none());
            assert!(store
                .publish_capture_basis_for_session(
                    1,
                    2,
                    Payload,
                    None,
                    Some(1.0),
                    Some(frame()),
                    wrong
                )
                .is_none());
            assert!(store
                .refresh_basis_for_session(1, 2, None, 1.0, None, Some(frame()), wrong)
                .is_none());
            assert_eq!(store.remove_for_capture(1, 2, wrong), None);
        }
        assert_eq!(
            store.window_for_snapshot(1, id),
            Some(2),
            "snapshot untouched"
        );
        assert_eq!(entries(&store), 2);
        assert_eq!(entries(&other_store), 1);
        assert!(store
            .refresh_basis_for_session(1, 2, None, 1.0, None, Some(frame()), &own)
            .is_some());
        assert!(other_store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), &foreign)
            .is_some());
    }

    /// A ticket dropped inside a visitor of its own store (which runs with
    /// the store locked) does not deadlock; the next ticket operation
    /// retires it.
    #[test]
    fn a_ticket_dropped_while_its_store_is_locked_is_retired_later() {
        let store = std::sync::Arc::new(SnapshotStore::new());
        store.publish(1, 2, Payload);
        let ticket = store.begin_capture(1, 2);
        let (done, finished) = std::sync::mpsc::channel();
        let visiting = store.clone();
        std::thread::spawn(move || {
            visiting.with_latest(1, 2, move |_, _| drop(ticket));
            done.send(()).unwrap();
        });
        finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("dropping a ticket inside a visitor deadlocked");
        assert_eq!(entries(&store), 1, "retired by the next ticket operation");
        drop(store.begin_capture(1, 9));
        assert_eq!(entries(&store), 0);
    }
}
