//! Dense native collections yield to the rest of the window without changing document order.

use super::snapshot::RetainedElement;
use super::tree::{AXNode, LabelPath};
use super::tree_source::{AxTreeSource, Read};
use cua_driver_core::walk_budget::WalkBudget;
use std::cell::Cell;
use std::collections::HashMap;

thread_local! {
    static ROWS_WINDOWED: Cell<bool> = const { Cell::new(false) };
    static VISIBLE_CLIP: Cell<ClipSource> = const { Cell::new(ClipSource::Unset) };
}

/// A container with at least this many children is windowed like a dense
/// collection: its offscreen children are left unwalked.
pub(crate) const MANY_CHILDREN: usize = 50;

/// Whether a read plans this container's children by visibility: a dense
/// collection, or any container with [`MANY_CHILDREN`] children or more, in a
/// windowed (model-facing) read outside web content.
pub(crate) fn windows_children(
    windowed: bool,
    in_web_content: bool,
    role: &str,
    child_count: usize,
) -> bool {
    windowed
        && !in_web_content
        && (DENSE_COLLECTION_ROLES.contains(&role) || child_count >= MANY_CHILDREN)
}

/// Where the frame of the top-level surface being walked comes from. It is
/// read only when a container first needs it.
#[derive(Clone, Copy)]
enum ClipSource {
    Unset,
    Surface(usize),
    Known(Option<[f64; 4]>),
}

/// Scope the top-level surface being walked: a child wholly outside its frame
/// is offscreen when the app does not list its visible children. The frame is
/// read on first use, so a surface without a large container pays nothing.
pub(crate) fn with_visible_clip<T>(surface: Option<usize>, walk: impl FnOnce() -> T) -> T {
    struct Restore(ClipSource);
    impl Drop for Restore {
        fn drop(&mut self) {
            VISIBLE_CLIP.with(|cell| cell.set(self.0));
        }
    }
    let source = surface.map_or(ClipSource::Unset, ClipSource::Surface);
    let _restore = Restore(VISIBLE_CLIP.with(|cell| cell.replace(source)));
    walk()
}

/// The walked surface's frame, read once per surface; `None` when its read
/// would start, or ended, past the deadline (nothing is cached then).
fn visible_clip<S: AxTreeSource>(
    source: &S,
    surface: &S::Element,
    budget: &WalkBudget,
) -> Option<Option<[f64; 4]>> {
    match VISIBLE_CLIP.with(Cell::get) {
        ClipSource::Unset => Some(None),
        ClipSource::Known(clip) => Some(clip),
        ClipSource::Surface(element) => {
            debug_assert_eq!(element, source.key(surface));
            source.set_messaging_timeout(surface);
            let clip = super::control_attrs::frame_batched_with(source, surface, budget)?;
            VISIBLE_CLIP.with(|cell| cell.set(ClipSource::Known(clip)));
            Some(clip)
        }
    }
}

/// Whether a child at `rect` can be on screen inside `clip`. Unknown or
/// non-finite geometry counts as visible, so it is walked rather than dropped.
pub(crate) fn rect_visible(rect: Option<[f64; 4]>, clip: Option<[f64; 4]>) -> bool {
    let (Some([x, y, w, h]), Some([cx, cy, cw, ch])) = (rect, clip) else {
        return true;
    };
    if ![x, y, w, h, cx, cy, cw, ch].iter().all(|v| v.is_finite()) {
        return true;
    }
    x < cx + cw && cx < x + w && y < cy + ch && cy < y + h
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
            if deadline_expired {
                // Unclassified: it may not be a row at all, so it is not
                // summarized as offscreen. The walker refuses it after the
                // deadline and counts it as pending.
                ChildPlan::Defer { visible: true }
            } else if is_row(i) {
                ChildPlan::Defer {
                    visible: visible.is_none_or(|contains| contains(i)),
                }
            } else {
                ChildPlan::WalkNow
            }
        })
        .collect()
}

