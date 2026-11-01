use super::tree::{
    apply_owned_label_claims, child_label_path, collect_label_claim, initial_owned_row_label,
    next_label_path, test_fold_and_filter, AXNode, LabelPath,
};
use crate::tools::{test_build_elements_array, test_build_observation_array};

struct FakeNode {
    role: &'static str,
    indexed: bool,
    text: Option<&'static str>,
    children: Vec<FakeNode>,
}

fn walk_fake(root: &FakeNode) -> Vec<AXNode> {
    walk_fake_mode(root, false)
}

fn walk_fake_mode(root: &FakeNode, exhaustive: bool) -> Vec<AXNode> {
    fn walk(
        node: &FakeNode,
        ptr: &mut usize,
        depth: usize,
        parent_role: Option<&str>,
        parent_path: LabelPath,
        nodes: &mut Vec<AXNode>,
        claims: &mut Vec<(usize, String)>,
        exhaustive: bool,
    ) {
        *ptr += 1;
        let id = *ptr;
        let path = next_label_path(parent_path, parent_role, Some(node.role));
        collect_label_claim(claims, path, node.role, [node.text, None, None]);
        let emit = node.indexed || node.text.is_some() || exhaustive;
        let child_path = if emit {
            let mut out = AXNode::for_test(node.indexed.then_some(id), id);
            out.role = node.role.into();
            out.depth = depth;
            out.value = node.text.map(str::to_owned);
            out.owned_row_label = initial_owned_row_label(node.role);
            nodes.push(out);
            child_label_path(path, node.role, parent_role, id)
        } else {
            child_label_path(path, node.role, parent_role, id)
        };
        for child in &node.children {
            walk(
                child,
                ptr,
                depth + 1,
                Some(node.role),
                child_path,
                nodes,
                claims,
                exhaustive,
            );
        }
    }
    let mut nodes = Vec::new();
    let mut claims = Vec::new();
    walk(
        root,
        &mut 0,
        0,
        None,
        LabelPath::Inactive,
        &mut nodes,
        &mut claims,
        exhaustive,
    );
    apply_owned_label_claims(&mut nodes, claims);
    test_fold_and_filter(nodes, "inner").0
}

fn item(role: &'static str, indexed: bool, children: Vec<FakeNode>) -> FakeNode {
    FakeNode {
        role,
        indexed,
        text: None,
        children,
    }
}

fn text(value: &'static str) -> FakeNode {
    FakeNode {
        role: "AXStaticText",
        indexed: false,
        text: Some(value),
        children: vec![],
    }
}

#[test]
fn row_and_direct_cell_both_get_the_text() {
    let nodes = walk_fake(&item(
        "AXRow",
        true,
        vec![item("AXCell", true, vec![text("owned")])],
    ));
    let entries = test_build_elements_array(&nodes, None);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["label"], "owned");
    assert_eq!(entries[1]["label"], "owned");
}

#[test]
fn unindexed_cell_still_labels_its_row() {
    let nodes = walk_fake(&item(
        "AXRow",
        true,
        vec![item("AXCell", false, vec![text("owned")])],
    ));
    assert_eq!(test_build_elements_array(&nodes, None)[0]["label"], "owned");
}

#[test]
fn nested_item_blocks_both() {
    let nodes = walk_fake(&item(
        "AXRow",
        true,
        vec![item(
            "AXCell",
            true,
            vec![item("AXListItem", false, vec![text("foreign")])],
        )],
    ));
    let entries = test_build_elements_array(&nodes, None);
    assert_ne!(entries[0]["label"], "foreign");
    assert_ne!(entries[1]["label"], "foreign");
}

#[test]
fn standalone_cell_keeps_its_text() {
    let nodes = walk_fake(&item("AXCell", true, vec![text("owned")]));
    assert_eq!(test_build_elements_array(&nodes, None)[0]["label"], "owned");
}

#[test]
fn nested_indexed_outline_row_keeps_its_text() {
    let nodes = walk_fake(&item(
        "AXRow",
        true,
        vec![item("AXOutlineRow", true, vec![text("owned")])],
    ));
    let entries = test_build_elements_array(&nodes, None);
    assert_ne!(entries[0]["label"], "owned");
    assert_eq!(entries[1]["label"], "owned");
}

#[test]
fn unindexed_cell_observation_keeps_label() {
    let nodes = walk_fake_mode(&item("AXCell", false, vec![text("owned")]), true);
    assert_eq!(test_build_observation_array(&nodes)[0]["label"], "owned");
}

#[test]
fn compaction_does_not_assign_foreign_text_to_an_unindexed_cell() {
    let nodes = walk_fake_mode(
        &item(
            "AXCell",
            false,
            vec![item("AXOutlineRow", false, vec![text("foreign")])],
        ),
        true,
    );
    let observed = test_build_observation_array(&nodes);
    assert_ne!(observed[0]["label"], "foreign");
}

#[test]
fn deferred_row_keeps_both_owners() {
    use super::dense_rows::DeferredRow;
    use super::snapshot::RetainedElement;
    let path = child_label_path(LabelPath::Open(10), "AXCell", Some("AXRow"), 11);
    let deferred = DeferredRow {
        anchor: 0,
        seq: 0,
        position: 0,
        collection: 0,
        element: unsafe { RetainedElement::retain(0) },
        depth: 0,
        parent_index: None,
        parent_role: "AXCell".into(),
        label_path: path,
        in_web_content: false,
        visible: true,
        walked: None,
    };
    let path = next_label_path(deferred.label_path, Some("AXCell"), Some("AXStaticText"));
    let mut claims = Vec::new();
    collect_label_claim(
        &mut claims,
        path,
        "AXStaticText",
        [Some("owned"), None, None],
    );
    assert_eq!(claims, [(11, "owned".into()), (10, "owned".into())]);
}

#[test]
fn an_existing_outer_row_does_not_borrow_an_inner_item_text() {
    let nodes = walk_fake(&FakeNode {
        role: "AXRow",
        indexed: true,
        text: None,
        children: vec![FakeNode {
            role: "AXOutlineRow",
            indexed: false,
            text: None,
            children: vec![FakeNode {
                role: "AXStaticText",
                indexed: false,
                text: Some("inner"),
                children: vec![],
            }],
        }],
    });
    let entries = test_build_elements_array(&nodes, None);
    assert_ne!(entries[0]["label"], "inner");
}
