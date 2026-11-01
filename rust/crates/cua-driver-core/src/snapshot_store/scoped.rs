use super::{
    InstanceCapture, InstanceEvidence, Snapshot, SnapshotPayload, SnapshotStore, StoreState,
};
use crate::browser::types::ProcessFingerprint;
use crate::element_token::{parse_token, refusal};
use crate::protocol::ToolResult;
use cua_driver_contract::IncompleteReason;
use std::sync::{Arc, Mutex};

/// Maximum number of rooted observations retained for one full snapshot.
pub const SCOPED_CAP_PER_PARENT: usize = 8;

pub(super) struct ScopedEntry<S> {
    pub(super) parent_id: u32,
    pub(super) parent_epoch: u64,
    pub(super) root_path: Vec<usize>,
    pub(super) snapshot: Snapshot<S>,
}

/// A selected root. It is not evidence that the native element is live.
pub struct ScopedCapture<S: SnapshotPayload> {
    state: Arc<Mutex<StoreState<S>>>,
    pid: i32,
    window_id: u64,
    parent_id: u32,
    parent_epoch: u64,
    source_id: u32,
    session: Option<String>,
    instance: ProcessFingerprint,
    path: Vec<usize>,
    element: S::Element,
}

impl<S: SnapshotPayload> std::fmt::Debug for ScopedCapture<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedCapture")
            .field("pid", &self.pid)
            .field("window_id", &self.window_id)
            .field("parent_id", &self.parent_id)
            .finish()
    }
}

/// A root freshly re-resolved by the platform outside the store lock.
pub struct ValidatedScopedCapture<S: SnapshotPayload> {
    capture: ScopedCapture<S>,
    live_element: S::Element,
}

impl<S: SnapshotPayload> std::fmt::Debug for ValidatedScopedCapture<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatedScopedCapture")
            .field("capture", &self.capture)
            .finish()
    }
}

impl<S: SnapshotPayload> ScopedCapture<S> {
    /// Re-resolve the structural path from the live window, check its process
    /// and window identity, compare it to the retained root, and return the
    /// fresh element. The callback runs without a store lock.
    pub fn validate_root(
        self,
        validate: impl FnOnce(
            &S::Element,
            &[usize],
            u64,
            &ProcessFingerprint,
        ) -> Result<S::Element, ToolResult>,
    ) -> Result<ValidatedScopedCapture<S>, ToolResult> {
        let live_element = validate(&self.element, &self.path, self.window_id, &self.instance)?;
        Ok(ValidatedScopedCapture {
            capture: self,
            live_element,
        })
    }
}

impl<S: SnapshotPayload> ValidatedScopedCapture<S> {
    pub fn root_element(&self) -> &S::Element {
        &self.live_element
    }
    pub fn parent_snapshot_id(&self) -> u32 {
        self.capture.parent_id
    }
    pub fn root_path(&self) -> &[usize] {
        &self.capture.path
    }
}

/// Only a complete walk can supply a payload to the scoped snapshot store.
pub enum ScopedWalk<S> {
    Complete(S),
    Incomplete(Vec<IncompleteReason>),
}

fn stale_parent() -> ToolResult {
    ToolResult::error("The parent snapshot changed before drill-down publication.")
        .with_structured(serde_json::json!({
            "status":"refused", "refusal":{"code":"stale_parent","message":"The parent snapshot changed before drill-down publication."},
            "drilldown_status":"stale_parent"
        }))
}

