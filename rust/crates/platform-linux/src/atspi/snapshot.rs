//! AT-SPI element cache for Linux.
//!
//! [`AtspiSnapshot`] is the [`SnapshotPayload`] published into the runtime
//! snapshot store (`cua_driver_core::snapshot_store`). An `element_token`
//! resolves through it to a [`CachedElement`]: the element's proven AT-SPI
//! identity ([`AtspiIdentity`]) plus its D-Bus address ([`native::ObjectRef`]),
//! role, and screen frame. Re-walking large trees was the multi-second stall
//! behind every element-index click.
//!
//! Membership follows the runtime cache rule: only nodes with an
//! `element_index` AND a proven identity are addressable. An unproven
//! identity (a well-known bus name whose owner could not be pinned) is
//! discovery-only.

use super::native::{self, ObjectRef};
use super::{AtspiIdentity, AtspiNode};
use cua_driver_core::snapshot_store::{SnapshotPayload, SnapshotStore};
use std::collections::HashMap;
use std::sync::Arc;

/// Identifies a process and window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub pid: u32,
    pub xid: u64,
}

/// One indexed element as the last `get_window_state` snapshot saw it.
#[derive(Debug, Clone)]
pub struct CachedElement {
    /// Opaque AT-SPI path hash (legacy `element_key`).
    pub key: u64,
    /// Proven identity (unique bus owner + owning frame): what an indexed
    /// click acts on; the live ordinal is never used to retarget.
    pub identity: AtspiIdentity,
    /// D-Bus address for the cached fast paths (same object as `identity`).
    pub object_ref: Option<ObjectRef>,
    pub role: String,
    /// The node lives inside an embedded web document (bounds need the
    /// document origin, which only a walk resolves).
    pub in_web_content: bool,
    /// Screen-space frame `(x, y, w, h)` when the snapshot resolved one.
    pub bounds: Option<(i32, i32, u32, u32)>,
}

pub struct AtspiSnapshot {
    elements: Arc<HashMap<usize, CachedElement>>,
}

impl AtspiSnapshot {
    pub fn elements(&self) -> Arc<HashMap<usize, CachedElement>> {
        Arc::clone(&self.elements)
    }

    pub fn from_nodes(nodes: &[AtspiNode]) -> Self {
        Self::from_nodes_with_bounds(nodes, &[])
    }

    /// `bounds` are the screen frames the same walk produced, keyed by
    /// element_index.
    pub fn from_nodes_with_bounds(
        nodes: &[AtspiNode],
        bounds: &[(usize, i32, i32, u32, u32)],
    ) -> Self {
        let mut elements: HashMap<usize, CachedElement> = nodes
            .iter()
            .filter_map(|node| {
                let index = node.element_index?;
                let identity = node.identity.clone()?;
                Some((
                    index,
                    CachedElement {
                        key: node.element_key,
                        identity,
                        object_ref: node.object_ref.clone(),
                        role: node.role.clone(),
                        in_web_content: node.in_web_content,
                        bounds: None,
                    },
                ))
            })
            .collect();
        for &(idx, x, y, w, h) in bounds {
            if let Some(element) = elements.get_mut(&idx) {
                element.bounds = Some((x, y, w, h));
            }
        }
        Self {
            elements: Arc::new(elements),
        }
    }
}

impl SnapshotPayload for AtspiSnapshot {
    type Element = CachedElement;
    fn len(&self) -> usize {
        self.elements.len()
    }
    fn retain(&self, index: usize) -> Option<CachedElement> {
        self.elements.get(&index).cloned()
    }
}

pub type Snapshots = SnapshotStore<AtspiSnapshot>;

/// Build a payload without publishing it.
pub(crate) fn build_snapshot(
    nodes: &[AtspiNode],
    bounds: &[(usize, i32, i32, u32, u32)],
) -> AtspiSnapshot {
    AtspiSnapshot::from_nodes_with_bounds(nodes, bounds)
}