fn known<T>(value: Read<T>) -> Option<T> {
    match value {
        Read::Known(value) => Some(value),
        Read::Absent | Read::Unknown(_) => None,
    }
}

pub(crate) fn plan_collection_children<S: AxTreeSource>(
    source: &S,
    element: &S::Element,
    surface: &S::Element,
    role: &str,
    children: &[S::Element],
    budget: &WalkBudget,
) -> Vec<ChildPlan> {
    plan_collection_children_observed(source, element, surface, role, children, budget).legacy
}

pub(crate) fn plan_collection_children_observed<S: AxTreeSource>(
    source: &S,
    element: &S::Element,
    surface: &S::Element,
    role: &str,
    children: &[S::Element],
    budget: &WalkBudget,
) -> super::evidence::Observed<Vec<ChildPlan>> {
    super::evidence::Observed::legacy_only(plan_collection_children_legacy(
        source, element, surface, role, children, budget,
    ))
}

fn plan_collection_children_legacy<S: AxTreeSource>(
    source: &S,
    element: &S::Element,
    surface: &S::Element,
    role: &str,
    children: &[S::Element],
    budget: &WalkBudget,
) -> Vec<ChildPlan> {
    let dense = DENSE_COLLECTION_ROLES.contains(&role);
    // Every read below asks the app, so each one first checks the deadline;
    // a child left unclassified is deferred as visible and refused by the
    // walker, which counts it as pending.
    let rows = if dense && !budget.expired() {
        known(source.element_set(element, "AXRows"))
    } else {
        None
    };
    let visible = if dense && !budget.expired() {
        known(source.element_set(element, "AXVisibleRows")).or_else(|| {
            (!budget.expired())
                .then(|| known(source.element_set(element, "AXVisibleChildren")))
                .flatten()
        })
    } else {
        None
    };
    // Without a visible-children list (absent or empty), a parent with many
    // children is cut by each child's frame against the walked surface. A
    // small collection keeps every row, as before. A child whose frame or
    // the surface's frame is read past the deadline stays unclassified
    // (visible), so the walker refuses it as pending rather than calling it
    // offscreen.
    let cut_by_frame = children.len() >= MANY_CHILDREN;
    let is_visible = |i: usize| match &visible {
        Some(set) => source.set_contains(set, &children[i]),
        None if !cut_by_frame => true,
        None => {
            source.set_messaging_timeout(&children[i]);
            let Some(rect) = super::control_attrs::frame_batched_with(source, &children[i], budget)
            else {
                return true;
            };
            let Some(clip) = visible_clip(source, surface, budget) else {
                return true;
            };
            rect_visible(rect, clip)
        }
    };
    plan_children(
        children.len(),
        |i| {
            if !dense {
                // Every child of a large generic container is deferrable.
                return true;
            }
            if let Some(rows) = &rows {
                return source.set_contains(rows, &children[i]);
            }
            source.set_messaging_timeout(&children[i]);
            let child_role = known(source.text(&children[i], "AXRole")).unwrap_or_default();
            child_role == "AXRow"
                || (matches!(role, "AXList" | "AXGrid")
                    && !matches!(child_role.as_str(), "AXScrollBar" | "AXColumn" | "AXHeader"))
        },
        Some(&is_visible as &dyn Fn(usize) -> bool),
        // Classification that asks the app (a role, or a frame when no
        // visible-row list answers) stops at the walk's deadline.
        || (rows.is_none() || visible.is_none()) && budget.expired(),
    )
}

pub(crate) struct Chunk {
    pub lines: Vec<(usize, String)>,
    pub nodes: Vec<AXNode>,
}

pub(crate) struct DeferredRow<H = RetainedElement> {
    pub anchor: usize,
    pub seq: usize,
    /// The row's position among its collection's `AXChildren`.
    pub position: usize,
    /// Which collection the row belongs to (its container element).
    pub collection: usize,
    pub element: H,
    pub depth: usize,
    pub parent_index: Option<usize>,
    pub parent_role: String,
    pub label_path: LabelPath,
    pub in_web_content: bool,
    pub visible: bool,
    /// None means intentionally skipped (offscreen) or refused (visible).
    pub walked: Option<Chunk>,
}

