use super::walker_test_support::{Batch, Slot};
use super::*;
use crate::ax::batch_attrs::BatchFailure;
use crate::ax::content_attrs::AttrStatus;
use crate::ax::write_outcome::Settable;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

pub(super) struct Fake {
    state: Rc<RefCell<State>>,
}

struct State {
    scripts: VecDeque<Step>,
    trace: Vec<String>,
    identities: HashMap<usize, (usize, usize)>,
    children: HashMap<usize, Vec<usize>>,
    handles: HashMap<usize, usize>,
    snapshot_credits: HashMap<usize, usize>,
}

pub(super) struct Handle {
    id: usize,
    state: Rc<RefCell<State>>,
}

impl Clone for Handle {
    fn clone(&self) -> Self {
        let mut state = self.state.borrow_mut();
        *state.handles.get_mut(&self.id).expect("live handle") += 1;
        state.trace.push(format!("retain {}", self.id));
        drop(state);
        Self {
            id: self.id,
            state: self.state.clone(),
        }
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        let count = state.handles.get_mut(&self.id).expect("live handle");
        assert!(*count > 0);
        *count -= 1;
        state.trace.push(format!("release {}", self.id));
    }
}

// Every target read must be explicitly scripted; scripts are in global order.
pub(super) enum Reply {
    Unit,
    Nested(Box<dyn FnOnce()>),
    Text(Read<String>),
    Status(AttrStatus),
    Bool(Read<bool>),
    Number(Read<f64>),
    Point(Read<(f64, f64)>),
    Value(Result<Option<super::super::bindings::StringishAttrValue>, AXError>),
    Batch(Result<Batch, BatchFailure>),
    Children(Read<Vec<usize>>),
    Element(Read<usize>),
    Set(Read<Vec<usize>>),
    Actions(Read<Vec<String>>),
    Writable(bool),
    Selected(Settable),
}
pub(super) struct Step {
    operation: String,
    reply: Reply,
}
impl Fake {
    fn step(&self, operation: String) -> Reply {
        loop {
            let mut state = self.state.borrow_mut();
            let step = state
                .scripts
                .pop_front()
                .unwrap_or_else(|| panic!("script exhausted at {operation}"));
            assert_eq!(step.operation, operation);
            match step.reply {
                Reply::Nested(walk) => {
                    drop(state);
                    walk();
                }
                reply => {
                    state.trace.push(operation);
                    return reply;
                }
            }
        }
    }
    pub(super) fn handle(&self, id: usize) -> Handle {
        assert_ne!(id, 0);
        let mut state = self.state.borrow_mut();
        assert!(state.identities.contains_key(&id), "unknown fake id {id}");
        *state.handles.entry(id).or_default() += 1;
        state.trace.push(format!("adopt {id}"));
        drop(state);
        Handle {
            id,
            state: self.state.clone(),
        }
    }
    pub(super) fn assert_consumed(&self) {
        assert!(
            self.state.borrow().scripts.is_empty(),
            "unconsumed required operations"
        );
    }
}
impl AxTreeSource for Fake {
    type Element = Handle;
    type Batch = Batch;
    type ElementSet = Vec<usize>;
    fn key(&self, e: &Handle) -> usize {
        e.id
    }
    fn set_messaging_timeout(&self, e: &Handle) {
        assert!(matches!(
            self.step(format!("timeout {}", e.id)),
            Reply::Unit
        ));
    }
    fn set_optional_timeout(&self, e: &Handle, seconds: f32) {
        assert!(matches!(
            self.step(format!(
                "optional_timeout {} {}",
                e.id,
                if seconds == 0.0 { "reset" } else { "set" }
            )),
            Reply::Unit
        ));
    }
    fn text(&self, e: &Handle, name: &str) -> Read<String> {
        match self.step(format!("text {} {name}", e.id)) {
            Reply::Text(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    fn text_status(&self, e: &Handle, name: &str) -> AttrStatus {
        match self.step(format!("status {} {name}", e.id)) {
            Reply::Status(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    fn boolean(&self, e: &Handle, name: &str) -> Read<bool> {
        match self.step(format!("bool {} {name}", e.id)) {
            Reply::Bool(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    fn number(&self, e: &Handle, name: &str) -> Read<f64> {
        match self.step(format!("number {} {name}", e.id)) {
            Reply::Number(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    fn position(&self, e: &Handle) -> Read<(f64, f64)> {
        match self.step(format!("position {}", e.id)) {
            Reply::Point(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    fn size(&self, e: &Handle) -> Read<(f64, f64)> {
        match self.step(format!("size {}", e.id)) {
            Reply::Point(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    fn value(
        &self,
        e: &Handle,
    ) -> Result<Option<super::super::bindings::StringishAttrValue>, AXError> {
        match self.step(format!("value {}", e.id)) {
            Reply::Value(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    fn batch(&self, e: &Handle, names: &[&str]) -> Result<Batch, BatchFailure> {
        match self.step(format!("batch {} {}", e.id, names.join(","))) {
            Reply::Batch(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    fn children(&self, e: &Handle) -> Read<Vec<Handle>> {
        match self.step(format!("children {}", e.id)) {
            Reply::Children(Read::Known(ids)) => {
                assert_eq!(self.state.borrow().children.get(&e.id), Some(&ids));
                Read::Known(ids.into_iter().map(|id| self.handle(id)).collect())
            }
            Reply::Children(Read::Absent) => Read::Absent,
            Reply::Children(Read::Unknown(e)) => Read::Unknown(e),
            _ => panic!("wrong reply"),
        }
    }
    fn element_attribute(&self, e: &Handle, name: &str) -> Read<Handle> {
        match self.step(format!("element {} {name}", e.id)) {
            Reply::Element(Read::Known(id)) => Read::Known(self.handle(id)),
            Reply::Element(Read::Absent) => Read::Absent,
            Reply::Element(Read::Unknown(e)) => Read::Unknown(e),
            _ => panic!("wrong reply"),
        }
    }
    fn element_set(&self, e: &Handle, name: &str) -> Read<Vec<usize>> {
        match self.step(format!("set {} {name}", e.id)) {
            Reply::Set(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    fn set_contains(&self, set: &Vec<usize>, e: &Handle) -> bool {
        assert!(matches!(
            self.step(format!("contains {} {:?}", e.id, set)),
            Reply::Unit
        ));
        let state = self.state.borrow();
        let (hash, identity) = state.identities[&e.id];
        set.iter().any(|item| {
            let (other_hash, other_identity) = state.identities[item];
            hash == other_hash && identity == other_identity
        })
    }
    fn action_names(&self, e: &Handle) -> Read<Vec<String>> {
        match self.step(format!("actions {}", e.id)) {
            Reply::Actions(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    fn value_settable(&self, e: &Handle) -> Settable {
        match self.step(format!("value_settable {}", e.id)) {
            Reply::Writable(v) => {
                if v {
                    Settable::Yes
                } else {
                    Settable::No
                }
            }
            _ => panic!("wrong reply"),
        }
    }
    fn selected_settable(&self, e: &Handle) -> Settable {
        match self.step(format!("selected_settable {}", e.id)) {
            Reply::Selected(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    fn frame(&self, e: &Handle) -> Read<[f64; 4]> {
        let position = match self.position(e) {
            Read::Known(v) => v,
            Read::Absent => return Read::Absent,
            Read::Unknown(e) => return Read::Unknown(e),
        };
        match self.size(e) {
            Read::Known(size) => {
                super::super::bindings::rect_from_parts(Some(position), Some(size))
                    .map_or(Read::Absent, Read::Known)
            }
            Read::Absent => Read::Absent,
            Read::Unknown(e) => Read::Unknown(e),
        }
    }
    fn retain_for_snapshot(&self, e: &Handle) -> usize {
        assert!(matches!(
            self.step(format!("snapshot {}", e.id)),
            Reply::Unit
        ));
        *self
            .state
            .borrow_mut()
            .snapshot_credits
            .entry(e.id)
            .or_default() += 1;
        e.id
    }
}

impl Fake {
    pub(super) fn new(nodes: &[(usize, usize, usize, &[usize])], steps: Vec<Step>) -> Self {
        let mut identities = HashMap::new();
        let mut children = HashMap::new();
        for &(id, hash, identity, descendants) in nodes {
            assert_ne!(id, 0);
            assert!(identities.insert(id, (hash, identity)).is_none());
            children.insert(id, descendants.to_vec());
        }
        for descendants in children.values() {
            for id in descendants {
                assert!(identities.contains_key(id), "unknown child id {id}");
            }
        }
        Self {
            state: Rc::new(RefCell::new(State {
                scripts: steps.into(),
                trace: Vec::new(),
                identities,
                children,
                handles: HashMap::new(),
                snapshot_credits: HashMap::new(),
            })),
        }
    }
    pub(super) fn trace(&self) -> Vec<String> {
        self.state.borrow().trace.clone()
    }
    pub(super) fn assert_lifetimes(&self, snapshot_credits: &[(usize, usize)]) {
        let state = self.state.borrow();
        assert!(
            state.handles.values().all(|count| *count == 0),
            "leaked handles"
        );
        assert_eq!(
            state.snapshot_credits,
            snapshot_credits.iter().copied().collect()
        );
    }
}

fn run_fake(
    source: &Fake,
    root: &Handle,
    query: Option<&str>,
    max_depth: usize,
    mut budget: WalkBudget,
    include_web_content: bool,
) -> TreeWalkResult {
    let ledger = SkipLedger::start();
    let mut nodes = Vec::new();
    let mut lines = Vec::new();
    let mut counter = 0;
    let mut disabled = false;
    let mut deferred = Vec::new();
    let mut claims = Vec::new();
    dense_rows::with_visible_clip(rows_windowed().then_some(root.id), || {
        walk_element(
            source,
            root,
            root,
            0,
            None,
            None,
            LabelPath::Inactive,
            false,
            &mut nodes,
            &mut lines,
            &mut counter,
            &mut budget,
            &mut disabled,
            max_depth,
            include_web_content,
            &mut deferred,
            &mut claims,
        );
    });
    let (lines, nodes, offscreen) = finish_deferred(
        source,
        lines,
        nodes,
        deferred,
        query,
        &mut counter,
        &mut budget,
        &mut disabled,
        max_depth,
        include_web_content,
        claims,
    );
    let outcome = budget.outcome();
    finalize_walk(
        lines,
        nodes,
        offscreen,
        query,
        outcome,
        ledger.finish(),
        None,
    )
}

pub(super) fn step(operation: &str, reply: Reply) -> Step {
    Step {
        operation: operation.to_owned(),
        reply,
    }
}
fn content(title: Option<&str>) -> Batch {
    let mut attrs = vec![Slot::Text(AttrStatus::Absent); 7];
    if let Some(title) = title {
        attrs[0] = Slot::Text(AttrStatus::Known(title.into()));
    }
    Batch(attrs)
}
fn frame_batch() -> Batch {
    Batch(vec![Slot::Point(None), Slot::Size(None)])
}
fn control_batch() -> Batch {
    Batch(vec![
        Slot::Bool(None),
        Slot::Point(None),
        Slot::Size(None),
        Slot::Text(AttrStatus::Absent),
        Slot::Number(None),
        Slot::Number(None),
        Slot::Bool(None),
    ])
}
fn str_value(value: &str) -> Reply {
    Reply::Value(Ok(Some(super::super::bindings::StringishAttrValue {
        string_value: Some(value.into()),
        state_value: value.into(),
    })))
}
const CONTENT: &str =
    "AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription";
const FRAME: &str = "AXPosition,AXSize";
const CONTROL: &str =
    "AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected";

// Baseline 36279fdc: a collapsed layout wrapper preserves document order,
// addressable controls, display text and native emission credits.
#[test]
fn representative_window_markdown_and_nodes_are_pinned() {
    let source = Fake::new(
        &[
            (1, 1, 1, &[2]),
            (2, 2, 2, &[3, 4, 5]),
            (3, 3, 3, &[]),
            (4, 4, 4, &[]),
            (5, 5, 5, &[]),
        ],
        vec![
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
            step(
                &format!("batch 1 {CONTENT}"),
                Reply::Batch(Ok(content(Some("Window")))),
            ),
            step("value 1", Reply::Value(Ok(None))),
            step("actions 1", Reply::Actions(Read::Known(vec![]))),
            step(&format!("batch 1 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
            step("children 1", Reply::Children(Read::Known(vec![2]))),
            step("timeout 2", Reply::Unit),
            step("text 2 AXRole", Reply::Text(Read::Known("AXGroup".into()))),
            step("children 2", Reply::Children(Read::Known(vec![3, 4, 5]))),
            step("timeout 3", Reply::Unit),
            step("text 3 AXRole", Reply::Text(Read::Known("AXButton".into()))),
            step(
                &format!("batch 3 {CONTENT}"),
                Reply::Batch(Ok(content(Some("Run")))),
            ),
            step("value 3", Reply::Value(Ok(None))),
            step(
                "actions 3",
                Reply::Actions(Read::Known(vec!["AXPress".into()])),
            ),
            step(
                &format!("batch 3 {CONTROL}"),
                Reply::Batch(Ok(control_batch())),
            ),
            step("snapshot 3", Reply::Unit),
            step("children 3", Reply::Children(Read::Absent)),
            step("timeout 4", Reply::Unit),
            step(
                "text 4 AXRole",
                Reply::Text(Read::Known("AXStaticText".into())),
            ),
            step(
                &format!("batch 4 {CONTENT}"),
                Reply::Batch(Ok(content(None))),
            ),
            step("value 4", str_value("Ready")),
            step("actions 4", Reply::Actions(Read::Known(vec![]))),
            step(&format!("batch 4 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
            step("children 4", Reply::Children(Read::Absent)),
            step("timeout 5", Reply::Unit),
            step(
                "text 5 AXRole",
                Reply::Text(Read::Known("AXTextField".into())),
            ),
            step(
                &format!("batch 5 {CONTENT}"),
                Reply::Batch(Ok(content(Some("Name")))),
            ),
            step("value 5", str_value("Ada")),
            step("actions 5", Reply::Actions(Read::Known(vec![]))),
            step("value_settable 5", Reply::Writable(true)),
            step(
                &format!("batch 5 {CONTROL}"),
                Reply::Batch(Ok(control_batch())),
            ),
            step("snapshot 5", Reply::Unit),
            step("children 5", Reply::Children(Read::Absent)),
        ],
    );
    let root = source.handle(1);
    let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(100), true);
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[(3, 1), (5, 1)]);
    assert_eq!(result.tree_markdown, "- AXWindow \"Window\"\n  - [0] AXButton \"Run\" [actions=[press]]\n  - AXStaticText = \"Ready\"\n  - [1] AXTextField \"Name\" = \"Ada\"\n");
    assert!(!result.nodes_skipped);
    assert!(!result.truncated);
    let projection: Vec<_> = result
        .nodes
        .iter()
        .map(|node| format!("{node:?}"))
        .collect();
    assert_eq!(projection, [
        "AXNode { owned_row_label: None, element_index: None, role: \"AXWindow\", title: Some(\"Window\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXButton\", title: Some(\"Run\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 3, depth: 1, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: None, role: \"AXStaticText\", title: None, value: Some(\"Ready\"), description: None, identifier: None, help: None, actions: [], element_ptr: 4, depth: 1, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: Some(1), role: \"AXTextField\", title: Some(\"Name\"), value: Some(\"Ada\"), description: None, identifier: None, help: None, actions: [], element_ptr: 5, depth: 1, parent_element_index: None, frame: None, value_state: Some(\"Ada\"), value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]);
    assert_eq!(source.trace(), [
        "adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "adopt 2", "timeout 2", "text 2 AXRole", "children 2", "adopt 3", "adopt 4", "adopt 5", "timeout 3", "text 3 AXRole", "batch 3 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 3", "actions 3", "batch 3 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 3", "children 3", "release 3", "timeout 4", "text 4 AXRole", "batch 4 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 4", "actions 4", "batch 4 AXPosition,AXSize", "children 4", "release 4", "timeout 5", "text 5 AXRole", "batch 5 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 5", "actions 5", "value_settable 5", "batch 5 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 5", "children 5", "release 5", "release 2", "release 1",
    ]);
}

// Baseline 36279fdc: false, absent and failed AXSelected all take the old
// collapsed-menu branch and call note_skipped without enumerating children.
#[test]
fn collapsed_menu_keeps_legacy_skip() {
    for selection in [
        Read::Known(false),
        Read::Absent,
        Read::Unknown(super::super::tree_source::ReadFailure::Ax(
            kAXErrorCannotComplete,
        )),
    ] {
        let source = Fake::new(
            &[(1, 1, 1, &[2]), (2, 2, 2, &[])],
            vec![
                step("timeout 1", Reply::Unit),
                step(
                    "text 1 AXRole",
                    Reply::Text(Read::Known("AXMenuBarItem".into())),
                ),
                step(
                    &format!("batch 1 {CONTENT}"),
                    Reply::Batch(Ok(content(Some("File")))),
                ),
                step("value 1", Reply::Value(Ok(None))),
                step(
                    "actions 1",
                    Reply::Actions(Read::Known(vec!["AXPress".into()])),
                ),
                step(
                    &format!("batch 1 {CONTROL}"),
                    Reply::Batch(Ok(control_batch())),
                ),
                step("snapshot 1", Reply::Unit),
                step("bool 1 AXSelected", Reply::Bool(selection)),
            ],
        );
        let root = source.handle(1);
        let result = with_menus_collapsed(true, || {
            run_fake(&source, &root, None, 10, WalkBudget::nodes_only(100), true)
        });
        drop(root);
        source.assert_consumed();
        source.assert_lifetimes(&[(1, 1)]);
        assert!(result.nodes_skipped);
        assert!(!result.truncated);
        assert_eq!(
            result.tree_markdown,
            "- [0] AXMenuBarItem \"File\" [actions=[press]]\n"
        );
        assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
            "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXMenuBarItem\", title: Some(\"File\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        ]);
        assert_eq!(source.trace(), [
            "adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 1", "bool 1 AXSelected", "release 1",
        ]);
    }
}

fn open_menu_case(collapse: bool, selected: bool, expected_trace: &[&str]) {
    let mut script = vec![
        step("timeout 1", Reply::Unit),
        step(
            "text 1 AXRole",
            Reply::Text(Read::Known("AXMenuBarItem".into())),
        ),
        step(
            &format!("batch 1 {CONTENT}"),
            Reply::Batch(Ok(content(Some("File")))),
        ),
        step("value 1", Reply::Value(Ok(None))),
        step(
            "actions 1",
            Reply::Actions(Read::Known(vec!["AXPress".into()])),
        ),
        step(
            &format!("batch 1 {CONTROL}"),
            Reply::Batch(Ok(control_batch())),
        ),
        step("snapshot 1", Reply::Unit),
    ];
    if collapse {
        script.push(step(
            "bool 1 AXSelected",
            Reply::Bool(Read::Known(selected)),
        ));
    }
    script.extend([
        step("children 1", Reply::Children(Read::Known(vec![2]))),
        step("timeout 2", Reply::Unit),
        step(
            "text 2 AXRole",
            Reply::Text(Read::Known("AXMenuItem".into())),
        ),
        step(
            &format!("batch 2 {CONTENT}"),
            Reply::Batch(Ok(content(Some("Open")))),
        ),
        step("value 2", Reply::Value(Ok(None))),
        step(
            "actions 2",
            Reply::Actions(Read::Known(vec!["AXPress".into()])),
        ),
        step(
            &format!("batch 2 {CONTROL}"),
            Reply::Batch(Ok(control_batch())),
        ),
        step("snapshot 2", Reply::Unit),
        step("children 2", Reply::Children(Read::Absent)),
    ]);
    let source = Fake::new(&[(1, 1, 1, &[2]), (2, 2, 2, &[])], script);
    let root = source.handle(1);
    let result = with_menus_collapsed(collapse, || {
        run_fake(&source, &root, None, 10, WalkBudget::nodes_only(100), true)
    });
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[(1, 1), (2, 1)]);
    assert!(!result.nodes_skipped);
    assert!(!result.truncated);
    assert_eq!(result.tree_markdown, "- [0] AXMenuBarItem \"File\" [actions=[press]]\n  - [1] AXMenuItem \"Open\" [actions=[press]]\n");
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
        "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXMenuBarItem\", title: Some(\"File\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: Some(1), role: \"AXMenuItem\", title: Some(\"Open\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 2, depth: 1, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]);
    assert_eq!(source.trace(), expected_trace);
}

// Baseline 36279fdc: an explicitly selected menu keeps its descendants.
#[test]
fn selected_open_menu_is_walked() {
    open_menu_case(true, true, &[
        "adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 1", "bool 1 AXSelected", "children 1", "adopt 2", "timeout 2", "text 2 AXRole", "batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 2", "actions 2", "batch 2 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 2", "children 2", "release 2", "release 1",
    ]);
}

// Baseline 36279fdc: when collapse is disabled, no AXSelected probe is made.
#[test]
fn expanded_menu_request_walks_closed_items() {
    open_menu_case(false, false, &[
        "adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 1", "children 1", "adopt 2", "timeout 2", "text 2 AXRole", "batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 2", "actions 2", "batch 2 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 2", "children 2", "release 2", "release 1",
    ]);
}
// Baseline 36279fdc: the first admission starts the clock and a zero timeout
// refuses the root before setting its messaging timeout or asking any AX data.
#[test]
fn zero_time_budget_starts_no_attribute_read() {
    let source = Fake::new(&[(1, 1, 1, &[])], vec![]);
    let root = source.handle(1);
    let result = run_fake(&source, &root, None, 10, WalkBudget::new(0, 100), true);
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    assert_eq!(source.trace(), ["adopt 1", "release 1"]);
    assert!(result.nodes.is_empty());
    assert!(result.truncated);
    assert_eq!(result.walk.reason(), Some("timeout"));
    assert_eq!(result.walk.nodes_visited, 0);
    assert_eq!(result.walk.nodes_pending, 1);
    assert!(!result.nodes_skipped);
    assert_eq!(result.tree_markdown, "\n⚠️ PARTIAL TREE: the 0 ms timeout_ms budget ran out after 0 node(s) (1 discovered but not visited). Every element listed is real; elements after the cut are missing. If the element you need is absent, retry with a larger timeout_ms (e.g. 5000) or narrow with query / max_depth.");
}

// Baseline 36279fdc: AXChildren error marks the walk skipped but retains its
// already emitted parent; a successful empty enumeration does not mark it.
#[test]
fn failed_children_read_keeps_parent_and_marks_skip() {
    for failed in [false, true] {
        let children = if failed {
            Read::Unknown(super::super::tree_source::ReadFailure::Ax(
                kAXErrorCannotComplete,
            ))
        } else {
            Read::Known(vec![])
        };
        let source = Fake::new(
            &[(1, 1, 1, &[])],
            vec![
                step("timeout 1", Reply::Unit),
                step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
                step(
                    &format!("batch 1 {CONTENT}"),
                    Reply::Batch(Ok(content(Some("Window")))),
                ),
                step("value 1", Reply::Value(Ok(None))),
                step("actions 1", Reply::Actions(Read::Known(vec![]))),
                step(&format!("batch 1 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
                step("children 1", Reply::Children(children)),
            ],
        );
        let root = source.handle(1);
        let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true);
        drop(root);
        source.assert_consumed();
        source.assert_lifetimes(&[]);
        assert_eq!(result.tree_markdown, "- AXWindow \"Window\"\n");
        assert_eq!(result.nodes_skipped, failed);
        assert!(!result.truncated);
        assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
            "AXNode { owned_row_label: None, element_index: None, role: \"AXWindow\", title: Some(\"Window\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        ]);
        assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "release 1"]);
    }
}

// Baseline 36279fdc: the depth cap performs one AXChildren probe; only a
// nonempty or failed probe marks a skip, and probed children are released.
#[test]
fn depth_cap_probes_children_once() {
    for case in 0..3 {
        let children = match case {
            0 => Read::Known(vec![]),
            1 => Read::Known(vec![2]),
            _ => Read::Unknown(super::super::tree_source::ReadFailure::Ax(
                kAXErrorCannotComplete,
            )),
        };
        let child_list: &[usize] = if case == 1 { &[2] } else { &[] };
        let source = Fake::new(
            &[(1, 1, 1, child_list), (2, 2, 2, &[])],
            vec![
                step("timeout 1", Reply::Unit),
                step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
                step(
                    &format!("batch 1 {CONTENT}"),
                    Reply::Batch(Ok(content(Some("Window")))),
                ),
                step("value 1", Reply::Value(Ok(None))),
                step("actions 1", Reply::Actions(Read::Known(vec![]))),
                step(&format!("batch 1 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
                step("children 1", Reply::Children(children)),
            ],
        );
        let root = source.handle(1);
        let result = run_fake(&source, &root, None, 0, WalkBudget::nodes_only(10), true);
        drop(root);
        source.assert_consumed();
        source.assert_lifetimes(&[]);
        assert_eq!(result.tree_markdown, "- AXWindow \"Window\"\n");
        assert_eq!(result.nodes_skipped, case != 0);
        assert!(!result.truncated);
        assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
            "AXNode { owned_row_label: None, element_index: None, role: \"AXWindow\", title: Some(\"Window\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        ]);
        let trace = source.trace();
        if case == 1 {
            assert_eq!(trace, ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "adopt 2", "release 2", "release 1"]);
        } else {
            assert_eq!(trace, ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "release 1"]);
        }
    }
}

// Baseline 36279fdc: secure role, secure subrole and a password placeholder
// each redact the value without requesting AXValue.
#[test]
fn secure_fields_never_request_axvalue() {
    for case in 0..3 {
        let role = if case == 0 {
            "AXSecureTextField"
        } else {
            "AXTextField"
        };
        let mut attrs = content(Some("Secret"));
        if case == 1 {
            attrs.0[5] = Slot::Text(AttrStatus::Known("AXSecureTextField".into()));
        }
        if case == 2 {
            attrs.0[1] = Slot::Text(AttrStatus::Known("Password".into()));
        }
        let source = Fake::new(
            &[(1, 1, 1, &[])],
            vec![
                step("timeout 1", Reply::Unit),
                step("text 1 AXRole", Reply::Text(Read::Known(role.into()))),
                step(&format!("batch 1 {CONTENT}"), Reply::Batch(Ok(attrs))),
                step("actions 1", Reply::Actions(Read::Known(vec![]))),
                step("value_settable 1", Reply::Writable(true)),
                step(
                    &format!("batch 1 {CONTROL}"),
                    Reply::Batch(Ok(control_batch())),
                ),
                step("snapshot 1", Reply::Unit),
                step("children 1", Reply::Children(Read::Absent)),
            ],
        );
        let root = source.handle(1);
        let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true);
        drop(root);
        source.assert_consumed();
        source.assert_lifetimes(&[(1, 1)]);
        assert!(!result.nodes_skipped);
        assert!(!result.truncated);
        let expected_markdown = if case == 0 {
            "- [0] AXSecureTextField \"Secret\" = \"[redacted]\"\n"
        } else {
            "- [0] AXTextField \"Secret\" = \"[redacted]\"\n"
        };
        assert_eq!(result.tree_markdown, expected_markdown);
        let expected_node = if case == 0 {
            "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXSecureTextField\", title: Some(\"Secret\"), value: Some(\"[redacted]\"), description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: Some(\"[redacted]\"), value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }"
        } else {
            "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXTextField\", title: Some(\"Secret\"), value: Some(\"[redacted]\"), description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: Some(\"[redacted]\"), value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }"
        };
        assert_eq!(
            result
                .nodes
                .iter()
                .map(|node| format!("{node:?}"))
                .collect::<Vec<_>>(),
            [expected_node]
        );
        assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "actions 1", "value_settable 1", "batch 1 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 1", "children 1", "release 1"]);
    }
}

// Baseline 36279fdc: failed or wrong-type security evidence withholds AXValue
// and marks an incomplete read while retaining a named writable control.
#[test]
fn unknown_security_evidence_never_requests_axvalue() {
    for evidence in [
        AttrStatus::WrongType,
        super::super::content_attrs::FAILED_TEST,
    ] {
        let mut attrs = content(Some("Secret"));
        attrs.0[5] = Slot::Text(evidence);
        let source = Fake::new(
            &[(1, 1, 1, &[])],
            vec![
                step("timeout 1", Reply::Unit),
                step(
                    "text 1 AXRole",
                    Reply::Text(Read::Known("AXTextField".into())),
                ),
                step(&format!("batch 1 {CONTENT}"), Reply::Batch(Ok(attrs))),
                step("actions 1", Reply::Actions(Read::Known(vec![]))),
                step("value_settable 1", Reply::Writable(true)),
                step(
                    &format!("batch 1 {CONTROL}"),
                    Reply::Batch(Ok(control_batch())),
                ),
                step("snapshot 1", Reply::Unit),
                step("children 1", Reply::Children(Read::Absent)),
            ],
        );
        let root = source.handle(1);
        let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true);
        drop(root);
        source.assert_consumed();
        source.assert_lifetimes(&[(1, 1)]);
        assert!(result.nodes_skipped);
        assert!(!result.truncated);
        assert_eq!(result.tree_markdown, "- [0] AXTextField \"Secret\"\n");
        assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
            "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXTextField\", title: Some(\"Secret\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        ]);
        assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "actions 1", "value_settable 1", "batch 1 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 1", "children 1", "release 1"]);
    }
    let source = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step(
                "text 1 AXRole",
                Reply::Text(Read::Known("AXTextField".into())),
            ),
            step(
                &format!("batch 1 {CONTENT}"),
                Reply::Batch(Err(BatchFailure::Decode)),
            ),
            step(
                "status 1 AXTitle",
                Reply::Status(AttrStatus::Known("Secret".into())),
            ),
            step(
                "status 1 AXPlaceholderValue",
                Reply::Status(AttrStatus::Absent),
            ),
            step("status 1 AXDescription", Reply::Status(AttrStatus::Absent)),
            step("status 1 AXIdentifier", Reply::Status(AttrStatus::Absent)),
            step("status 1 AXHelp", Reply::Status(AttrStatus::Absent)),
            step(
                "status 1 AXSubrole",
                Reply::Status(super::super::content_attrs::FAILED_TEST),
            ),
            step(
                "status 1 AXRoleDescription",
                Reply::Status(AttrStatus::Absent),
            ),
            step("actions 1", Reply::Actions(Read::Known(vec![]))),
            step("value_settable 1", Reply::Writable(true)),
            step(
                &format!("batch 1 {CONTROL}"),
                Reply::Batch(Ok(control_batch())),
            ),
            step("snapshot 1", Reply::Unit),
            step("children 1", Reply::Children(Read::Absent)),
        ],
    );
    let root = source.handle(1);
    let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true);
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[(1, 1)]);
    assert!(result.nodes_skipped && !result.truncated);
    assert_eq!(result.tree_markdown, "- [0] AXTextField \"Secret\"\n");
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), ["AXNode { owned_row_label: None, element_index: Some(0), role: \"AXTextField\", title: Some(\"Secret\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }"]);
    assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "status 1 AXTitle", "status 1 AXPlaceholderValue", "status 1 AXDescription", "status 1 AXIdentifier", "status 1 AXHelp", "status 1 AXSubrole", "status 1 AXRoleDescription", "actions 1", "value_settable 1", "batch 1 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 1", "children 1", "release 1"]);
}

// Baseline 36279fdc: web exclusion stops at its role, notes a skip and keeps
// the native chrome sibling and original actionable index.
#[test]
fn web_content_exclusion_preserves_legacy_skip() {
    let source = Fake::new(
        &[(1, 1, 1, &[2, 3]), (2, 2, 2, &[]), (3, 3, 3, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
            step(
                &format!("batch 1 {CONTENT}"),
                Reply::Batch(Ok(content(Some("Window")))),
            ),
            step("value 1", Reply::Value(Ok(None))),
            step("actions 1", Reply::Actions(Read::Known(vec![]))),
            step(&format!("batch 1 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
            step("children 1", Reply::Children(Read::Known(vec![2, 3]))),
            step("timeout 2", Reply::Unit),
            step("text 2 AXRole", Reply::Text(Read::Known("AXButton".into()))),
            step(
                &format!("batch 2 {CONTENT}"),
                Reply::Batch(Ok(content(Some("Back")))),
            ),
            step("value 2", Reply::Value(Ok(None))),
            step(
                "actions 2",
                Reply::Actions(Read::Known(vec!["AXPress".into()])),
            ),
            step(
                &format!("batch 2 {CONTROL}"),
                Reply::Batch(Ok(control_batch())),
            ),
            step("snapshot 2", Reply::Unit),
            step("children 2", Reply::Children(Read::Absent)),
            step("timeout 3", Reply::Unit),
            step(
                "text 3 AXRole",
                Reply::Text(Read::Known("AXWebArea".into())),
            ),
        ],
    );
    let root = source.handle(1);
    let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), false);
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[(2, 1)]);
    assert!(result.nodes_skipped);
    assert!(!result.truncated);
    assert_eq!(
        result.tree_markdown,
        "- AXWindow \"Window\"\n  - [0] AXButton \"Back\" [actions=[press]]\n"
    );
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
        "AXNode { owned_row_label: None, element_index: None, role: \"AXWindow\", title: Some(\"Window\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXButton\", title: Some(\"Back\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 2, depth: 1, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]);
    assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "adopt 2", "adopt 3", "timeout 2", "text 2 AXRole", "batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 2", "actions 2", "batch 2 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 2", "children 2", "release 2", "timeout 3", "text 3 AXRole", "release 3", "release 1"]);
}

// Baseline 36279fdc: listed generic failures for description and identifier
// on NSTextView still permit AXValue, after all classification singles.
#[test]
fn nstextview_listed_failure_exception_is_preserved() {
    let failure = AttrStatus::Failed {
        error: kAXErrorFailure,
        listed: super::super::content_attrs::AttributeListing::Listed,
    };
    let source = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step(
                "text 1 AXRole",
                Reply::Text(Read::Known("AXTextArea".into())),
            ),
            step(
                &format!("batch 1 {CONTENT}"),
                Reply::Batch(Err(BatchFailure::WholeCall(kAXErrorAttributeUnsupported))),
            ),
            step(
                "status 1 AXTitle",
                Reply::Status(AttrStatus::Known("Editor".into())),
            ),
            step(
                "status 1 AXPlaceholderValue",
                Reply::Status(AttrStatus::Absent),
            ),
            step("status 1 AXDescription", Reply::Status(failure.clone())),
            step("status 1 AXIdentifier", Reply::Status(failure)),
            step("status 1 AXHelp", Reply::Status(AttrStatus::Absent)),
            step("status 1 AXSubrole", Reply::Status(AttrStatus::Absent)),
            step(
                "status 1 AXRoleDescription",
                Reply::Status(AttrStatus::Known("text entry area".into())),
            ),
            step("value 1", str_value("Hello")),
            step("actions 1", Reply::Actions(Read::Known(vec![]))),
            step("value_settable 1", Reply::Writable(true)),
            step("bool 1 AXEnabled", Reply::Bool(Read::Absent)),
            step("position 1", Reply::Point(Read::Absent)),
            step("text 1 AXValueDescription", Reply::Text(Read::Absent)),
            step("number 1 AXMinValue", Reply::Number(Read::Absent)),
            step("number 1 AXMaxValue", Reply::Number(Read::Absent)),
            step("bool 1 AXSelected", Reply::Bool(Read::Absent)),
            step("snapshot 1", Reply::Unit),
            step("children 1", Reply::Children(Read::Absent)),
        ],
    );
    let root = source.handle(1);
    let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true);
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[(1, 1)]);
    assert!(!result.nodes_skipped);
    assert!(!result.truncated);
    assert_eq!(
        result.tree_markdown,
        "- [0] AXTextArea \"Editor\" = \"Hello\"\n"
    );
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
        "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXTextArea\", title: Some(\"Editor\"), value: Some(\"Hello\"), description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: Some(\"Hello\"), value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]);
    assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "status 1 AXTitle", "status 1 AXPlaceholderValue", "status 1 AXDescription", "status 1 AXIdentifier", "status 1 AXHelp", "status 1 AXSubrole", "status 1 AXRoleDescription", "value 1", "actions 1", "value_settable 1", "bool 1 AXEnabled", "position 1", "text 1 AXValueDescription", "number 1 AXMinValue", "number 1 AXMaxValue", "bool 1 AXSelected", "snapshot 1", "children 1", "release 1"]);
}

// Baseline 36279fdc: after the node cap, every enumerated sibling still
// attempts admission and increments pending without any attribute reads.
#[test]
fn node_budget_cut_preserves_pending_counts() {
    let source = Fake::new(
        &[
            (1, 1, 1, &[2, 3, 4]),
            (2, 2, 2, &[]),
            (3, 3, 3, &[]),
            (4, 4, 4, &[]),
        ],
        vec![
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXGroup".into()))),
            step("children 1", Reply::Children(Read::Known(vec![2, 3, 4]))),
            step("timeout 2", Reply::Unit),
            step("text 2 AXRole", Reply::Text(Read::Known("AXButton".into()))),
            step(
                &format!("batch 2 {CONTENT}"),
                Reply::Batch(Ok(content(Some("Run")))),
            ),
            step("value 2", Reply::Value(Ok(None))),
            step(
                "actions 2",
                Reply::Actions(Read::Known(vec!["AXPress".into()])),
            ),
            step(
                &format!("batch 2 {CONTROL}"),
                Reply::Batch(Ok(control_batch())),
            ),
            step("snapshot 2", Reply::Unit),
            step("children 2", Reply::Children(Read::Absent)),
        ],
    );
    let root = source.handle(1);
    let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(2), true);
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[(2, 1)]);
    assert!(result.truncated);
    assert!(!result.nodes_skipped);
    assert_eq!(result.walk.reason(), Some("node_budget"));
    assert_eq!(result.walk.nodes_visited, 2);
    assert_eq!(result.walk.nodes_pending, 2);
    assert_eq!(result.tree_markdown, "- [0] AXButton \"Run\" [actions=[press]]\n\n⚠️ PARTIAL TREE: the max_elements node budget ran out after 2 node(s) (2 discovered but not visited). Every element listed is real; elements after the cut are missing. If the element you need is absent, retry with a larger max_elements or narrow with query / max_depth.");
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
        "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXButton\", title: Some(\"Run\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 2, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]);
    assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "children 1", "adopt 2", "adopt 3", "adopt 4", "timeout 2", "text 2 AXRole", "batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 2", "actions 2", "batch 2 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 2", "children 2", "release 2", "release 3", "release 4", "release 1"]);
    assert_deferred_budget();
}

fn assert_deferred_budget() {
    let source = Fake::new(
        &[(1, 1, 1, &[2, 3]), (2, 2, 2, &[]), (3, 3, 3, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXTable".into()))),
            step(
                &format!("batch 1 {CONTENT}"),
                Reply::Batch(Ok(content(Some("Items")))),
            ),
            step("value 1", Reply::Value(Ok(None))),
            step("actions 1", Reply::Actions(Read::Known(vec![]))),
            step(&format!("batch 1 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
            step("children 1", Reply::Children(Read::Known(vec![2, 3]))),
            step("set 1 AXRows", Reply::Set(Read::Known(vec![2, 3]))),
            step("set 1 AXVisibleRows", Reply::Set(Read::Known(vec![2]))),
            step("contains 2 [2, 3]", Reply::Unit),
            step("contains 2 [2]", Reply::Unit),
            step("contains 3 [2, 3]", Reply::Unit),
            step("contains 3 [2]", Reply::Unit),
        ],
    );
    let root = source.handle(1);
    let result = with_dense_rows_windowed(true, || {
        run_fake(
            &source,
            &root,
            Some("missing"),
            10,
            WalkBudget::nodes_only(1),
            true,
        )
    });
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    assert!(result.truncated);
    assert!(!result.nodes_skipped);
    assert_eq!(result.walk.reason(), Some("node_budget"));
    assert_eq!(result.walk.nodes_visited, 1);
    assert_eq!(result.walk.nodes_pending, 2);
    assert_eq!(result.tree_markdown, "\n⚠️ PARTIAL TREE: the max_elements node budget ran out after 1 node(s) (2 discovered but not visited). Every element listed is real; elements after the cut are missing. If the element you need is absent, retry with a larger max_elements or narrow with query / max_depth.");
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), ["AXNode { owned_row_label: None, element_index: None, role: \"AXTable\", title: Some(\"Items\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }"]);
    assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "adopt 2", "adopt 3", "set 1 AXRows", "set 1 AXVisibleRows", "contains 2 [2, 3]", "contains 2 [2]", "contains 3 [2, 3]", "contains 3 [2]", "retain 2", "release 2", "retain 3", "release 3", "release 2", "release 3", "release 1"]);
}

fn fallback_window_script(failure: BatchFailure, frame_batch_after: bool) -> Vec<Step> {
    let mut script = vec![
        step("timeout 1", Reply::Unit),
        step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
        step(&format!("batch 1 {CONTENT}"), Reply::Batch(Err(failure))),
        step(
            "status 1 AXTitle",
            Reply::Status(AttrStatus::Known("Window".into())),
        ),
        step(
            "status 1 AXPlaceholderValue",
            Reply::Status(AttrStatus::Absent),
        ),
        step("status 1 AXDescription", Reply::Status(AttrStatus::Absent)),
        step("status 1 AXIdentifier", Reply::Status(AttrStatus::Absent)),
        step("status 1 AXHelp", Reply::Status(AttrStatus::Absent)),
        step("status 1 AXSubrole", Reply::Status(AttrStatus::Absent)),
        step(
            "status 1 AXRoleDescription",
            Reply::Status(AttrStatus::Absent),
        ),
        step("value 1", Reply::Value(Ok(None))),
        step("actions 1", Reply::Actions(Read::Known(vec![]))),
    ];
    if frame_batch_after {
        script.push(step(
            &format!("batch 1 {FRAME}"),
            Reply::Batch(Ok(frame_batch())),
        ));
    } else {
        script.push(step("position 1", Reply::Point(Read::Absent)));
    }
    script.push(step("children 1", Reply::Children(Read::Absent)));
    script
}
fn assert_fallback_window(result: &TreeWalkResult) {
    assert_eq!(result.tree_markdown, "- AXWindow \"Window\"\n");
    assert!(!result.nodes_skipped);
    assert!(!result.truncated);
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
        "AXNode { owned_row_label: None, element_index: None, role: \"AXWindow\", title: Some(\"Window\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]);
}

// Baseline 36279fdc: an unsupported batch disables batching for the remainder
// of only that walk; a second walk begins by attempting its content batch.
#[test]
fn unsupported_batch_disables_only_current_walk() {
    let mut script =
        fallback_window_script(BatchFailure::WholeCall(kAXErrorAttributeUnsupported), false);
    script.extend([
        step("timeout 1", Reply::Unit),
        step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
        step(
            &format!("batch 1 {CONTENT}"),
            Reply::Batch(Ok(content(Some("Window")))),
        ),
        step("value 1", Reply::Value(Ok(None))),
        step("actions 1", Reply::Actions(Read::Known(vec![]))),
        step(&format!("batch 1 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
        step("children 1", Reply::Children(Read::Absent)),
    ]);
    let source = Fake::new(&[(1, 1, 1, &[])], script);
    let root = source.handle(1);
    let first = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true);
    let second = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true);
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    assert_fallback_window(&first);
    assert_fallback_window(&second);
    assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "status 1 AXTitle", "status 1 AXPlaceholderValue", "status 1 AXDescription", "status 1 AXIdentifier", "status 1 AXHelp", "status 1 AXSubrole", "status 1 AXRoleDescription", "value 1", "actions 1", "position 1", "children 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "release 1"]);
    assert_deferred_batch_disabled();
}

fn assert_deferred_batch_disabled() {
    let mut script = vec![
        step("timeout 1", Reply::Unit),
        step("text 1 AXRole", Reply::Text(Read::Known("AXTable".into()))),
        step(
            &format!("batch 1 {CONTENT}"),
            Reply::Batch(Err(BatchFailure::WholeCall(kAXErrorAttributeUnsupported))),
        ),
    ];
    for (name, value) in [
        ("AXTitle", Some("Items")),
        ("AXPlaceholderValue", None),
        ("AXDescription", None),
        ("AXIdentifier", None),
        ("AXHelp", None),
        ("AXSubrole", None),
        ("AXRoleDescription", None),
    ] {
        script.push(step(
            &format!("status 1 {name}"),
            Reply::Status(value.map_or(AttrStatus::Absent, |s| AttrStatus::Known(s.into()))),
        ));
    }
    script.extend([
        step("value 1", Reply::Value(Ok(None))),
        step("actions 1", Reply::Actions(Read::Known(vec![]))),
        step("position 1", Reply::Point(Read::Absent)),
        step("children 1", Reply::Children(Read::Known(vec![2]))),
        step("set 1 AXRows", Reply::Set(Read::Known(vec![2]))),
        step("set 1 AXVisibleRows", Reply::Set(Read::Known(vec![2]))),
        step("contains 2 [2]", Reply::Unit),
        step("contains 2 [2]", Reply::Unit),
        step("timeout 2", Reply::Unit),
        step("text 2 AXRole", Reply::Text(Read::Known("AXRow".into()))),
    ]);
    for (name, value) in [
        ("AXTitle", Some("Row")),
        ("AXPlaceholderValue", None),
        ("AXDescription", None),
        ("AXIdentifier", None),
        ("AXHelp", None),
        ("AXSubrole", None),
        ("AXRoleDescription", None),
    ] {
        script.push(step(
            &format!("status 2 {name}"),
            Reply::Status(value.map_or(AttrStatus::Absent, |s| AttrStatus::Known(s.into()))),
        ));
    }
    script.extend([
        step("value 2", Reply::Value(Ok(None))),
        step(
            "actions 2",
            Reply::Actions(Read::Known(vec!["AXPress".into()])),
        ),
        step("bool 2 AXEnabled", Reply::Bool(Read::Absent)),
        step("position 2", Reply::Point(Read::Absent)),
        step("text 2 AXValueDescription", Reply::Text(Read::Absent)),
        step("number 2 AXMinValue", Reply::Number(Read::Absent)),
        step("number 2 AXMaxValue", Reply::Number(Read::Absent)),
        step("bool 2 AXSelected", Reply::Bool(Read::Absent)),
        step("snapshot 2", Reply::Unit),
        step("children 2", Reply::Children(Read::Absent)),
    ]);
    let source = Fake::new(&[(1, 1, 1, &[2]), (2, 2, 2, &[])], script);
    let root = source.handle(1);
    let result = with_dense_rows_windowed(true, || {
        run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true)
    });
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[(2, 1)]);
    assert!(!result.nodes_skipped && !result.truncated);
    assert_eq!(
        result.tree_markdown,
        "- AXTable \"Items\"\n  - [0] AXRow \"Row\" [actions=[press]]\n"
    );
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
        "AXNode { owned_row_label: None, element_index: None, role: \"AXTable\", title: Some(\"Items\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: Some(None), element_index: Some(0), role: \"AXRow\", title: Some(\"Row\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 2, depth: 1, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]);
    assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "status 1 AXTitle", "status 1 AXPlaceholderValue", "status 1 AXDescription", "status 1 AXIdentifier", "status 1 AXHelp", "status 1 AXSubrole", "status 1 AXRoleDescription", "value 1", "actions 1", "position 1", "children 1", "adopt 2", "set 1 AXRows", "set 1 AXVisibleRows", "contains 2 [2]", "contains 2 [2]", "retain 2", "release 2", "timeout 2", "text 2 AXRole", "status 2 AXTitle", "status 2 AXPlaceholderValue", "status 2 AXDescription", "status 2 AXIdentifier", "status 2 AXHelp", "status 2 AXSubrole", "status 2 AXRoleDescription", "value 2", "actions 2", "bool 2 AXEnabled", "position 2", "text 2 AXValueDescription", "number 2 AXMinValue", "number 2 AXMaxValue", "bool 2 AXSelected", "snapshot 2", "children 2", "release 2", "release 1"]);
}

// Baseline 36279fdc: a batch decode failure replays singles but leaves batch
// support enabled for the frame request in the same walk.
#[test]
fn decode_failure_replays_without_disabling_batching() {
    let source = Fake::new(
        &[(1, 1, 1, &[])],
        fallback_window_script(BatchFailure::Decode, true),
    );
    let root = source.handle(1);
    let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true);
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    assert_fallback_window(&result);
    assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "status 1 AXTitle", "status 1 AXPlaceholderValue", "status 1 AXDescription", "status 1 AXIdentifier", "status 1 AXHelp", "status 1 AXSubrole", "status 1 AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "release 1"]);
}
fn dense_case(search: bool, alias: bool) -> (TreeWalkResult, Vec<String>) {
    let rows = if alias { vec![20, 40] } else { vec![2, 4] };
    let visible = if alias { vec![20] } else { vec![2] };
    let mut script = vec![
        step("timeout 1", Reply::Unit),
        step("text 1 AXRole", Reply::Text(Read::Known("AXTable".into()))),
        step(
            &format!("batch 1 {CONTENT}"),
            Reply::Batch(Ok(content(Some("Items")))),
        ),
        step("value 1", Reply::Value(Ok(None))),
        step(
            "actions 1",
            Reply::Actions(Read::Known(vec!["AXPress".into()])),
        ),
        step(
            &format!("batch 1 {CONTROL}"),
            Reply::Batch(Ok(control_batch())),
        ),
        step("snapshot 1", Reply::Unit),
        step("children 1", Reply::Children(Read::Known(vec![2, 3, 4]))),
        step("set 1 AXRows", Reply::Set(Read::Known(rows.clone()))),
        step(
            "set 1 AXVisibleRows",
            Reply::Set(Read::Known(visible.clone())),
        ),
        step(&format!("contains 2 {:?}", rows), Reply::Unit),
        step(&format!("contains 2 {:?}", visible), Reply::Unit),
        step(&format!("contains 3 {:?}", rows), Reply::Unit),
        step(&format!("contains 4 {:?}", rows), Reply::Unit),
        step(&format!("contains 4 {:?}", visible), Reply::Unit),
        step("timeout 3", Reply::Unit),
        step("text 3 AXRole", Reply::Text(Read::Known("AXButton".into()))),
        step(
            &format!("batch 3 {CONTENT}"),
            Reply::Batch(Ok(content(Some("Now")))),
        ),
        step("value 3", Reply::Value(Ok(None))),
        step(
            "actions 3",
            Reply::Actions(Read::Known(vec!["AXPress".into()])),
        ),
        step(
            &format!("batch 3 {CONTROL}"),
            Reply::Batch(Ok(control_batch())),
        ),
        step("snapshot 3", Reply::Unit),
        step("children 3", Reply::Children(Read::Absent)),
        step("timeout 2", Reply::Unit),
        step("text 2 AXRole", Reply::Text(Read::Known("AXRow".into()))),
        step(
            &format!("batch 2 {CONTENT}"),
            Reply::Batch(Ok(content(None))),
        ),
        step("value 2", Reply::Value(Ok(None))),
        step(
            "actions 2",
            Reply::Actions(Read::Known(vec!["AXPress".into()])),
        ),
        step(
            &format!("batch 2 {CONTROL}"),
            Reply::Batch(Ok(control_batch())),
        ),
        step("snapshot 2", Reply::Unit),
        step("children 2", Reply::Children(Read::Known(vec![5]))),
        step("timeout 5", Reply::Unit),
        step(
            "text 5 AXRole",
            Reply::Text(Read::Known("AXStaticText".into())),
        ),
        step(
            &format!("batch 5 {CONTENT}"),
            Reply::Batch(Ok(content(None))),
        ),
        step("value 5", str_value("Alpha")),
        step("actions 5", Reply::Actions(Read::Known(vec![]))),
        step(&format!("batch 5 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
        step("children 5", Reply::Children(Read::Absent)),
    ];
    if search {
        script.extend([
            step("timeout 4", Reply::Unit),
            step("text 4 AXRole", Reply::Text(Read::Known("AXRow".into()))),
            step(
                &format!("batch 4 {CONTENT}"),
                Reply::Batch(Ok(content(Some("Beta")))),
            ),
            step("value 4", Reply::Value(Ok(None))),
            step(
                "actions 4",
                Reply::Actions(Read::Known(vec!["AXPress".into()])),
            ),
            step(
                &format!("batch 4 {CONTROL}"),
                Reply::Batch(Ok(control_batch())),
            ),
            step("snapshot 4", Reply::Unit),
            step("children 4", Reply::Children(Read::Absent)),
        ]);
    }
    // 2 and 20 have equal identities; all three identities deliberately share
    // one hash to cover collisions rather than pointer-only membership.
    let source = Fake::new(
        &[
            (1, 1, 1, &[2, 3, 4]),
            (2, 7, 2, &[5]),
            (3, 7, 3, &[]),
            (4, 7, 4, &[]),
            (5, 5, 5, &[]),
            (20, 7, 2, &[]),
            (40, 7, 4, &[]),
        ],
        script,
    );
    let root = source.handle(1);
    let result = with_dense_rows_windowed(true, || {
        run_fake(
            &source,
            &root,
            search.then_some("Beta"),
            10,
            WalkBudget::nodes_only(100),
            true,
        )
    });
    drop(root);
    source.assert_consumed();
    if search {
        source.assert_lifetimes(&[(1, 1), (2, 1), (3, 1), (4, 1)]);
    } else {
        source.assert_lifetimes(&[(1, 1), (2, 1), (3, 1)]);
    }
    (result, source.trace())
}

fn assert_dense_result(result: &TreeWalkResult, search: bool) {
    assert_eq!(
        result
            .nodes
            .iter()
            .map(|node| format!("{node:?}"))
            .collect::<Vec<_>>(),
        if search {
            vec![
        "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXTable\", title: Some(\"Items\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: Some(Some(\"Alpha\")), element_index: Some(1), role: \"AXRow\", title: None, value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 2, depth: 1, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: None, role: \"AXStaticText\", title: None, value: Some(\"Alpha\"), description: None, identifier: None, help: None, actions: [], element_ptr: 5, depth: 2, parent_element_index: Some(1), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: Some(2), role: \"AXButton\", title: Some(\"Now\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 3, depth: 1, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: Some(None), element_index: Some(3), role: \"AXRow\", title: Some(\"Beta\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 4, depth: 1, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]
        } else {
            vec![
        "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXTable\", title: Some(\"Items\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: Some(Some(\"Alpha\")), element_index: Some(1), role: \"AXRow\", title: None, value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 2, depth: 1, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: None, role: \"AXStaticText\", title: None, value: Some(\"Alpha\"), description: None, identifier: None, help: None, actions: [], element_ptr: 5, depth: 2, parent_element_index: Some(1), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: Some(2), role: \"AXButton\", title: Some(\"Now\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 3, depth: 1, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]
        }
    );
    assert!(!result.truncated);
    assert_eq!(result.nodes_skipped, !search);
    if search {
        assert_eq!(
            result.tree_markdown,
            "- [0] AXTable \"Items\" [actions=[press]]\n  - [3] AXRow \"Beta\" [actions=[press]]\n"
        );
    } else {
        assert_eq!(result.tree_markdown, "- [0] AXTable \"Items\" [actions=[press]]\n  - [1] AXRow [actions=[press]]\n    - AXStaticText = \"Alpha\"\n  - [2] AXButton \"Now\" [actions=[press]]\n  - (offscreen rows: 1; scroll the list, or pass query to search them)\n");
        assert_eq!(
            result.offscreen_rows,
            vec![dense_rows::OffscreenRows {
                parent_index: Some(0),
                first_position: 2,
                last_position: 2,
                count: 1
            }]
        );
    }
}

// Baseline 36279fdc: visible rows walk after the immediate sibling, then
// splice back before it; offscreen rows retain their place and summary.
#[test]
fn deferred_rows_keep_order_indices_parents_and_labels() {
    let (result, trace) = dense_case(false, false);
    assert_dense_result(&result, false);
    assert_eq!(trace, ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 1", "children 1", "adopt 2", "adopt 3", "adopt 4", "set 1 AXRows", "set 1 AXVisibleRows", "contains 2 [2, 4]", "contains 2 [2]", "contains 3 [2, 4]", "contains 4 [2, 4]", "contains 4 [2]", "retain 2", "release 2", "timeout 3", "text 3 AXRole", "batch 3 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 3", "actions 3", "batch 3 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 3", "children 3", "release 3", "retain 4", "release 4", "timeout 2", "text 2 AXRole", "batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 2", "actions 2", "batch 2 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 2", "children 2", "adopt 5", "timeout 5", "text 5 AXRole", "batch 5 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 5", "actions 5", "batch 5 AXPosition,AXSize", "children 5", "release 5", "release 2", "release 4", "release 1"]);
}

// Baseline 36279fdc: a query also walks offscreen deferred rows while
// retaining original ancestry and document order after renumbering.
#[test]
fn query_walks_deferred_offscreen_rows() {
    let (result, trace) = dense_case(true, false);
    assert_dense_result(&result, true);
    assert_eq!(trace, ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 1", "children 1", "adopt 2", "adopt 3", "adopt 4", "set 1 AXRows", "set 1 AXVisibleRows", "contains 2 [2, 4]", "contains 2 [2]", "contains 3 [2, 4]", "contains 4 [2, 4]", "contains 4 [2]", "retain 2", "release 2", "timeout 3", "text 3 AXRole", "batch 3 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 3", "actions 3", "batch 3 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 3", "children 3", "release 3", "retain 4", "release 4", "timeout 2", "text 2 AXRole", "batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 2", "actions 2", "batch 2 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 2", "children 2", "adopt 5", "timeout 5", "text 5 AXRole", "batch 5 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 5", "actions 5", "batch 5 AXPosition,AXSize", "children 5", "release 5", "timeout 4", "text 4 AXRole", "batch 4 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 4", "actions 4", "batch 4 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 4", "children 4", "release 2", "release 4", "release 1"]);
}

// Baseline 36279fdc: CFHash buckets plus CFEqual accept distinct proxies
// for a row and reject another row with the same hash.
#[test]
fn dense_visibility_uses_ax_identity() {
    let (result, trace) = dense_case(false, true);
    assert_dense_result(&result, false);
    assert_eq!(trace, ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 1", "children 1", "adopt 2", "adopt 3", "adopt 4", "set 1 AXRows", "set 1 AXVisibleRows", "contains 2 [20, 40]", "contains 2 [20]", "contains 3 [20, 40]", "contains 4 [20, 40]", "contains 4 [20]", "retain 2", "release 2", "timeout 3", "text 3 AXRole", "batch 3 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 3", "actions 3", "batch 3 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 3", "children 3", "release 3", "retain 4", "release 4", "timeout 2", "text 2 AXRole", "batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 2", "actions 2", "batch 2 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 2", "children 2", "adopt 5", "timeout 5", "text 5 AXRole", "batch 5 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 5", "actions 5", "batch 5 AXPosition,AXSize", "children 5", "release 5", "release 2", "release 4", "release 1"]);
}
fn row_fixture(nested: bool, two_texts: bool) -> (TreeWalkResult, Vec<String>) {
    let node3_role = if nested { "AXRow" } else { "AXCell" };
    let node3_title = if nested { None } else { Some("Cell") };
    let node3_children = if two_texts { vec![4, 5] } else { vec![4] };
    let mut script = vec![
        step("timeout 1", Reply::Unit),
        step(
            "text 1 AXRole",
            Reply::Text(Read::Known(
                if nested { "AXOutline" } else { "AXTable" }.into(),
            )),
        ),
        step(
            &format!("batch 1 {CONTENT}"),
            Reply::Batch(Ok(content(Some("Items")))),
        ),
        step("value 1", Reply::Value(Ok(None))),
        step("actions 1", Reply::Actions(Read::Known(vec![]))),
        step(&format!("batch 1 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
        step("children 1", Reply::Children(Read::Known(vec![2]))),
        step("timeout 2", Reply::Unit),
        step("text 2 AXRole", Reply::Text(Read::Known("AXRow".into()))),
        step(
            &format!("batch 2 {CONTENT}"),
            Reply::Batch(Ok(content(None))),
        ),
        step("value 2", Reply::Value(Ok(None))),
        step("actions 2", Reply::Actions(Read::Known(vec![]))),
        step("selected_settable 2", Reply::Selected(Settable::Yes)),
        step(
            &format!("batch 2 {CONTROL}"),
            Reply::Batch(Ok(control_batch())),
        ),
        step("snapshot 2", Reply::Unit),
        step("children 2", Reply::Children(Read::Known(vec![3]))),
        step("timeout 3", Reply::Unit),
        step("text 3 AXRole", Reply::Text(Read::Known(node3_role.into()))),
        step(
            &format!("batch 3 {CONTENT}"),
            Reply::Batch(Ok(content(node3_title))),
        ),
        step("value 3", Reply::Value(Ok(None))),
        step(
            "actions 3",
            Reply::Actions(Read::Known(if nested {
                vec!["AXPress".into()]
            } else {
                vec![]
            })),
        ),
    ];
    if nested {
        script.extend([
            step(
                &format!("batch 3 {CONTROL}"),
                Reply::Batch(Ok(control_batch())),
            ),
            step("snapshot 3", Reply::Unit),
        ]);
    } else {
        script.push(step(
            &format!("batch 3 {FRAME}"),
            Reply::Batch(Ok(frame_batch())),
        ));
    }
    script.push(step(
        "children 3",
        Reply::Children(Read::Known(node3_children.clone())),
    ));
    script.extend([
        step("timeout 4", Reply::Unit),
        step(
            "text 4 AXRole",
            Reply::Text(Read::Known("AXStaticText".into())),
        ),
        step(
            &format!("batch 4 {CONTENT}"),
            Reply::Batch(Ok(content(None))),
        ),
        step("value 4", str_value(if nested { "Inner" } else { "Alpha" })),
        step("actions 4", Reply::Actions(Read::Known(vec![]))),
        step(&format!("batch 4 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
        step("children 4", Reply::Children(Read::Absent)),
    ]);
    if two_texts {
        script.extend([
            step("timeout 5", Reply::Unit),
            step(
                "text 5 AXRole",
                Reply::Text(Read::Known("AXStaticText".into())),
            ),
            step(
                &format!("batch 5 {CONTENT}"),
                Reply::Batch(Ok(content(None))),
            ),
            step("value 5", str_value("Beta")),
            step("actions 5", Reply::Actions(Read::Known(vec![]))),
            step(&format!("batch 5 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
            step("children 5", Reply::Children(Read::Absent)),
        ]);
    }
    let children3: &[usize] = if two_texts { &[4, 5] } else { &[4] };
    let source = Fake::new(
        &[
            (1, 1, 1, &[2]),
            (2, 2, 2, &[3]),
            (3, 3, 3, children3),
            (4, 4, 4, &[]),
            (5, 5, 5, &[]),
        ],
        script,
    );
    let root = source.handle(1);
    let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(100), true);
    drop(root);
    source.assert_consumed();
    if nested {
        source.assert_lifetimes(&[(2, 1), (3, 1)]);
    } else {
        source.assert_lifetimes(&[(2, 1)]);
    }
    (result, source.trace())
}

// Baseline 36279fdc: a flat selectable row and its direct cell both claim
// the contained text after traversal, with cell and text still displayed.
#[test]
fn flat_row_and_direct_cell_keep_owned_text() {
    let (result, trace) = row_fixture(false, false);
    assert!(!result.nodes_skipped && !result.truncated);
    assert_eq!(result.tree_markdown, "- AXTable \"Items\"\n  - [0] AXRow\n    - AXCell \"Cell\"\n      - AXStaticText = \"Alpha\"\n");
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
        "AXNode { owned_row_label: None, element_index: None, role: \"AXTable\", title: Some(\"Items\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: Some(Some(\"Alpha\")), element_index: Some(0), role: \"AXRow\", title: None, value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 2, depth: 1, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: Some(Some(\"Alpha\")), element_index: None, role: \"AXCell\", title: Some(\"Cell\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 3, depth: 2, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: None, role: \"AXStaticText\", title: None, value: Some(\"Alpha\"), description: None, identifier: None, help: None, actions: [], element_ptr: 4, depth: 3, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]);
    assert_eq!(trace, ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "adopt 2", "timeout 2", "text 2 AXRole", "batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 2", "actions 2", "selected_settable 2", "batch 2 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 2", "children 2", "adopt 3", "timeout 3", "text 3 AXRole", "batch 3 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 3", "actions 3", "batch 3 AXPosition,AXSize", "children 3", "adopt 4", "timeout 4", "text 4 AXRole", "batch 4 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 4", "actions 4", "batch 4 AXPosition,AXSize", "children 4", "release 4", "release 3", "release 2", "release 1"]);
}

// Baseline 36279fdc: a nested outline row opens its own label path; its text
// must not be claimed by the enclosing selectable row.
#[test]
fn nested_outline_row_does_not_label_outer_row() {
    let (result, trace) = row_fixture(true, false);
    assert!(!result.nodes_skipped && !result.truncated);
    assert_eq!(result.tree_markdown, "- AXOutline \"Items\"\n  - [0] AXRow\n    - [1] AXRow [actions=[press]]\n      - AXStaticText = \"Inner\"\n");
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
        "AXNode { owned_row_label: None, element_index: None, role: \"AXOutline\", title: Some(\"Items\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: Some(None), element_index: Some(0), role: \"AXRow\", title: None, value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 2, depth: 1, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: Some(Some(\"Inner\")), element_index: Some(1), role: \"AXRow\", title: None, value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 3, depth: 2, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: None, role: \"AXStaticText\", title: None, value: Some(\"Inner\"), description: None, identifier: None, help: None, actions: [], element_ptr: 4, depth: 3, parent_element_index: Some(1), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]);
    assert_eq!(trace, ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "adopt 2", "timeout 2", "text 2 AXRole", "batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 2", "actions 2", "selected_settable 2", "batch 2 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 2", "children 2", "adopt 3", "timeout 3", "text 3 AXRole", "batch 3 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 3", "actions 3", "batch 3 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 3", "children 3", "adopt 4", "timeout 4", "text 4 AXRole", "batch 4 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 4", "actions 4", "batch 4 AXPosition,AXSize", "children 4", "release 4", "release 3", "release 2", "release 1"]);
}

// Baseline 36279fdc: the folding sentinel protects the unindexed cell and
// both text children, then is removed before nodes are returned.
#[test]
fn unindexed_cell_survives_owned_item_folding() {
    let (result, trace) = row_fixture(false, true);
    assert!(!result.nodes_skipped && !result.truncated);
    assert_eq!(result.tree_markdown, "- AXTable \"Items\"\n  - [0] AXRow\n    - AXCell \"Cell\"\n      - AXStaticText = \"Alpha\"\n      - AXStaticText = \"Beta\"\n");
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
        "AXNode { owned_row_label: None, element_index: None, role: \"AXTable\", title: Some(\"Items\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: Some(Some(\"Alpha\")), element_index: Some(0), role: \"AXRow\", title: None, value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 2, depth: 1, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: Some(Some(\"Alpha\")), element_index: None, role: \"AXCell\", title: Some(\"Cell\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 3, depth: 2, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: None, role: \"AXStaticText\", title: None, value: Some(\"Alpha\"), description: None, identifier: None, help: None, actions: [], element_ptr: 4, depth: 3, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: None, role: \"AXStaticText\", title: None, value: Some(\"Beta\"), description: None, identifier: None, help: None, actions: [], element_ptr: 5, depth: 3, parent_element_index: Some(0), frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
    ]);
    assert_eq!(trace, ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "adopt 2", "timeout 2", "text 2 AXRole", "batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 2", "actions 2", "selected_settable 2", "batch 2 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", "snapshot 2", "children 2", "adopt 3", "timeout 3", "text 3 AXRole", "batch 3 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 3", "actions 3", "batch 3 AXPosition,AXSize", "children 3", "adopt 4", "adopt 5", "timeout 4", "text 4 AXRole", "batch 4 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 4", "actions 4", "batch 4 AXPosition,AXSize", "children 4", "release 4", "timeout 5", "text 5 AXRole", "batch 5 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 5", "actions 5", "batch 5 AXPosition,AXSize", "children 5", "release 5", "release 3", "release 2", "release 1"]);
}
// Baseline 36279fdc: a large generic container without visibility lists
// reads child frames, lazily reads its surface once, and summarizes all 50.
#[test]
fn large_container_reads_surface_clip_lazily_once() {
    let ids: Vec<usize> = (2..52).collect();
    let mut node_specs: Vec<(usize, usize, usize, &[usize])> = vec![(1, 1, 1, ids.as_slice())];
    node_specs.extend(ids.iter().map(|&id| (id, id, id, &[][..])));
    let mut script = vec![
        step("timeout 1", Reply::Unit),
        step("text 1 AXRole", Reply::Text(Read::Known("AXGroup".into()))),
        step("children 1", Reply::Children(Read::Known(ids.clone()))),
        step("text 1 AXTitle", Reply::Text(Read::Absent)),
        step("text 1 AXDescription", Reply::Text(Read::Absent)),
        step("text 1 AXRoleDescription", Reply::Text(Read::Absent)),
    ];
    for &id in &ids {
        script.extend([
            step(&format!("timeout {id}"), Reply::Unit),
            step(
                &format!("text {id} AXRole"),
                Reply::Text(Read::Known("AXButton".into())),
            ),
            step(
                &format!("text {id} AXRoleDescription"),
                Reply::Text(Read::Absent),
            ),
        ]);
    }
    for &id in &ids {
        script.push(step(&format!("timeout {id}"), Reply::Unit));
        script.push(step(
            &format!("batch {id} {FRAME}"),
            Reply::Batch(Ok(Batch(vec![
                Slot::Point(Some((100.0, 100.0))),
                Slot::Size(Some((1.0, 1.0))),
            ]))),
        ));
        if id == 2 {
            script.push(step("timeout 1", Reply::Unit));
            script.push(step(
                &format!("batch 1 {FRAME}"),
                Reply::Batch(Ok(Batch(vec![
                    Slot::Point(Some((0.0, 0.0))),
                    Slot::Size(Some((10.0, 10.0))),
                ]))),
            ));
        }
    }
    let source = Fake::new(&node_specs, script);
    let root = source.handle(1);
    let result = with_dense_rows_windowed(true, || {
        run_fake(&source, &root, None, 10, WalkBudget::nodes_only(100), true)
    });
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    assert!(result.nodes_skipped);
    assert!(!result.truncated);
    assert!(result.nodes.is_empty());
    assert_eq!(
        result.tree_markdown,
        "- (offscreen rows: 50; scroll the list, or pass query to search them)\n"
    );
    assert_eq!(
        result.offscreen_rows,
        [dense_rows::OffscreenRows {
            parent_index: None,
            first_position: 0,
            last_position: 49,
            count: 50
        }]
    );
    let trace = source.trace();
    assert_eq!(
        &trace[..4],
        ["adopt 1", "timeout 1", "text 1 AXRole", "children 1"]
    );
    assert_eq!(
        trace
            .iter()
            .filter(|line| *line == "batch 1 AXPosition,AXSize")
            .count(),
        1
    );
    assert_eq!(trace.iter().filter(|line| *line == "timeout 1").count(), 2);
    assert_eq!(
        trace
            .iter()
            .filter(|line| line.starts_with("retain "))
            .count(),
        50
    );
    assert_eq!(
        trace
            .iter()
            .filter(|line| line.starts_with("release "))
            .count(),
        101
    );
    assert_eq!(
        &trace[207..211],
        [
            "timeout 2",
            "batch 2 AXPosition,AXSize",
            "timeout 1",
            "batch 1 AXPosition,AXSize"
        ]
    );
    assert_eq!(
        &trace[trace.len() - 3..],
        ["release 51", "release 2", "release 1"]
    );
}

// Baseline 36279fdc: vertical then horizontal element/role/value reads are
// released immediately; an invalid scrollbar role suppresses its value read.
#[test]
pub(crate) fn scroll_area_preserves_position_labels_and_read_order() {
    for exhaustive in [false, true] {
        let mut attrs = content(Some("Scroll"));
        attrs.0[2] = Slot::Text(AttrStatus::Known("Pane".into()));
        attrs.0[3] = Slot::Text(AttrStatus::Known("main".into()));
        let mut script = vec![
            step("timeout 1", Reply::Unit),
            step(
                "text 1 AXRole",
                Reply::Text(Read::Known("AXScrollArea".into())),
            ),
            step(&format!("batch 1 {CONTENT}"), Reply::Batch(Ok(attrs))),
            step("value 1", str_value("own")),
            step(
                "element 1 AXVerticalScrollBar",
                Reply::Element(Read::Known(2)),
            ),
            step(
                "text 2 AXRole",
                Reply::Text(Read::Known("AXScrollBar".into())),
            ),
            step("number 2 AXValue", Reply::Number(Read::Known(0.25))),
            step(
                "element 1 AXHorizontalScrollBar",
                Reply::Element(Read::Known(3)),
            ),
            step(
                "text 3 AXRole",
                Reply::Text(Read::Known(
                    if exhaustive {
                        "AXButton"
                    } else {
                        "AXScrollBar"
                    }
                    .into(),
                )),
            ),
        ];
        if !exhaustive {
            script.push(step("number 3 AXValue", Reply::Number(Read::Known(0.75))));
        }
        script.push(step("snapshot 1", Reply::Unit));
        if !exhaustive {
            script.extend([
                step(
                    "text 1 AXDescription",
                    Reply::Text(Read::Known("Pane".into())),
                ),
                step(
                    "text 1 AXIdentifier",
                    Reply::Text(Read::Known("main".into())),
                ),
            ]);
        }
        script.extend([
            step(
                "actions 1",
                Reply::Actions(Read::Known(vec!["AXPress".into()])),
            ),
            step("position 1", Reply::Point(Read::Absent)),
            step("children 1", Reply::Children(Read::Absent)),
        ]);
        let source = Fake::new(&[(1, 1, 1, &[]), (2, 2, 2, &[]), (3, 3, 3, &[])], script);
        let root = source.handle(1);
        let result = with_exhaustive_nodes(exhaustive, || {
            run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true)
        });
        assert!(result.nodes[0].canonical.role_read);
        assert!(result.nodes[0].canonical.copied_value.is_none());
        drop(root);
        source.assert_consumed();
        source.assert_lifetimes(&[(1, 1)]);
        assert!(!result.nodes_skipped && !result.truncated);
        if exhaustive {
            assert_eq!(
                result.tree_markdown,
                "- [0] AXScrollArea \"Scroll\" = \"own\" (Pane) [id=main actions=[press]]\n"
            );
            assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), ["AXNode { owned_row_label: None, element_index: Some(0), role: \"AXScrollArea\", title: Some(\"Scroll\"), value: Some(\"own\"), description: Some(\"Pane\"), identifier: Some(\"main\"), help: None, actions: [\"AXPress\"], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: Some(\"0.250\"), value_description: None, min_value: Some(0.0), max_value: Some(1.0), enabled: None, selected: None, in_web_content: false }"]);
            assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "element 1 AXVerticalScrollBar", "adopt 2", "text 2 AXRole", "number 2 AXValue", "release 2", "element 1 AXHorizontalScrollBar", "adopt 3", "text 3 AXRole", "release 3", "snapshot 1", "actions 1", "position 1", "children 1", "release 1"]);
        } else {
            assert_eq!(result.tree_markdown, "- [0] AXScrollArea = \"scrolled 25% down, 75% right\" (Pane) [id=main actions=[press]]\n");
            assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), ["AXNode { owned_row_label: None, element_index: Some(0), role: \"AXScrollArea\", title: None, value: Some(\"scrolled 25% down, 75% right\"), description: Some(\"Pane\"), identifier: Some(\"main\"), help: None, actions: [\"AXPress\"], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: Some(\"0.250\"), value_description: None, min_value: Some(0.0), max_value: Some(1.0), enabled: None, selected: None, in_web_content: false }"]);
            assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "element 1 AXVerticalScrollBar", "adopt 2", "text 2 AXRole", "number 2 AXValue", "release 2", "element 1 AXHorizontalScrollBar", "adopt 3", "text 3 AXRole", "number 3 AXValue", "release 3", "snapshot 1", "text 1 AXDescription", "text 1 AXIdentifier", "actions 1", "position 1", "children 1", "release 1"]);
        }
    }
}
// Baseline 36279fdc: ten tabs are planned with their selection metadata,
// nine are omitted and the selected tab keeps its actionable index.
#[test]
fn tab_compaction_keeps_summary_indices_and_skip() {
    let ids: Vec<usize> = (2..12).collect();
    let mut nodes: Vec<(usize, usize, usize, &[usize])> = vec![(1, 1, 1, ids.as_slice())];
    nodes.extend(ids.iter().map(|&id| (id, id, id, &[][..])));
    let mut script = vec![
        step("timeout 1", Reply::Unit),
        step(
            "text 1 AXRole",
            Reply::Text(Read::Known("AXTabGroup".into())),
        ),
        step(
            &format!("batch 1 {CONTENT}"),
            Reply::Batch(Ok(content(Some("Tabs")))),
        ),
        step("value 1", Reply::Value(Ok(None))),
        step("actions 1", Reply::Actions(Read::Known(vec![]))),
        step(&format!("batch 1 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
        step("children 1", Reply::Children(Read::Known(ids.clone()))),
        step("text 1 AXTitle", Reply::Text(Read::Known("Tabs".into()))),
        step("text 1 AXDescription", Reply::Text(Read::Absent)),
        step("text 1 AXRoleDescription", Reply::Text(Read::Absent)),
    ];
    for &id in &ids {
        script.extend([
            step(&format!("timeout {id}"), Reply::Unit),
            step(
                &format!("text {id} AXRole"),
                Reply::Text(Read::Known("AXTab".into())),
            ),
            step(
                &format!("bool {id} AXSelected"),
                Reply::Bool(Read::Known(id == 5)),
            ),
            step(
                &format!("batch {id} {CONTENT}"),
                Reply::Batch(Ok(content(Some(if id == 5 { "Active" } else { "Other" })))),
            ),
            step(&format!("value {id}"), Reply::Value(Ok(None))),
        ]);
    }
    let mut selected_control = control_batch();
    selected_control.0[6] = Slot::Bool(Some(true));
    script.extend([
        step("timeout 5", Reply::Unit),
        step("text 5 AXRole", Reply::Text(Read::Known("AXTab".into()))),
        step(
            &format!("batch 5 {CONTENT}"),
            Reply::Batch(Ok(content(Some("Active")))),
        ),
        step("value 5", Reply::Value(Ok(None))),
        step(
            "actions 5",
            Reply::Actions(Read::Known(vec!["AXPress".into()])),
        ),
        step(
            &format!("batch 5 {CONTROL}"),
            Reply::Batch(Ok(selected_control)),
        ),
        step("snapshot 5", Reply::Unit),
        step("children 5", Reply::Children(Read::Absent)),
    ]);
    let source = Fake::new(&nodes, script);
    let root = source.handle(1);
    let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(100), true);
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[(5, 1)]);
    assert!(result.nodes_skipped);
    assert!(!result.truncated);
    assert_eq!(result.tree_markdown, "- AXTabGroup \"Tabs\"\n  - (9 inactive tabs omitted)\n  - [0] AXTab \"Active\" [actions=[press]]\n");
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), [
        "AXNode { owned_row_label: None, element_index: None, role: \"AXTabGroup\", title: Some(\"Tabs\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }",
        "AXNode { owned_row_label: None, element_index: Some(0), role: \"AXTab\", title: Some(\"Active\"), value: None, description: None, identifier: None, help: None, actions: [\"AXPress\"], element_ptr: 5, depth: 1, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: Some(true), in_web_content: false }",
    ]);
    let trace = source.trace();
    assert_eq!(&trace[..7], ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize"]);
    assert_eq!(
        trace
            .iter()
            .filter(|line| line.starts_with("bool ") && line.ends_with(" AXSelected"))
            .count(),
        10
    );
    assert_eq!(trace.iter().filter(|line| *line == "timeout 5").count(), 2);
    assert_eq!(
        trace
            .iter()
            .filter(|line| line.starts_with("release "))
            .count(),
        11
    );
    assert_eq!(
        &trace[trace.len() - 9..],
        [
            "children 5",
            "release 5",
            "release 6",
            "release 7",
            "release 8",
            "release 9",
            "release 10",
            "release 11",
            "release 1"
        ]
    );
}
// Baseline 36279fdc: nested traversal restores menu, exhaustive and dense
// modes and its skip ledger without leaking its skipped web subtree outward.
#[test]
fn nested_walks_restore_thread_local_state() {
    let enclosing = SkipLedger::start();
    note_skipped();
    let nested = || {
        assert!(MENUS_COLLAPSED.with(std::cell::Cell::get));
        assert!(!exhaustive_nodes());
        assert!(rows_windowed());
        assert!(!WALK_SKIPPED.with(std::cell::Cell::get));
        let inner = Fake::new(
            &[(2, 2, 2, &[])],
            vec![
                step("timeout 2", Reply::Unit),
                step(
                    "text 2 AXRole",
                    Reply::Text(Read::Known("AXWebArea".into())),
                ),
            ],
        );
        let inner_root = inner.handle(2);
        let result = with_menus_collapsed(false, || {
            with_exhaustive_nodes(true, || {
                with_dense_rows_windowed(false, || {
                    run_fake(
                        &inner,
                        &inner_root,
                        None,
                        10,
                        WalkBudget::nodes_only(10),
                        false,
                    )
                })
            })
        });
        drop(inner_root);
        inner.assert_consumed();
        inner.assert_lifetimes(&[]);
        assert!(result.nodes_skipped);
        assert!(!result.truncated);
        assert!(result.nodes.is_empty());
        assert_eq!(result.tree_markdown, "");
        assert_eq!(
            inner.trace(),
            ["adopt 2", "timeout 2", "text 2 AXRole", "release 2"]
        );
        assert!(MENUS_COLLAPSED.with(std::cell::Cell::get));
        assert!(!exhaustive_nodes());
        assert!(rows_windowed());
        assert!(!WALK_SKIPPED.with(std::cell::Cell::get));
    };
    let source = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("timeout 1", Reply::Nested(Box::new(nested))),
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
            step(
                &format!("batch 1 {CONTENT}"),
                Reply::Batch(Ok(content(Some("Window")))),
            ),
            step("value 1", Reply::Value(Ok(None))),
            step("actions 1", Reply::Actions(Read::Known(vec![]))),
            step(&format!("batch 1 {FRAME}"), Reply::Batch(Ok(frame_batch()))),
            step("children 1", Reply::Children(Read::Absent)),
        ],
    );
    let root = source.handle(1);
    let result = with_menus_collapsed(true, || {
        with_exhaustive_nodes(false, || {
            with_dense_rows_windowed(true, || {
                run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true)
            })
        })
    });
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    assert!(!result.nodes_skipped && !result.truncated);
    assert_eq!(result.tree_markdown, "- AXWindow \"Window\"\n");
    assert_eq!(result.nodes.iter().map(|node| format!("{node:?}")).collect::<Vec<_>>(), ["AXNode { owned_row_label: None, element_index: None, role: \"AXWindow\", title: Some(\"Window\"), value: None, description: None, identifier: None, help: None, actions: [], element_ptr: 1, depth: 0, parent_element_index: None, frame: None, value_state: None, value_description: None, min_value: None, max_value: None, enabled: None, selected: None, in_web_content: false }"]);
    assert_eq!(source.trace(), ["adopt 1", "timeout 1", "text 1 AXRole", "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", "value 1", "actions 1", "batch 1 AXPosition,AXSize", "children 1", "release 1"]);
    assert!(WALK_SKIPPED.with(std::cell::Cell::get));
    assert!(enclosing.finish());
}

