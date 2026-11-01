use super::walker_seam_tests::{run_fake_observed, step, Fake, Reply};
use super::walker_test_support::{Batch, Slot};
use super::*;
use crate::ax::content_attrs::AttrStatus;
use serde_json::{json, Value};

fn depth_limited_walk(query: Option<&str>) -> Value {
    let source = Fake::new(
        &[(1, 1, 1, &[2]), (2, 2, 2, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
            step(
                "batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription",
                Reply::Batch(Ok(Batch(vec![Slot::Text(AttrStatus::Absent); 7]))),
            ),
            step("value 1", Reply::Value(Ok(None))),
            step("actions 1", Reply::Actions(Read::Known(vec![]))),
            step("batch 1 AXPosition,AXSize", Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)])))),
            step("children 1", Reply::Children(Read::Known(vec![2]))),
        ],
    );
    let root = source.handle(1);
    let tree = run_fake_observed(&source, &root, query, 0, WalkBudget::nodes_only(10), true).legacy;
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    let mut output = json!({});
    tree.walk.apply(&mut output);
    output
}

#[test]
fn depth_omission_reports_depth_limit() {
    let output = depth_limited_walk(None);
    assert!(output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"policy_omission", "detail":"depth_limit"})));
}

#[test]
fn query_with_an_omission_is_false_with_its_typed_reason() {
    let output = depth_limited_walk(Some("missing"));
    assert_eq!(output["observation_complete"], false);
    assert!(output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"policy_omission", "detail":"depth_limit"})));
}