pub(crate) fn is_summary_line(line: &str) -> bool {
    cua_driver_core::snapshot_compaction::is_compaction_summary_line(line)
}

type Spliced = (Vec<(usize, String)>, Vec<AXNode>, Vec<Option<usize>>);

/// One run of consecutive offscreen rows a windowed read left unwalked: the
/// range named by its summary line, by position among the collection's
/// `AXChildren`. `parent_index` is the collection's nearest indexed ancestor,
/// in the indices the walk returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OffscreenRows {
    pub parent_index: Option<usize>,
    pub first_position: usize,
    pub last_position: usize,
    pub count: usize,
}

#[cfg(test)]
pub(crate) fn splice<H>(
    lines: Vec<(usize, String)>,
    nodes: Vec<AXNode>,
    deferred: Vec<DeferredRow<H>>,
) -> Spliced {
    splice_with_offscreen_rows(lines, nodes, deferred).0
}

/// [`splice`], with the offscreen runs its summary lines stand for.
pub(crate) fn splice_with_offscreen_rows<H>(
    lines: Vec<(usize, String)>,
    nodes: Vec<AXNode>,
    mut deferred: Vec<DeferredRow<H>>,
) -> (Spliced, Vec<OffscreenRows>) {
    debug_assert_eq!(lines.len(), nodes.len());
    deferred.sort_by_key(|row| (row.anchor, row.seq));
    let mut rows = deferred.into_iter().peekable();
    let mut output = Chunk {
        lines: Vec::new(),
        nodes: Vec::new(),
    };
    let mut map = Vec::new();
    let mut offscreen = Vec::new();
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
                let mut last_position = row.position;
                // A run is consecutive children of one collection: a child
                // between them that was walked, or another collection, ends it.
                while rows.peek().is_some_and(|next| {
                    next.anchor == anchor
                        && next.depth == row.depth
                        && next.parent_index == row.parent_index
                        && next.collection == row.collection
                        && next.position == last_position + 1
                        && !next.visible
                        && next.walked.is_none()
                }) {
                    last_position = rows.next().unwrap().position;
                    count += 1;
                }
                offscreen.push(OffscreenRows {
                    parent_index: row.parent_index,
                    first_position: row.position,
                    last_position,
                    count,
                });
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
    ((output.lines, output.nodes, map), offscreen)
}

/// Walk the deferred rows a windowed read planned: visible rows first, then
/// offscreen rows only when `search_offscreen`. Returns whether an offscreen row
/// was left unwalked.
pub(crate) fn walk_deferred<H>(
    deferred: &mut [DeferredRow<H>],
    search_offscreen: bool,
    mut walk: impl FnMut(&DeferredRow<H>) -> Chunk,
) -> bool {
    let mut skipped = false;
    for visible in [true, false] {
        if !visible && !search_offscreen {
            skipped = deferred.iter().any(|row| !row.visible);
            continue;
        }
        for row in deferred.iter_mut().filter(|row| row.visible == visible) {
            let chunk = walk(row);
            row.walked = Some(chunk);
        }
    }
    skipped
}

/// Splice the walked rows into document order, renumber every row, and name
/// each offscreen run under its collection's renumbered parent.
pub(crate) fn splice_and_renumber<H>(
    lines: Vec<(usize, String)>,
    nodes: Vec<AXNode>,
    deferred: Vec<DeferredRow<H>>,
) -> (Vec<(usize, String)>, Vec<AXNode>, Vec<OffscreenRows>) {
    let ((mut lines, mut nodes, map), runs) = splice_with_offscreen_rows(lines, nodes, deferred);
    let indices = renumber(&mut lines, &mut nodes, &map);
    let runs = runs
        .into_iter()
        .map(|run| OffscreenRows {
            parent_index: run.parent_index.map(|old| indices[&old]),
            ..run
        })
        .collect();
    (lines, nodes, runs)
}

