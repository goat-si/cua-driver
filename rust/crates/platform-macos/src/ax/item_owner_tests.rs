use super::tree::{
    apply_owned_label_claims, child_label_path, collect_label_claim, initial_owned_row_label,
    next_label_path, test_fold_and_filter, AXNode, LabelPath,
};
use crate::tools::test_build_elements_array;

struct FakeNode {
    role: &'static str,
    indexed: bool,
    text: Option<&'static str>,
    children: Vec<FakeNode>,
}

fn walk_fake(root: &FakeNode) -> Vec<AXNode> {
    fn walk(
        node: &FakeNode,
        ptr: &mut usize,
        depth: usize,
        parent_role: Option<&str>,
        parent_path: LabelPath,
        nodes: &mut Vec<AXNode>,
        claims: &mut Vec<(usize, String)>,
    ) {
        *ptr += 1;
        let id = *ptr;
        let path = next_label_path(parent_path, parent_role, Some(node.role));
        collect_label_claim(claims, path, node.role, [node.text, None, None]);
        let emit = node.indexed || node.text.is_some();
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
    );
    apply_owned_label_claims(&mut nodes, claims);
    test_fold_and_filter(nodes, "inner").0
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