// Shared fake-only production walker entry for canonical-output regression tests.
pub(crate) fn canonical_test_node(secure: bool, enabled: Option<bool>) -> (AXNode, Vec<String>) {
    let role = if secure { "AXTextField" } else { "AXButton" };
    let mut attrs = content(Some("Example"));
    if secure {
        attrs.0[5] = Slot::Text(AttrStatus::Known("AXSecureTextField".into()));
    }
    let mut controls = control_batch();
    controls.0[0] = Slot::Bool(enabled);
    let mut steps = vec![
        step("timeout 1", Reply::Unit),
        step("text 1 AXRole", Reply::Text(Read::Known(role.into()))),
        step(&format!("batch 1 {CONTENT}"), Reply::Batch(Ok(attrs))),
    ];
    if !secure {
        steps.push(step("value 1", Reply::Value(Ok(None))));
    }
    steps.push(step(
        "actions 1",
        Reply::Actions(Read::Known(vec!["AXPress".into()])),
    ));
    steps.push(step(
        &format!("batch 1 {CONTROL}"),
        Reply::Batch(Ok(controls)),
    ));
    steps.push(step("snapshot 1", Reply::Unit));
    steps.push(step("children 1", Reply::Children(Read::Absent)));
    let source = Fake::new(&[(1, 1, 1, &[])], steps);
    let root = source.handle(1);
    let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true);
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[(1, 1)]);
    (result.nodes.into_iter().next().unwrap(), source.trace())
}

