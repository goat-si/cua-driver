use super::{Snapshot, SnapshotPayload, SnapshotStore, StoreState, TicketState};
use crate::protocol::ToolResult;
use crate::window_basis::{basis_refusal, format_basis_id, parse_basis_id, WindowBasis};

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
    /// Reserve the ordering ticket before starting a capture. Tickets never
    /// wrap: once the store's 64-bit counter is spent this returns 0, a ticket
    /// every publication and refresh refuses.
    pub fn begin_capture(&self, pid: i32, window_id: u64) -> u64 {
        let mut inner = self.inner.lock().unwrap();
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
        ticket: u64,
    ) -> Option<(u32, Vec<u32>)> {
        self.publish_snapshot(
            pid,
            window_id,
            payload,
            session,
            (screenshot_scale, None),
            (true, Some((ticket, basis))),
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
        ticket: u64,
    ) -> Option<(u32, Vec<u32>)> {
        self.publish_snapshot(
            pid,
            window_id,
            payload,
            session,
            (screenshot_scale, None),
            (false, Some((ticket, basis))),
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
        ticket: u64,
    ) -> Option<u32> {
        let mut inner = self.inner.lock().unwrap();
        if session.is_some_and(crate::session::is_session_ended)
            || basis
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
        inner.apply_ticket(pid, window_id, ticket);
        Some(id)
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
    struct Payload;
    impl SnapshotPayload for Payload {
        type Element = ();
        fn len(&self) -> usize {
            0
        }
        fn retain(&self, _: usize) -> Option<()> {
            None
        }
    }
    fn frame() -> WindowBasis {
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
    fn generation_overflow_permanently_clears_basis() {
        let store = SnapshotStore::new();
        let t = store.begin_capture(1, 2);
        let id = store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), t)
            .unwrap()
            .0;
        store.inner.lock().unwrap().get_mut(&1).unwrap()[0].frame_generation = u32::MAX;
        let t = store.begin_capture(1, 2);
        store
            .refresh_basis_for_session(1, 2, None, 1.0, None, None, t)
            .unwrap();
        let t = store.begin_capture(1, 2);
        store
            .refresh_basis_for_session(1, 2, None, 1.0, None, Some(frame()), t)
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
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), t)
            .unwrap();
        // Nothing is in flight once the newest ticket lands.
        assert!(store.inner.lock().unwrap().tickets.is_empty());

        // A capture abandoned before clear() can neither land nor match a
        // ticket issued afterwards.
        let abandoned = store.begin_capture(1, 2);
        assert_eq!(store.clear(), 1);
        assert!(store.inner.lock().unwrap().tickets.is_empty());
        let fresh = store.begin_capture(1, 2);
        assert!(fresh > abandoned);
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), abandoned)
            .is_none());
        let (id, _) = store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), fresh)
            .unwrap();

        // remove() drops the window's in-flight captures with its snapshot.
        let in_flight = store.begin_capture(1, 2);
        assert_eq!(store.remove(1, 2), Some(id));
        assert!(store.inner.lock().unwrap().tickets.is_empty());
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), in_flight)
            .is_none());
    }

    #[test]
    fn exhausted_ticket_counter_refuses_instead_of_panicking() {
        let store = SnapshotStore::new();
        store.inner.lock().unwrap().last_ticket = u64::MAX;
        let t = store.begin_capture(1, 2);
        assert_eq!(t, 0);
        assert!(store
            .publish_basis_for_session(1, 2, Payload, None, Some(1.0), Some(frame()), t)
            .is_none());
        assert!(store.inner.lock().unwrap().tickets.is_empty());
    }
}