/// Renumber spliced rows in document order. Returns the old-to-new element
/// index map.
pub(crate) fn renumber(
    lines: &mut [(usize, String)],
    nodes: &mut [AXNode],
    map: &[Option<usize>],
) -> HashMap<usize, usize> {
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
    indices
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::AxSnapshot;
    use super::*;
    use core_foundation::base::{CFGetRetainCount, CFRetain, CFTypeRef, TCFType};
    use core_foundation::string::CFString;
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
            owned_row_label: None,
            canonical: Default::default(),
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
            position: seq,
            collection: 1,
            element: unsafe { RetainedElement::retain(0) },
            depth: 2,
            parent_index: Some(1),
            parent_role: "AXTable".into(),
            label_path: LabelPath::Inactive,
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

    /// Plan a collection's children as the windowed walker does, walk the
    /// deferred rows through [`walk_deferred`] with a recording walker, and
    /// splice the result. Returns the walked positions and the spliced read.
    #[allow(clippy::type_complexity)]
    fn windowed_read(
        child_count: usize,
        is_row: impl Fn(usize) -> bool,
        shown: impl Fn(usize) -> bool,
        collection_of: impl Fn(usize) -> usize,
        base: Chunk,
        anchor: usize,
        parent: Option<usize>,
    ) -> (
        Vec<usize>,
        Vec<(usize, String)>,
        Vec<AXNode>,
        Vec<OffscreenRows>,
    ) {
        let plan = plan_children(child_count, is_row, Some(&shown), || false);
        let mut deferred: Vec<DeferredRow> = plan
            .iter()
            .enumerate()
            .filter_map(|(position, plan)| match *plan {
                ChildPlan::Defer { visible } => Some(DeferredRow {
                    anchor,
                    seq: position,
                    position,
                    collection: collection_of(position),
                    element: unsafe { RetainedElement::retain(0) },
                    depth: 2,
                    parent_index: parent,
                    parent_role: "AXTable".into(),
                    label_path: LabelPath::Inactive,
                    in_web_content: false,
                    visible,
                    walked: None,
                }),
                ChildPlan::WalkNow => None,
            })
            .collect();
        let mut walked = Vec::new();
        let mut next_index = 100;
        let skipped = walk_deferred(&mut deferred, false, |row| {
            walked.push(row.position);
            next_index += 1;
            chunk(vec![node(
                Some(next_index),
                "AXRow",
                &format!("Row {}", row.position),
                2,
                parent,
            )])
        });
        assert_eq!(skipped, deferred.iter().any(|row| !row.visible));
        let (lines, nodes, runs) = splice_and_renumber(base.lines, base.nodes, deferred);
        (walked, lines, nodes, runs)
    }

    #[test]
    fn a_large_list_walks_and_returns_only_its_visible_rows() {
        let (walked, lines, nodes, runs) = windowed_read(
            200,
            |_| true,
            |i| (50..60).contains(&i),
            |_| 7,
            window(),
            2,
            Some(1),
        );
        assert_eq!(
            walked,
            (50..60).collect::<Vec<_>>(),
            "offscreen rows are never walked"
        );
        let rows: Vec<_> = nodes
            .iter()
            .filter(|n| n.role == "AXRow")
            .map(|n| n.title.clone().unwrap())
            .collect();
        assert_eq!(
            rows,
            (50..60).map(|i| format!("Row {i}")).collect::<Vec<_>>()
        );
        assert_eq!(nodes.len(), 3 + 10);
        let indices: Vec<_> = nodes.iter().filter_map(|n| n.element_index).collect();
        assert_eq!(
            indices,
            (0..13).collect::<Vec<_>>(),
            "renumbered in document order"
        );
        assert_eq!(lines.iter().filter(|(_, l)| is_summary_line(l)).count(), 2);
        let run = |first, last| OffscreenRows {
            parent_index: Some(1),
            first_position: first,
            last_position: last,
            count: last - first + 1,
        };
        assert_eq!(runs, [run(0, 49), run(60, 199)]);
    }

    #[test]
    fn offscreen_runs_stop_at_a_walked_child_and_at_another_collection() {
        // Position 2 is not a row: it is walked in place and ends the run.
        // Positions 6.. belong to a second collection under the same parent.
        let (walked, _, _, runs) = windowed_read(
            9,
            |i| i != 2,
            |_| false,
            |i| if i < 6 { 7 } else { 8 },
            window(),
            2,
            Some(1),
        );
        assert!(walked.is_empty());
        let run = |first, last| OffscreenRows {
            parent_index: Some(1),
            first_position: first,
            last_position: last,
            count: last - first + 1,
        };
        assert_eq!(runs, [run(0, 1), run(3, 5), run(6, 8)]);
    }

    #[test]
    fn an_offscreen_run_names_its_parent_by_the_renumbered_index() {
        // A visible row of an earlier collection is spliced in before the
        // table, so the table moves from index 1 to index 2.
        let mut deferred = vec![
            DeferredRow {
                parent_index: Some(0),
                depth: 1,
                collection: 3,
                ..row(
                    1,
                    0,
                    true,
                    Some(chunk(vec![node(Some(9), "AXRow", "Early", 1, Some(0))])),
                )
            },
            DeferredRow {
                collection: 4,
                ..row(2, 1, false, None)
            },
        ];
        deferred[1].position = 0;
        let (_, nodes, runs) = splice_and_renumber(window().lines, window().nodes, deferred);
        let table = nodes.iter().find(|n| n.role == "AXTable").unwrap();
        assert_eq!(table.element_index, Some(2));
        assert_eq!(runs[0].parent_index, Some(2));
    }

    #[test]
    fn dense_roles_are_scheduled_and_many_children_are_cut_by_frame() {
        assert!(windows_children(true, false, "AXTable", 3));
        assert!(windows_children(true, false, "AXGroup", MANY_CHILDREN));
        assert!(!windows_children(true, false, "AXGroup", MANY_CHILDREN - 1));
        assert!(
            !windows_children(false, false, "AXTable", 500),
            "full reads walk everything"
        );
        assert!(
            !windows_children(true, true, "AXTable", 500),
            "web content keeps its own path"
        );

        let clip = Some([0.0, 0.0, 400.0, 300.0]);
        assert!(rect_visible(Some([10.0, 10.0, 50.0, 20.0]), clip));
        assert!(
            rect_visible(Some([390.0, 290.0, 50.0, 20.0]), clip),
            "partly inside"
        );
        assert!(
            !rect_visible(Some([0.0, 300.0, 400.0, 20.0]), clip),
            "just below"
        );
        assert!(
            !rect_visible(Some([-60.0, 10.0, 50.0, 20.0]), clip),
            "left of it"
        );
        assert!(rect_visible(None, clip), "unknown geometry is walked");
        assert!(
            rect_visible(Some([900.0, 900.0, 1.0, 1.0]), None),
            "no clip: walked"
        );
        assert!(
            rect_visible(Some([900.0, 900.0, f64::NAN, 1.0]), clip),
            "NaN size: walked"
        );
        assert!(
            rect_visible(Some([f64::INFINITY, 0.0, 1.0, 1.0]), clip),
            "infinite: walked"
        );
        assert!(rect_visible(
            Some([10.0, 10.0, 5.0, 5.0]),
            Some([0.0, 0.0, f64::NAN, 1.0])
        ));
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
        // A child the deadline left unclassified may be a button, not a row:
        // it is never folded into an "offscreen rows" line. Deferred as
        // visible, the walker's admission refuses it after the deadline and
        // counts it as pending, so the read reports the timeout.
        for (probe_limit, expected) in [
            (0, [shown, shown, shown, shown]),
            (2, [shown, ChildPlan::WalkNow, shown, shown]),
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