impl<S: SnapshotPayload> SnapshotStore<S> {
    /// Select a qualified root atomically against its full parent. `live`
    /// must come from a fresh process lookup performed outside the store lock.
    pub fn begin_scoped_capture(
        &self,
        pid: i32,
        window_id: u64,
        root_token: &str,
        session: Option<&str>,
        live: InstanceEvidence,
    ) -> Result<ScopedCapture<S>, ToolResult> {
        let (source_id, index) = parse_token(root_token).ok_or_else(|| {
            refusal(
                "invalid_element_token",
                "Root must be a snapshot-qualified element token.".into(),
            )
        })?;
        let inner = self.inner.lock().unwrap();
        let lane = inner
            .snapshots
            .get(&pid)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let scoped = inner
            .scoped
            .iter()
            .filter(|((owner, _), _)| *owner == pid)
            .flat_map(|(_, entries)| entries.iter())
            .find(|entry| entry.snapshot.id == source_id);
        let (source, parent_id, parent_epoch, mut path) = if let Some(entry) = scoped {
            (
                &entry.snapshot,
                entry.parent_id,
                entry.parent_epoch,
                entry.root_path.clone(),
            )
        } else {
            let source = lane
                .iter()
                .find(|entry| entry.id == source_id)
                .ok_or_else(|| refusal("stale_element_token", "Root snapshot is stale.".into()))?;
            (source, source.id, source.semantic_epoch, Vec::new())
        };
        let parent = lane
            .iter()
            .find(|entry| entry.id == parent_id && entry.semantic_epoch == parent_epoch)
            .ok_or_else(stale_parent)?;
        if !parent.semantic || parent.window_id != window_id || source.window_id != window_id {
            return Err(refusal(
                "conflicting_element_target",
                "Root does not belong to the requested semantic window.".into(),
            ));
        }
        if parent.root_session_retired
            || parent.publisher.as_deref() != session
            || source.publisher.as_deref() != session
            || session.is_some_and(crate::session::is_session_ended)
        {
            return Err(refusal(
                "stale_element_token",
                "Root belongs to another or retired session.".into(),
            ));
        }
        let instance = match (&parent.instance, live) {
            (None, _) => {
                return Err(refusal(
                    "root_requires_protected_snapshot",
                    "Root requires a process-protected snapshot.".into(),
                ))
            }
            (Some(InstanceCapture::Captured(stored)), InstanceEvidence::Live(live))
                if stored.start_time.is_some()
                    && live.start_time.is_some()
                    && stored.matches(&live) =>
            {
                stored.clone()
            }
            _ => {
                return Err(refusal(
                    "stale_element_token",
                    "Root process identity is unavailable or changed.".into(),
                ))
            }
        };
        path.extend(source.payload.structural_path(index).ok_or_else(|| {
            refusal(
                "root_identity_unproven",
                "Root structural path is unavailable.".into(),
            )
        })?);
        let element = source.payload.retain(index).ok_or_else(|| {
            refusal(
                "invalid_element_token",
                "Root index is not in the snapshot.".into(),
            )
        })?;
        Ok(ScopedCapture {
            state: self.inner.clone(),
            pid,
            window_id,
            parent_id,
            parent_epoch,
            source_id,
            session: session.map(str::to_owned),
            instance,
            path,
            element,
        })
    }

    /// Publish only a complete scoped walk without touching full-capture tickets.
    pub fn commit_scoped_capture(
        &self,
        validated: ValidatedScopedCapture<S>,
        walk: ScopedWalk<S>,
    ) -> Result<u32, ToolResult> {
        let capture = validated.capture;
        if !Arc::ptr_eq(&self.inner, &capture.state) {
            return Err(stale_parent());
        }
        let (id, evicted) = {
            let mut inner = self.inner.lock().unwrap();
            let parent = inner
                .snapshots
                .get(&capture.pid)
                .and_then(|lane| {
                    lane.iter().find(|s| {
                        s.id == capture.parent_id
                            && s.semantic_epoch == capture.parent_epoch
                            && s.window_id == capture.window_id
                            && s.publisher == capture.session
                            && s.semantic
                            && !s.root_session_retired
                    })
                })
                .ok_or_else(stale_parent)?;
            if capture
                .session
                .as_deref()
                .is_some_and(crate::session::is_session_ended)
            {
                return Err(stale_parent());
            }
            if capture.source_id != capture.parent_id
                && !inner
                    .scoped
                    .get(&(capture.pid, capture.parent_id))
                    .is_some_and(|entries| {
                        entries
                            .iter()
                            .any(|entry| entry.snapshot.id == capture.source_id)
                    })
            {
                return Err(stale_parent());
            }
            let instance = parent.instance.clone();
            let payload = match walk {
                ScopedWalk::Complete(payload) => payload,
                ScopedWalk::Incomplete(mut reasons) => {
                    if reasons.is_empty() {
                        reasons.push(IncompleteReason {
                            kind: cua_driver_contract::IncompleteReasonKind::Unknown,
                            detail: cua_driver_contract::IncompleteReasonDetail::NotAsserted,
                        });
                    }
                    return Err(ToolResult::error("The subtree observation is incomplete.")
                        .with_structured(serde_json::json!({"status":"refused", "refusal":{"code":"drilldown_incomplete","message":"The subtree observation is incomplete."},
                            "drilldown_status":"incomplete", "observation_complete":false, "drilldown_diagnostics":reasons})));
                }
            };
            let id = crate::element_token::mint_snapshot_id();
            let entry = ScopedEntry {
                parent_id: capture.parent_id,
                parent_epoch: capture.parent_epoch,
                root_path: capture.path,
                snapshot: Snapshot {
                    id,
                    window_id: capture.window_id,
                    publisher: capture.session,
                    root_session_retired: false,
                    instance,
                    screenshot_owner: None,
                    screenshot_scale: None,
                    screenshot_window_size: None,
                    zoom: None,
                    semantic: true,
                    payload,
                    basis: None,
                    frame_generation: 0,
                    basis_exhausted: false,
                    ticket_floor: 0,
                    semantic_epoch: capture.parent_epoch,
                },
            };
            let entries = inner
                .scoped
                .entry((capture.pid, capture.parent_id))
                .or_default();
            let evicted = if entries.len() == SCOPED_CAP_PER_PARENT {
                Some(entries.remove(0))
            } else {
                None
            };
            entries.push(entry);
            (id, evicted)
        };
        drop(evicted);
        Ok(id)
    }
}