/// Exercise the same post-walk collector used by the native output path.
pub(crate) fn canonical_metadata_fixture(
    role: &str,
    mode: &str,
) -> (super::super::vocabulary::CanonicalEvidence, Vec<String>) {
    use super::super::vocabulary::{collect_state_evidence, CanonicalEvidence};
    let names = "AXExpanded,AXRequired,AXInvalid,AXEditable,AXElementBusy,AXModal,AXMultiSelectable,AXPopupValue";
    let mut steps = Vec::new();
    if mode != "before" && mode != "reuse_late" {
        steps.push(step("optional_timeout 1 set", Reply::Unit));
        let reply = if mode == "failure" {
            Err(BatchFailure::WholeCall(kAXErrorCannotComplete))
        } else {
            let mut slots = vec![Slot::Bool(None); 8];
            if mode == "mixed" {
                slots[2] = Slot::MixedText("grammar".into());
                slots[7] = Slot::MixedText("menu".into());
            } else if mode == "numeric" {
                slots[0] = Slot::Number(Some(2.0));
                slots[1] = Slot::Number(Some(1.0));
            }
            Ok(Batch(slots))
        };
        steps.push(step(&format!("batch 1 {names}"), Reply::Batch(reply)));
        if role == "AXTextField" && mode != "late" && mode != "reuse" {
            steps.push(step("optional_timeout 1 set", Reply::Unit));
            steps.push(step("value_settable 1", Reply::Writable(false)));
        }
        steps.push(step("optional_timeout 1 reset", Reply::Unit));
    }
    let source = Fake::new(&[(1, 1, 1, &[])], steps);
    let root = source.handle(1);
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(1);
    let mut calls = 0;
    let mut evidence = CanonicalEvidence::default();
    if mode == "reuse" || mode == "reuse_late" {
        evidence.states.value_settable = super::super::vocabulary::Observation::Known(Settable::No);
        if mode == "reuse_late" {
            evidence.value_settable_observed_at = Some(deadline);
        }
    }
    collect_state_evidence(&source, &root, role, &mut evidence, deadline, || {
        calls += 1;
        if mode == "before"
            || mode == "reuse_late"
            || (mode == "late" && calls >= 3)
            || (mode == "late_settable" && calls >= 6)
        {
            deadline
        } else {
            start
        }
    });
    drop(root);
    source.assert_consumed();
    (evidence, source.trace())
}

