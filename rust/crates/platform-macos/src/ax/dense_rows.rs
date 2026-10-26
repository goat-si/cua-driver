//! Dense native collections yield to the rest of the window without changing document order.

use super::bindings::*;
use super::snapshot::RetainedElement;
use super::tree::{set_messaging_timeout, AXNode};
use core_foundation::array::CFArray;
use core_foundation::base::{CFEqual, CFGetTypeID, CFHash, CFTypeRef, TCFType};
use core_foundation::string::CFString;
use cua_driver_core::walk_budget::WalkBudget;
use std::cell::Cell;
use std::collections::HashMap;

thread_local! {
    static ROWS_WINDOWED: Cell<bool> = const { Cell::new(false) };
}

/// Scope dense-row scheduling to model-facing window reads on this thread.
pub fn with_dense_rows_windowed<T>(on: bool, walk: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            ROWS_WINDOWED.with(|flag| flag.set(self.0));
        }
    }
    let _restore = Restore(ROWS_WINDOWED.with(|flag| flag.replace(on)));
    walk()
}

pub(crate) fn rows_windowed() -> bool {
    ROWS_WINDOWED.with(Cell::get)
}

pub(crate) const DENSE_COLLECTION_ROLES: &[&str] = &["AXTable", "AXOutline", "AXList", "AXGrid"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChildPlan {
    WalkNow,
    Defer { visible: bool },
}

pub(crate) fn plan_children(
    child_count: usize,
    is_row: impl Fn(usize) -> bool,
    visible: Option<&dyn Fn(usize) -> bool>,
    expired: impl Fn() -> bool,
) -> Vec<ChildPlan> {
    let mut deadline_expired = false;
    (0..child_count)
        .map(|i| {
            deadline_expired = deadline_expired || expired();
            if deadline_expired || is_row(i) {
                ChildPlan::Defer {
                    visible: visible.is_none_or(|contains| contains(i)),
                }
            } else {
                ChildPlan::WalkNow
            }
        })
        .collect()
}

// AX returns distinct proxy pointers for the same row. Hash by CF identity,
// then resolve collisions with CFEqual, rather than comparing every pair.
struct ElementSet {
    _array: CFArray<CFTypeRef>,
    buckets: HashMap<usize, Vec<CFTypeRef>>,
}

impl ElementSet {
    unsafe fn copy(element: AXUIElementRef, attribute: &str) -> Option<Self> {
        let attribute = CFString::new(attribute);
        let mut value = std::ptr::null();
        if AXUIElementCopyAttributeValue(element, attribute.as_concrete_TypeRef(), &mut value)
            != kAXErrorSuccess
            || value.is_null()
        {
            return None;
        }
        if CFGetTypeID(value) != CFArray::<CFTypeRef>::type_id() {
            core_foundation::base::CFRelease(value);
            return None;
        }
        let array = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
        let mut buckets: HashMap<usize, Vec<CFTypeRef>> = HashMap::new();
        for item in array.iter() {
            if CFGetTypeID(*item) == AXUIElementGetTypeID() {
                buckets.entry(CFHash(*item)).or_default().push(*item);
            }
        }
        (!buckets.is_empty()).then_some(Self {
            _array: array,
            buckets,
        })
    }

    unsafe fn contains(&self, element: AXUIElementRef) -> bool {
        let element = element as CFTypeRef;
        self.buckets
            .get(&CFHash(element))
            .is_some_and(|bucket| bucket.iter().any(|&other| CFEqual(element, other) != 0))
    }
}

pub(crate) unsafe fn plan_collection_children(
    element: AXUIElementRef,
    role: &str,
    children: &[AXUIElementRef],
    budget: &WalkBudget,
) -> Vec<ChildPlan> {
    let rows = ElementSet::copy(element, "AXRows");
    let visible = ElementSet::copy(element, "AXVisibleRows")
        .or_else(|| ElementSet::copy(element, "AXVisibleChildren"));
    let is_visible = |i| visible.as_ref().is_none_or(|set| set.contains(children[i]));
    plan_children(
        children.len(),
        |i| {
            if let Some(rows) = &rows {
                return rows.contains(children[i]);
            }
            set_messaging_timeout(children[i]);
            let child_role = copy_string_attr(children[i], "AXRole").unwrap_or_default();
            child_role == "AXRow"
                || (matches!(role, "AXList" | "AXGrid")
                    && !matches!(child_role.as_str(), "AXScrollBar" | "AXColumn" | "AXHeader"))
        },
        visible
            .as_ref()
            .map(|_| &is_visible as &dyn Fn(usize) -> bool),
        || rows.is_none() && budget.expired(),
    )
}

pub(crate) struct Chunk {
    pub lines: Vec<(usize, String)>,
    pub nodes: Vec<AXNode>,
}

pub(crate) struct DeferredRow {
    pub anchor: usize,
    pub seq: usize,
    pub element: RetainedElement,
    pub depth: usize,
    pub parent_index: Option<usize>,
    pub in_web_content: bool,
    pub visible: bool,
    /// None means intentionally skipped (offscreen) or refused (visible).
    pub walked: Option<Chunk>,
}

pub(crate) fn is_summary_line(line: &str) -> bool {
    line.trim_start().starts_with("- (offscreen rows: ")
}

type Spliced = (Vec<(usize, String)>, Vec<AXNode>, Vec<Option<usize>>);

pub(crate) fn splice(
    lines: Vec<(usize, String)>,
    nodes: Vec<AXNode>,
    mut deferred: Vec<DeferredRow>,
) -> Spliced {
    debug_assert_eq!(lines.len(), nodes.len());
    deferred.sort_by_key(|row| (row.anchor, row.seq));
    let mut rows = deferred.into_iter().peekable();
    let mut output = Chunk {
        lines: Vec::new(),
        nodes: Vec::new(),
    };
    let mut map = Vec::new();
    let mut original = lines.into_iter().zip(nodes);
    for anchor in 0..=original.len() {
        while rows.peek().is_some_and(|row| row.anchor == anchor) {
            let mut row = rows.next().unwrap();
            if let Some(chunk) = row.walked.take() {
                debug_assert_eq!(chunk.lines.len(), chunk.nodes.len());
                map.extend((0..chunk.nodes.len()).map(|i| Some(output.nodes.len() + i)));
                output.lines.extend(chunk.lines);
                output.nodes.extend(chunk.nodes);
            } else if !row.visible {
                let mut count = 1;
                while rows.peek().is_some_and(|next| {
                    next.anchor == anchor
                        && next.depth == row.depth
                        && next.parent_index == row.parent_index
                        && !next.visible
                        && next.walked.is_none()
                }) {
                    rows.next();
                    count += 1;
                }
                output.lines.push((
                    row.depth,
                    format!(
                    "- (offscreen rows: {count}; scroll the list, or pass query to search them)"
                ),
                ));
                map.push(None);
            }
        }
        if let Some((line, node)) = original.next() {
            map.push(Some(output.nodes.len()));
            output.lines.push(line);
            output.nodes.push(node);
        }
    }
    (output.lines, output.nodes, map)
}

pub(crate) fn renumber(lines: &mut [(usize, String)], nodes: &mut [AXNode], map: &[Option<usize>]) {
    let indices: HashMap<usize, usize> = nodes
        .iter()
        .filter_map(|node| node.element_index)
        .enumerate()
        .map(|(new, old)| (old, new))
        .collect();
    for node in nodes.iter_mut() {
        node.element_index = node.element_index.map(|old| indices[&old]);
        node.parent_element_index = node.parent_element_index.map(|old| indices[&old]);
    }
    for ((_, line), node_index) in lines.iter_mut().zip(map) {
        if let Some(new) = node_index.and_then(|i| nodes[i].element_index) {
            if let Some((_, tail)) = line.strip_prefix("- [").and_then(|s| s.split_once("] ")) {
                *line = format!("- [{new}] {tail}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::AxSnapshot;
    use super::*;
    use core_foundation::base::{CFGetRetainCount, CFRetain};
    use cua_driver_core::protocol::Content;
    use cua_driver_core::snapshot_store::SnapshotPayload;
    use cua_driver_core::window_state_view::{apply, ViewContext, ViewKey, ViewOptions};
    use serde_json::json;

    fn node(
        index: Option<usize>,
        role: &str,
        title: &str,
        depth: usize,
        parent: Option<usize>,
    ) -> AXNode {
        AXNode {
            element_index: index,
            role: role.into(),
            title: Some(title.into()),
            value: None,
            description: None,
            identifier: None,
            help: None,
            actions: vec![],
            element_ptr: 0,
            depth,
            parent_element_index: parent,
            frame: None,
            value_state: None,
            value_description: None,
            min_value: None,
            max_value: None,
            enabled: None,
            selected: None,
            in_web_content: false,
        }
    }

    fn chunk(nodes: Vec<AXNode>) -> Chunk {
        let lines = nodes
            .iter()
            .map(|n| {
                let index = n
                    .element_index
                    .map(|i| format!("[{i}] "))
                    .unwrap_or_default();
                (
                    n.depth,
                    format!("- {index}{} \"{}\"", n.role, n.title.as_deref().unwrap()),
                )
            })
            .collect();
        Chunk { lines, nodes }
    }

    fn row(anchor: usize, seq: usize, visible: bool, walked: Option<Chunk>) -> DeferredRow {
        DeferredRow {
            anchor,
            seq,
            element: unsafe { RetainedElement::retain(0) },
            depth: 2,
            parent_index: Some(1),
            in_web_content: false,
            visible,
            walked,
        }
    }

    fn window() -> Chunk {
        chunk(vec![
            node(Some(0), "AXWindow", "Example", 0, None),
            node(Some(1), "AXTable", "Messages", 1, Some(0)),
            node(Some(2), "AXTextArea", "Compose", 1, Some(0)),
        ])
    }

    fn message() -> Chunk {
        chunk(vec![
            node(Some(3), "AXRow", "Message", 2, Some(1)),
            node(None, "AXStaticText", "Body", 3, Some(3)),
            node(Some(4), "AXButton", "Reply", 3, Some(3)),
        ])
    }

    fn markdown(lines: &[(usize, String)]) -> String {
        lines
            .iter()
            .map(|(depth, line)| format!("{}{line}\n", "  ".repeat(*depth)))
            .collect()
    }

    #[test]
    fn rows_yield_to_other_children_and_missing_visibility_keeps_all_rows() {
        let is_row = |i| matches!(i, 0 | 2 | 3);
        assert_eq!(
            plan_children(4, is_row, Some(&|i| i == 2), || false),
            vec![
                ChildPlan::Defer { visible: false },
                ChildPlan::WalkNow,
                ChildPlan::Defer { visible: true },
                ChildPlan::Defer { visible: false },
            ]
        );
        assert_eq!(
            plan_children(4, is_row, None, || false),
            vec![
                ChildPlan::Defer { visible: true },
                ChildPlan::WalkNow,
                ChildPlan::Defer { visible: true },
                ChildPlan::Defer { visible: true },
            ]
        );
    }

    #[test]
    fn expired_classification_stops_probing_and_defers_every_remaining_child() {
        let is_visible = |i: usize| i.is_multiple_of(2);
        let shown = ChildPlan::Defer { visible: true };
        let hidden = ChildPlan::Defer { visible: false };
        for (probe_limit, expected) in [
            (0, [shown, hidden, shown, hidden]),
            (2, [shown, ChildPlan::WalkNow, shown, hidden]),
            (4, [shown, ChildPlan::WalkNow, ChildPlan::WalkNow, hidden]),
        ] {
            let probes = Cell::new(0);
            let plan = plan_children(
                4,
                |i| {
                    probes.set(probes.get() + 1);
                    matches!(i, 0 | 3)
                },
                Some(&is_visible),
                || probes.get() >= probe_limit,
            );
            assert_eq!(probes.get(), probe_limit);
            assert_eq!(plan, expected);
        }
        assert_eq!(
            plan_children(
                2,
                |_| panic!("expired children must not be probed"),
                None,
                || true
            ),
            [shown, shown]
        );
    }

    #[test]
    fn deferred_collections_keep_document_order_and_summarize_each_skipped_run() {
        let base = window();
        let second = chunk(vec![node(
            Some(5),
            "AXRow",
            "Second collection",
            2,
            Some(1),
        )]);
        let deferred = vec![
            row(2, 7, true, Some(second)),
            row(2, 0, false, None),
            row(2, 1, false, None),
            row(2, 2, true, Some(message())),
            row(2, 3, true, None),
            row(2, 4, false, None),
            row(2, 5, false, None),
            row(2, 6, false, None),
            row(
                3,
                8,
                true,
                Some(chunk(vec![node(None, "AXRow", "Last", 2, Some(1))])),
            ),
        ];
        let (lines, nodes, map) = splice(base.lines, base.nodes, deferred);
        let titles: Vec<_> = nodes.iter().map(|n| n.title.as_deref().unwrap()).collect();
        assert_eq!(
            titles,
            [
                "Example",
                "Messages",
                "Message",
                "Body",
                "Reply",
                "Second collection",
                "Compose",
                "Last"
            ]
        );
        assert_eq!(
            lines
                .iter()
                .filter(|(_, l)| is_summary_line(l))
                .collect::<Vec<_>>(),
            vec![
                &(
                    2,
                    "- (offscreen rows: 2; scroll the list, or pass query to search them)".into()
                ),
                &(
                    2,
                    "- (offscreen rows: 3; scroll the list, or pass query to search them)".into()
                ),
            ]
        );
        assert_eq!(
            map,
            [
                Some(0),
                Some(1),
                None,
                Some(2),
                Some(3),
                Some(4),
                None,
                Some(5),
                Some(6),
                Some(7)
            ]
        );
    }

    #[test]
    fn spliced_indices_resolve_by_snapshot_position_and_remap_parents() {
        let base = window();
        let (mut lines, mut nodes, map) = splice(
            base.lines,
            base.nodes,
            vec![row(2, 0, true, Some(message()))],
        );
        renumber(&mut lines, &mut nodes, &map);
        assert_eq!(
            nodes.iter().map(|n| n.element_index).collect::<Vec<_>>(),
            [Some(0), Some(1), Some(2), None, Some(3), Some(4)]
        );
        assert_eq!(
            nodes
                .iter()
                .map(|n| n.parent_element_index)
                .collect::<Vec<_>>(),
            [None, Some(0), Some(1), Some(2), Some(2), Some(0)]
        );
        let snapshot = AxSnapshot::from_nodes(&nodes);
        for (position, n) in nodes
            .iter()
            .filter(|n| n.element_index.is_some())
            .enumerate()
        {
            assert_eq!(n.element_index, Some(position));
            assert_eq!(snapshot.retain(position).unwrap().as_ptr(), n.element_ptr);
            assert!(lines.iter().any(|(_, l)| l
                == &format!(
                    "- [{position}] {} \"{}\"",
                    n.role,
                    n.title.as_deref().unwrap()
                )));
        }
    }

    #[test]
    fn since_reindex_ranges_ascend_when_deferred_rows_precede_a_later_control() {
        let make = |extra: bool| {
            let base = window();
            let mut deferred_chunk = message();
            if extra {
                deferred_chunk
                    .nodes
                    .insert(0, node(Some(5), "AXRow", "Earlier", 2, Some(1)));
                deferred_chunk = chunk(deferred_chunk.nodes);
            }
            let (mut lines, mut nodes, map) = splice(
                base.lines,
                base.nodes,
                vec![row(2, 0, true, Some(deferred_chunk))],
            );
            renumber(&mut lines, &mut nodes, &map);
            markdown(&lines)
        };
        let ctx = ViewContext {
            pid: 260026,
            window_id: 260026,
            key: ViewKey {
                query: None,
                max_elements: 250,
                max_depth: None,
            },
            focus_probe: None,
        };
        let mut before = json!({"snapshot_id":"s00de0001", "tree_markdown":make(false)});
        let mut content = vec![Content::text(before["tree_markdown"].as_str().unwrap())];
        apply(
            &ViewOptions::from_args(&json!({})).unwrap(),
            &ctx,
            &mut content,
            &mut before,
        );
        let mut after = json!({"snapshot_id":"s00de0002", "tree_markdown":make(true)});
        let mut content = vec![Content::text(after["tree_markdown"].as_str().unwrap())];
        apply(
            &ViewOptions::from_args(&json!({"since":"s00de0001"})).unwrap(),
            &ctx,
            &mut content,
            &mut after,
        );
        assert_eq!(after["since_status"], "diff");
        assert!(
            after["tree_diff"]
                .as_str()
                .unwrap()
                .contains("reindexed: [2-4] -> [3-5]"),
            "{after}"
        );
        // Count-only summaries retain their parse_rows identity across reads.
        let old = "- AXTable \"History\"\n  - AXRow \"First unchanged message\"\n  - AXRow \"Second unchanged message\"\n  - AXRow \"Third unchanged message\"\n  - (offscreen rows: 8; scroll the list, or pass query to search them)\n";
        let new = old.replace("rows: 8;", "rows: 9;");
        let mut before = json!({"snapshot_id":"s00de0003", "tree_markdown":old});
        apply(
            &ViewOptions::from_args(&json!({})).unwrap(),
            &ctx,
            &mut content,
            &mut before,
        );
        let mut after = json!({"snapshot_id":"s00de0004", "tree_markdown":new});
        apply(
            &ViewOptions::from_args(&json!({"since":"s00de0003"})).unwrap(),
            &ctx,
            &mut content,
            &mut after,
        );
        assert!(
            after["tree_diff"]
                .as_str()
                .unwrap()
                .contains("0 added, 1 changed, 0 removed"),
            "{after}"
        );
    }

    #[test]
    fn nested_window_reads_restore_the_previous_row_scheduling_mode() {
        assert!(!rows_windowed());
        with_dense_rows_windowed(true, || {
            assert!(rows_windowed());
            with_dense_rows_windowed(false, || assert!(!rows_windowed()));
            assert!(rows_windowed());
        });
        assert!(!rows_windowed());
    }

    #[test]
    fn snapshot_targets_and_retains_survive_splicing_walked_skipped_and_refused_rows() {
        let values: Vec<_> = (0..7)
            .map(|i| CFString::new(&format!("dense-snapshot-owned-element-{i}")))
            .collect();
        let ptrs: Vec<_> = values
            .iter()
            .map(|v| v.as_concrete_TypeRef() as usize)
            .collect();
        let counts: Vec<_> = ptrs
            .iter()
            .map(|&p| unsafe { CFGetRetainCount(p as CFTypeRef) })
            .collect();
        let mut base = window();
        let mut message = chunk(vec![
            node(Some(3), "AXRow", "Message", 2, Some(1)),
            node(Some(4), "AXButton", "Reply", 3, Some(3)),
        ]);
        for (n, &p) in base
            .nodes
            .iter_mut()
            .chain(message.nodes.iter_mut())
            .zip(&ptrs)
        {
            unsafe {
                CFRetain(p as CFTypeRef);
            }
            n.element_ptr = p;
        }
        let mut walked = row(2, 0, true, Some(message));
        walked.element = unsafe { RetainedElement::retain(ptrs[3]) };
        let mut skipped = row(2, 1, false, None);
        skipped.element = unsafe { RetainedElement::retain(ptrs[5]) };
        let mut refused = row(2, 2, true, None);
        refused.element = unsafe { RetainedElement::retain(ptrs[6]) };
        let (mut lines, mut nodes, map) =
            splice(base.lines, base.nodes, vec![walked, skipped, refused]);
        renumber(&mut lines, &mut nodes, &map);
        let snapshot = AxSnapshot::from_nodes(&nodes);
        let expected = [
            (0, None),
            (1, Some(0)),
            (3, Some(1)),
            (4, Some(2)),
            (2, Some(0)),
        ];
        for (index, (original, parent)) in expected.into_iter().enumerate() {
            let guard = snapshot.retain(index).unwrap();
            assert_eq!(guard.as_ptr(), ptrs[original]);
            assert_eq!(nodes[index].parent_element_index, parent);
            assert!(lines
                .iter()
                .any(|(_, l)| l.starts_with(&format!("- [{index}] "))));
            assert_eq!(
                unsafe { CFGetRetainCount(ptrs[original] as CFTypeRef) },
                counts[original] + 2
            );
        }
        assert!(snapshot.retain(5).is_none());
        for i in 0..7 {
            assert_eq!(
                unsafe { CFGetRetainCount(ptrs[i] as CFTypeRef) },
                counts[i] + if i < 5 { 1 } else { 0 }
            );
        }
        drop(snapshot);
        for i in 0..7 {
            assert_eq!(unsafe { CFGetRetainCount(ptrs[i] as CFTypeRef) }, counts[i]);
        }
    }
}
