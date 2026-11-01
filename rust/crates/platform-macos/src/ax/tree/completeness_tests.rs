use super::walker_seam_tests::{run_fake_observed, step, Fake, Reply};
use super::walker_test_support::{Batch, Slot, SlotFailure};
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

#[test]
fn failed_attribute_reports_attribute_read_failed() {
    let (tree, _) = super::walker_seam_tests::canonical_walk_fixture("u25");
    let mut output = json!({});
    tree.walk.apply(&mut output);
    assert!(output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"})));

    // The same shadowed failure must reach the shared production finalizer.
    let mut attrs = vec![Slot::Text(AttrStatus::Absent); 7];
    attrs[0] = Slot::Text(AttrStatus::Known("Window".into()));
    attrs[2] = Slot::Text(AttrStatus::WrongType);
    let source = Fake::new(
        &[(2, 2, 2, &[])],
        vec![
            step("timeout 2", Reply::Unit),
            step("text 2 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
            step("batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", Reply::Batch(Ok(Batch(attrs)))),
            step("actions 2", Reply::Actions(Read::Known(vec![]))),
            step("batch 2 AXPosition,AXSize", Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)])))),
            step("children 2", Reply::Children(Read::Absent)),
        ],
    );
    let observed = super::environment_setup_tests::run_unscoped_source(&source, 2, None);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    let mut output = json!({});
    observed.legacy.walk.apply(&mut output);
    assert!(output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"})));
}

#[test]
fn failed_control_attribute_reports_attribute_read_failed() {
    let source = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXButton".into()))),
            step("batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", Reply::Batch(Ok(Batch(vec![Slot::Text(AttrStatus::Absent); 7])))),
            step("value 1", Reply::Value(Ok(None))),
            step("actions 1", Reply::Actions(Read::Known(vec!["AXPress".into()]))),
            step("batch 1 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected", Reply::Batch(Ok(Batch(vec![Slot::Failure(SlotFailure::WrongType), Slot::Point(None), Slot::Size(None), Slot::Text(AttrStatus::Absent), Slot::Number(None), Slot::Number(None), Slot::Bool(None)])))),
            step("snapshot 1", Reply::Unit),
            step("children 1", Reply::Children(Read::Absent)),
        ],
    );
    let root = source.handle(1);
    let tree = run_fake_observed(&source, &root, None, 10, WalkBudget::nodes_only(10), true).legacy;
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[(1, 1)]);
    let mut output = json!({});
    tree.walk.apply(&mut output);
    assert!(output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"})));
}

#[test]
fn failed_role_reports_attribute_read_failed() {
    let (tree, _) = super::walker_seam_tests::canonical_walk_fixture("role_failure");
    let mut output = json!({});
    tree.walk.apply(&mut output);
    assert!(output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"})));
}

#[test]
fn recovered_batch_fallback_is_not_incomplete() {
    let (tree, trace) = super::walker_seam_tests::canonical_walk_fixture("recovered");
    assert!(trace.iter().any(|entry| entry == "status 1 AXTitle"));
    let mut output = json!({});
    tree.walk.apply(&mut output);
    assert_eq!(
        output["observation_incomplete_reasons"],
        json!([{"kind":"unknown", "detail":"not_asserted"}])
    );
}

#[test]
fn secure_redaction_without_read_failure_is_not_incomplete() {
    let (tree, trace) = super::walker_seam_tests::canonical_walk_fixture("secure");
    assert!(!trace.iter().any(|entry| entry == "value 1"));
    let mut output = json!({});
    tree.walk.apply(&mut output);
    assert_eq!(
        output["observation_incomplete_reasons"],
        json!([{"kind":"unknown", "detail":"not_asserted"}])
    );
}