pub(crate) fn canonical_walk_fixture(case: &str) -> (TreeWalkResult, Vec<String>) {
    use super::super::content_attrs::AttributeListing;
    let role = match case {
        "role_failure" => None,
        "role_unknown" => Some("AXUnknown"),
        "secure" | "classification_failure" | "display" => Some("AXTextField"),
        "u25" => Some("AXTextArea"),
        "placeholder" | "copied" => Some("AXCheckBox"),
        _ => Some("AXButton"),
    };
    let mut attrs = content(Some("Example"));
    match case {
        "secure" => attrs.0[5] = Slot::Text(AttrStatus::Known("AXSecureTextField".into())),
        "classification_failure" => {
            attrs.0[5] = Slot::Text(super::super::content_attrs::FAILED_TEST)
        }
        "placeholder" => attrs.0[1] = Slot::Text(AttrStatus::Known("1".into())),
        "u25" => {
            for i in [2, 3] {
                attrs.0[i] = Slot::Text(AttrStatus::Failed {
                    error: kAXErrorFailure,
                    listed: AttributeListing::Listed,
                });
            }
            attrs.0[6] = Slot::Text(AttrStatus::Known("text entry area".into()));
        }
        _ => (),
    }
    let role_reply = role.map_or(
        Read::Unknown(super::super::tree_source::ReadFailure::WrongType),
        |role| Read::Known(role.into()),
    );
    let mut steps = vec![
        step("timeout 1", Reply::Unit),
        step("text 1 AXRole", Reply::Text(role_reply)),
    ];
    if case == "u25" {
        steps.push(step(
            &format!("batch 1 {CONTENT}"),
            Reply::Batch(Err(BatchFailure::WholeCall(kAXErrorAttributeUnsupported))),
        ));
        for (index, name) in CONTENT.split(',').enumerate() {
            let Slot::Text(status) = attrs.0[index].clone() else {
                unreachable!()
            };
            steps.push(step(&format!("status 1 {name}"), Reply::Status(status)));
        }
    } else {
        steps.push(step(&format!("batch 1 {CONTENT}"), Reply::Batch(Ok(attrs))));
    }
    if !matches!(
        case,
        "role_failure" | "role_unknown" | "secure" | "classification_failure"
    ) {
        steps.push(step(
            "value 1",
            if case == "copied" {
                str_value("1")
            } else {
                Reply::Value(Ok(None))
            },
        ));
    }
    steps.push(step(
        "actions 1",
        Reply::Actions(Read::Known(vec!["AXPress".into()])),
    ));
    if case == "u25" {
        steps.extend([
            step("bool 1 AXEnabled", Reply::Bool(Read::Absent)),
            step("position 1", Reply::Point(Read::Absent)),
            step("text 1 AXValueDescription", Reply::Text(Read::Absent)),
            step("number 1 AXMinValue", Reply::Number(Read::Absent)),
            step("number 1 AXMaxValue", Reply::Number(Read::Absent)),
            step("bool 1 AXSelected", Reply::Bool(Read::Absent)),
        ]);
    } else {
        steps.push(step(
            &format!("batch 1 {CONTROL}"),
            Reply::Batch(Ok(control_batch())),
        ));
    }
    steps.push(step("snapshot 1", Reply::Unit));
    steps.push(step("children 1", Reply::Children(Read::Absent)));
    let source = Fake::new(&[(1, 1, 1, &[])], steps);
    let root = source.handle(1);
    let result = run_fake(&source, &root, None, 10, WalkBudget::nodes_only(10), true);
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[(1, 1)]);
    (result, source.trace())
}