/// Hit-test a *screen* point against the cached frames of (pid, xid): the
/// smallest covering real actuator wins, then a passive label. Returns the
/// element index and its cached identity.
pub(crate) fn hit_test(
    elements: &HashMap<usize, CachedElement>,
    sx: i32,
    sy: i32,
) -> Option<(usize, CachedElement)> {
    let mut indexed: Vec<(&usize, &CachedElement)> = elements.iter().collect();
    indexed.sort_by_key(|(idx, _)| **idx);
    let frames: Vec<(usize, i32, i32, u32, u32, bool)> = indexed
        .iter()
        .enumerate()
        .filter_map(|(pos, (_, e))| {
            let (x, y, w, h) = e.bounds?;
            // The application's own frame is never the control under a
            // point; leave such a hit to the live descent.
            (w > 0 && h > 0 && !crate::at_point_policy::is_top_level_shell_role(&e.role))
                .then_some((pos, x, y, w, h, native::is_passive_role(&e.role)))
        })
        .collect();
    let pos = native::select_click_target(&frames, sx, sy)?;
    let (idx, element) = indexed[pos];
    Some((*idx, element.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::element_token::{token_for, ResolvedElement};

    fn node(index: usize) -> AtspiNode {
        node_with_role(index, "button")
    }

    fn node_with_role(index: usize, role: &str) -> AtspiNode {
        AtspiNode {
            element_index: Some(index),
            role: role.into(),
            name: None,
            value: None,
            checked: None,
            enabled: None,
            selected: None,
            toggle_state: None,
            expanded: None,
            description: None,
            actions: Vec::new(),
            element_key: 999,
            identity: Some(AtspiIdentity {
                bus_name: ":1.1".into(),
                path: format!("/node/{index}"),
                frame_bus_name: ":1.1".into(),
                frame_path: "/frame".into(),
            }),
            depth: 0,
            parent_element_index: None,
            in_web_content: false,
            object_ref: Some(ObjectRef {
                bus: ":1.1".into(),
                path: format!("/node/{index}"),
            }),
        }
    }

    #[test]
    fn side_index_ignores_capture_superseded_by_newer_publication() {
        let (pid, xid) = (424_243_u32, 78);
        let cache = Snapshots::new();
        let older = cache.begin_capture(pid as i32, xid);
        let newer = cache.begin_capture(pid as i32, xid);
        let fresh = build_snapshot(&[node(2)], &[(2, 100, 100, 10, 10)]);
        assert!(cache
            .publish_basis_for_session(pid as i32, xid, fresh, None, None, None, newer)
            .is_some());
        let stale = build_snapshot(&[node(1)], &[(1, 0, 0, 10, 10)]);
        assert!(cache
            .publish_basis_for_session(pid as i32, xid, stale, None, None, None, older)
            .is_none());
        let elements = cache
            .with_latest(pid as i32, xid, |_, s| s.elements())
            .unwrap();
        assert!(hit_test(&elements, 5, 5).is_none());
        assert_eq!(hit_test(&elements, 105, 105).map(|(idx, _)| idx), Some(2));
        assert_eq!(cache.remove_for_capture(pid as i32, xid, older), None);
        assert!(cache
            .with_latest(pid as i32, xid, |_, s| hit_test(&s.elements(), 105, 105))
            .flatten()
            .is_some());
        let ticket = cache.begin_capture(pid as i32, xid);
        assert!(cache
            .publish_capture_basis_for_session(
                pid as i32,
                xid,
                AtspiSnapshot::from_nodes(&[]),
                None,
                None,
                None,
                ticket
            )
            .is_some());
        assert!(cache
            .with_latest(pid as i32, xid, |_, s| hit_test(&s.elements(), 105, 105))
            .flatten()
            .is_none());
    }

    #[test]
    fn sparse_application_indices_are_members_not_dense_offsets_or_native_keys() {
        let cache = Snapshots::new();
        let id = cache.publish(42, 7, AtspiSnapshot::from_nodes(&[node(11), node(7)]));
        for index in [7, 11] {
            let resolved = cache
                .resolve(
                    42,
                    &serde_json::json!({ "element_token": token_for(id, index) }),
                )
                .unwrap();
            assert!(
                matches!(resolved, ResolvedElement::Element { element, .. } if element.identity.path == format!("/node/{index}"))
            );
        }
        for index in [0, 1, 8, 999] {
            assert!(cache
                .resolve(
                    42,
                    &serde_json::json!({ "element_token": token_for(id, index) })
                )
                .is_err());
        }
    }

    #[test]
    fn duplicate_and_unindexed_nodes_do_not_create_members() {
        let mut unindexed = node(8);
        unindexed.element_index = None;
        let payload = AtspiSnapshot::from_nodes(&[node(11), unindexed, node(7), node(11)]);
        assert_eq!(payload.len(), 2);
        assert_eq!(payload.retain(7).unwrap().identity.path, "/node/7");
        assert_eq!(payload.retain(11).unwrap().identity.path, "/node/11");
        assert!(payload.retain(8).is_none());
        assert!(payload.retain(0).is_none());
    }

    #[test]
    fn unproven_identity_is_discovery_only() {
        let mut unproven = node(3);
        unproven.identity = None;
        let payload = AtspiSnapshot::from_nodes(&[unproven, node(4)]);
        assert_eq!(payload.len(), 1);
        assert!(payload.retain(3).is_none());
    }

    #[test]
    fn reordered_live_index_cannot_retarget_an_observed_control() {
        let cache = Snapshots::new();
        let observed = cache.publish(42, 7, AtspiSnapshot::from_nodes(&[node(5)]));
        let mut replacement = node(5);
        replacement.identity.as_mut().unwrap().path = "/node/replacement".into();
        let current = cache.publish(42, 7, AtspiSnapshot::from_nodes(&[replacement]));

        // The former observation is invalidated rather than resolving index 5
        // to the replacement. The current token retains the replacement's own
        // object address for the X11 click resolver to match directly.
        assert!(cache
            .resolve(
                42,
                &serde_json::json!({ "element_token": token_for(observed, 5) })
            )
            .is_err());
        let current = cache
            .resolve(
                42,
                &serde_json::json!({ "element_token": token_for(current, 5) }),
            )
            .unwrap();
        assert!(
            matches!(current, ResolvedElement::Element { element, .. } if element.identity.path == "/node/replacement")
        );
    }

    #[test]
    fn replacement_retires_old_linux_membership() {
        let cache = Snapshots::new();
        let old = cache.publish(42, 7, AtspiSnapshot::from_nodes(&[node(7), node(11)]));
        let fresh = cache.publish(42, 7, AtspiSnapshot::from_nodes(&[node(3)]));
        for index in [7, 11] {
            let refusal = cache
                .resolve(
                    42,
                    &serde_json::json!({ "element_token": token_for(old, index) }),
                )
                .unwrap_err();
            assert_eq!(
                refusal.structured_content.unwrap()["refusal"]["code"],
                "stale_element_token"
            );
            assert!(cache
                .resolve(
                    42,
                    &serde_json::json!({ "element_token": token_for(fresh, index) })
                )
                .is_err());
        }
        let target = cache
            .resolve(
                42,
                &serde_json::json!({ "element_token": token_for(fresh, 3) }),
            )
            .unwrap();
        assert!(matches!(
            target,
            ResolvedElement::Element {
                window_id: 7,
                element,
                ..
            } if element.identity.path == "/node/3"
        ));
    }

    #[test]
    fn empty_linux_snapshot_has_no_element_zero() {
        let cache = Snapshots::new();
        let id = cache.publish(42, 7, AtspiSnapshot::from_nodes(&[]));
        assert!(cache
            .resolve(
                42,
                &serde_json::json!({ "element_token": token_for(id, 0) })
            )
            .is_err());
    }

    #[test]
    fn snapshot_hit_test_prefers_smallest_real_actuator_and_keeps_refs() {
        let nodes = vec![
            node_with_role(0, "panel"),
            node_with_role(1, "push button"),
            node_with_role(2, "label"),
        ];
        let snapshot = build_snapshot(
            &nodes,
            &[
                (0, 0, 0, 500, 500),
                (1, 100, 100, 50, 20),
                (2, 105, 102, 40, 16),
            ],
        );
        assert_eq!(snapshot.retain(1).unwrap().bounds, Some((100, 100, 50, 20)));
        let elements = snapshot.elements();
        let (idx, element) = hit_test(&elements, 110, 110).expect("hit");
        assert_eq!(idx, 1);
        assert_eq!(
            element.object_ref.as_ref().map(|r| r.path.as_str()),
            Some("/node/1")
        );
        assert!(hit_test(&elements, 900, 900).is_none());
    }
}
