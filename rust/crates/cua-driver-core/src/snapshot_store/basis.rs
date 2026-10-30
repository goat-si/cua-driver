use super::{Snapshot, SnapshotPayload, SnapshotStore};
use crate::protocol::ToolResult;
use crate::window_basis::{basis_refusal, format_basis_id, parse_basis_id, WindowBasis};

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
    /// Reserve the ordering ticket before starting a capture. Tickets never wrap.
    pub fn begin_capture(&self, pid: i32, window_id: u64) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let state = inner.tickets.entry((pid, window_id)).or_default();
        state.next = state
            .next
            .checked_add(1)
            .expect("capture ticket space exhausted");
        state.next
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
        let state = inner.tickets.get(&(pid, window_id))?;
        if ticket == 0 || ticket > state.next || ticket <= state.applied {
            return None;
        }
        let snapshot = inner
            .get_mut(&pid)?
            .iter_mut()
            .find(|s| s.window_id == window_id)?;
        snapshot.advance_basis(basis, session);
        snapshot.screenshot_owner = session.map(str::to_owned);
        snapshot.screenshot_scale = Some(screenshot_scale);
        snapshot.screenshot_window_size = window_size;
        snapshot.zoom = None;
        let id = snapshot.id;
        inner
            .tickets
            .get_mut(&(pid, window_id))
            .expect("ticket exists")
            .applied = ticket;
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
}