pub(crate) fn canonical_folded_row_fixture() -> TreeWalkResult {
    row_fixture(false, false).0
}

pub(crate) fn canonical_deferred_fixture() -> TreeWalkResult {
    dense_case(false, false).0
}

/// Walker, post-walk collection and output tests share one node, without AX.
pub(crate) fn canonical_walk_and_collect() -> (AXNode, Vec<String>) {
    let (tree, _) = canonical_walk_fixture("button");
    let mut node = tree.nodes.into_iter().next().unwrap();
    let names = "AXExpanded,AXRequired,AXInvalid,AXEditable,AXElementBusy,AXModal,AXMultiSelectable,AXPopupValue";
    let mut slots = vec![Slot::Bool(None); 8];
    slots[1] = Slot::Bool(Some(true));
    let source = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("optional_timeout 1 set", Reply::Unit),
            step(&format!("batch 1 {names}"), Reply::Batch(Ok(Batch(slots)))),
            step("optional_timeout 1 reset", Reply::Unit),
        ],
    );
    let root = source.handle(1);
    let start = std::time::Instant::now();
    super::super::vocabulary::collect_node_state_evidence(
        &source,
        &root,
        &mut node,
        start + std::time::Duration::from_secs(1),
        || start,
    );
    drop(root);
    source.assert_consumed();
    (node, source.trace())
}
