use super::walker_seam_tests::{run_fake_observed, step, Fake, Reply};
use super::walker_test_support::{Batch, Slot, SlotFailure};
use super::*;
use crate::ax::content_attrs::AttrStatus;
use serde_json::{json, Value};

fn reasons(tree: &TreeWalkResult) -> Value {
    let mut output = json!({});
    tree.walk.apply(&mut output);
    output["observation_incomplete_reasons"].clone()
}

fn has_reason(tree: &TreeWalkResult, kind: &str, detail: &str) -> bool {
    reasons(tree)
        .as_array()
        .unwrap()
        .contains(&json!({"kind": kind, "detail": detail}))
}

fn attribute_failure_after(
    entries: &[(
        Option<usize>,
        crate::ax::evidence::ReadSite,
        crate::ax::evidence::EvidenceStatus,
    )],
) -> bool {
    let scope = crate::ax::evidence::EvidenceScope::start();
    for &(owner, site, status) in entries {
        crate::ax::evidence::record_status(owner, site, status);
    }
    let mut budget = WalkBudget::nodes_only(10);
    record_attribute_failures(&mut budget);
    let mut output = json!({});
    budget.outcome().apply(&mut output);
    let failure = output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"}));
    let _ = scope.finish(());
    failure
}

fn assert_attribute_recovery(site: crate::ax::evidence::ReadSite) {
    use crate::ax::evidence::{EvidenceStatus as Status, ReadSite as Site};
    use crate::ax::tree_source::ReadFailure;
    let failed = Status::Unknown(ReadFailure::WrongType);
    assert!(
        !attribute_failure_after(&[(Some(1), site, failed), (Some(1), site, Status::Known)]),
        "same read should recover: {site:?}"
    );
    assert!(
        attribute_failure_after(&[(Some(1), site, failed), (Some(2), site, Status::Known)]),
        "other owner must not recover: {site:?}"
    );
    assert!(
        attribute_failure_after(&[
            (Some(1), site, failed),
            (Some(1), Site::Content("AXHelp"), Status::Known)
        ]),
        "different read must not recover: {site:?}"
    );
    assert!(
        attribute_failure_after(&[(Some(1), site, Status::Known), (Some(1), site, failed)]),
        "earlier success must not recover: {site:?}"
    );
    assert!(
        attribute_failure_after(&[(None, site, failed), (None, site, Status::Known)]),
        "unknown owner must not recover: {site:?}"
    );
    assert!(
        attribute_failure_after(&[
            (Some(1), site, failed),
            (Some(1), site, Status::Known),
            (Some(1), site, failed)
        ]),
        "later failure must remain: {site:?}"
    );
}

macro_rules! attribute_recovery_test {
    ($name:ident, $site:expr) => {
        #[test]
        fn $name() {
            assert_attribute_recovery($site);
        }
    };
}
attribute_recovery_test!(
    role_attribute_read_recovers_for_same_owner,
    crate::ax::evidence::ReadSite::Role
);
attribute_recovery_test!(
    content_attribute_read_recovers_for_same_owner,
    crate::ax::evidence::ReadSite::Content("AXTitle")
);
attribute_recovery_test!(
    control_attribute_read_recovers_for_same_owner,
    crate::ax::evidence::ReadSite::Control("AXSelected")
);
attribute_recovery_test!(
    frame_attribute_read_recovers_for_same_owner,
    crate::ax::evidence::ReadSite::Frame("AXPosition")
);
attribute_recovery_test!(
    scrollbar_attribute_read_recovers_for_same_owner,
    crate::ax::evidence::ReadSite::Scrollbar("AXValue")
);
attribute_recovery_test!(
    actions_read_recovers_for_same_owner,
    crate::ax::evidence::ReadSite::ActionNames
);
attribute_recovery_test!(
    value_settable_read_recovers_for_same_owner,
    crate::ax::evidence::ReadSite::ValueSettable
);
attribute_recovery_test!(
    selected_settable_read_recovers_for_same_owner,
    crate::ax::evidence::ReadSite::SelectedSettable
);
attribute_recovery_test!(
    menu_selection_read_recovers_for_same_owner,
    crate::ax::evidence::ReadSite::MenuSelected
);

#[test]
fn cross_site_attribute_recovery_preserves_keys() {
    use crate::ax::evidence::{EvidenceStatus as Status, ReadSite as Site};
    use crate::ax::tree_source::ReadFailure;
    let failed = Status::Unknown(ReadFailure::WrongType);
    for (failed_site, later_site) in [
        (Site::Scrollbar("AXRole"), Site::Role),
        (Site::Frame("AXPosition"), Site::Control("AXPosition")),
        (Site::MenuSelected, Site::Control("AXSelected")),
        (Site::Content("AXDescription"), Site::Tab("AXDescription")),
    ] {
        assert!(
            !attribute_failure_after(&[
                (Some(1), failed_site, failed),
                (Some(1), later_site, Status::Known)
            ]),
            "equivalent attribute should recover: {failed_site:?} -> {later_site:?}"
        );
    }
    assert!(attribute_failure_after(&[
        (Some(1), Site::ValueSettable, failed),
        (Some(1), Site::Content("AXValue"), Status::Known)
    ]));
    assert!(attribute_failure_after(&[
        (Some(1), Site::Role, failed),
        (Some(1), Site::Role, Status::Absent)
    ]));
    assert!(!attribute_failure_after(&[
        (Some(1), Site::Content("AXTitle"), failed),
        (Some(1), Site::Content("AXTitle"), Status::Absent)
    ]));

    // A composite frame or a settable probe is not a single scalar attribute.
    assert!(attribute_failure_after(&[
        (Some(1), Site::Frame("scroll"), failed),
        (Some(1), Site::Frame("AXPosition"), Status::Known)
    ]));
    assert!(attribute_failure_after(&[
        (Some(1), Site::ValueSettable, failed),
        (Some(1), Site::SelectedSettable, Status::Known)
    ]));

    // Reading a literal AXUnknown role through another probe does not recover
    // the earlier missing role; the actual source result supplies the status.
    let source = Fake::new(
        &[(1, 1, 1, &[])],
        vec![step(
            "text 1 AXRole",
            Reply::Text(Read::Known("AXUnknown".into())),
        )],
    );
    let root = source.handle(1);
    let scope = crate::ax::evidence::EvidenceScope::start();
    crate::ax::evidence::record_status(Some(1), Site::Role, failed);
    let wrapped = crate::ax::evidence_source::EvidenceSource::new(
        &source,
        crate::ax::evidence_source::Domain::Scrollbar,
        None,
    );
    assert!(matches!(wrapped.text(&root, "AXRole"), Read::Known(_)));
    wrapped.finish(()).into_walk_legacy();
    let mut budget = WalkBudget::nodes_only(10);
    record_attribute_failures(&mut budget);
    let mut output = json!({});
    budget.outcome().apply(&mut output);
    assert!(output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"})));
    let _ = scope.finish(());
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
}

#[test]
fn menu_frame_unavailable_reports_attribute_failure_and_scope_mismatch() {
    use crate::ax::context_menus::MenuOwner;
    let frame = [0.0, 0.0, 40.0, 40.0];
    let (walk, _) = super::environment_setup_tests::run_menu_case(
        Some(vec![frame]),
        vec![Some((3, None, Some(MenuOwner::Window(99))))],
    );
    assert!(has_reason(&walk.legacy, "unknown", "scope_mismatch"));
    assert!(has_reason(
        &walk.legacy,
        "provider_failure",
        "attribute_read_failed"
    ));
}

#[test]
fn menu_ownership_unavailable_reports_attribute_failure_and_scope_mismatch() {
    let frame = [0.0, 0.0, 40.0, 40.0];
    let (walk, _) = super::environment_setup_tests::run_menu_case(
        Some(vec![frame]),
        vec![Some((3, Some(frame), None))],
    );
    assert!(has_reason(&walk.legacy, "unknown", "scope_mismatch"));
    assert!(has_reason(
        &walk.legacy,
        "provider_failure",
        "attribute_read_failed"
    ));
    let (geometry_only, _) = super::environment_setup_tests::run_menu_case(
        Some(vec![frame]),
        vec![Some((
            3,
            Some([500.0, 500.0, 40.0, 40.0]),
            Some(crate::ax::context_menus::MenuOwner::Window(99)),
        ))],
    );
    assert!(has_reason(
        &geometry_only.legacy,
        "unknown",
        "scope_mismatch"
    ));
    assert!(!has_reason(
        &geometry_only.legacy,
        "provider_failure",
        "attribute_read_failed"
    ));
}

#[test]
fn verify_read_with_no_omission_is_complete() {
    use super::environment_evidence_tests::Scenario;
    let (walk, _, _) = with_exhaustive_assertion(true, || {
        with_exhaustive_nodes(true, || {
            super::orchestration_preservation_tests::run_scenario(
                Scenario::Launching,
                Some((2, "AXWindow")),
            )
        })
    });
    assert_eq!(walk.legacy.window_scope, Some(WindowScope::Matched));
    let mut output = json!({});
    walk.legacy.walk.apply(&mut output);
    assert_eq!(output["observation_complete"], true, "{output}");
    assert_eq!(output["observation_incomplete_reasons"], json!([]));
}

#[test]
fn ordinary_read_is_not_asserted() {
    use super::environment_evidence_tests::Scenario;
    let (walk, _, _) = super::orchestration_preservation_tests::run_scenario(
        Scenario::Launching,
        Some((2, "AXWindow")),
    );
    assert_eq!(
        reasons(&walk.legacy),
        json!([{"kind":"unknown", "detail":"not_asserted"}])
    );
}

#[test]
fn window_recovery_failure_never_asserts_complete() {
    use super::environment_evidence_tests::Scenario;
    for (scenario, root) in [
        (Scenario::NoMatch, None),
        (Scenario::ProviderFailure, Some((2, "AXWindow"))),
        (Scenario::CachedFailure, Some((2, "AXWindow"))),
    ] {
        let (walk, _, _) = with_exhaustive_assertion(true, || {
            with_exhaustive_nodes(true, || {
                super::orchestration_preservation_tests::run_scenario(scenario, root)
            })
        });
        let mut output = json!({});
        walk.legacy.walk.apply(&mut output);
        assert_eq!(output["observation_complete"], false, "{output}");
        assert!(output["observation_incomplete_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason != &json!({"kind":"unknown", "detail":"not_asserted"})));
    }
}

#[test]
fn missing_application_root_reports_enumeration_error() {
    let (walk, _) = super::environment_setup_tests::run_application_failure();
    assert!(has_reason(
        &walk.legacy,
        "provider_failure",
        "enumeration_error_unclassified"
    ));
}

#[test]
fn application_children_failure_is_not_hidden_by_recovered_window() {
    use super::environment_evidence_tests::Scenario;
    let (walk, _, _) = super::orchestration_preservation_tests::run_scenario(
        Scenario::ChildrenFailure,
        Some((2, "AXWindow")),
    );
    assert_eq!(walk.legacy.window_scope, Some(WindowScope::Matched));
    assert!(has_reason(
        &walk.legacy,
        "provider_failure",
        "child_enumeration_failed"
    ));
    let (recovered, _, _) = super::orchestration_preservation_tests::run_scenario(
        Scenario::ChildrenRecovered,
        Some((2, "AXWindow")),
    );
    assert!(!has_reason(
        &recovered.legacy,
        "provider_failure",
        "child_enumeration_failed"
    ));
}

#[test]
fn failed_window_enumeration_reports_enumeration_error() {
    use super::environment_evidence_tests::Scenario;
    let (walk, _, _) = super::orchestration_preservation_tests::run_scenario(
        Scenario::WindowsFailure,
        Some((2, "AXWindow")),
    );
    assert_eq!(walk.legacy.window_scope, Some(WindowScope::Matched));
    assert!(has_reason(
        &walk.legacy,
        "provider_failure",
        "enumeration_error_unclassified"
    ));
    let (recovered, _, _) = super::orchestration_preservation_tests::run_scenario(
        Scenario::WindowsRecovered,
        Some((2, "AXWindow")),
    );
    assert!(!has_reason(
        &recovered.legacy,
        "provider_failure",
        "enumeration_error_unclassified"
    ));
}

#[test]
fn unresolved_window_reports_scope_mismatch() {
    use super::environment_evidence_tests::Scenario;
    let (walk, _, _) =
        super::orchestration_preservation_tests::run_scenario(Scenario::NoMatch, None);
    assert!(has_reason(&walk.legacy, "unknown", "scope_mismatch"));
}

fn assert_scope_probe_failures(walk: &CompletedWalk) {
    use crate::ax::evidence::{EvidenceStatus, ReadSite};
    assert!(walk
        .completion
        .read_evidence
        .iter()
        .any(|entry| entry.owner == Some(3)
            && entry.site == ReadSite::ScopeCandidate("AXIdentifier")
            && matches!(entry.status, EvidenceStatus::Unknown(_))));
    assert!(walk
        .completion
        .read_evidence
        .iter()
        .any(|entry| entry.owner == Some(3)
            && entry.site == ReadSite::RecoverySheetRole
            && matches!(entry.status, EvidenceStatus::Unknown(_))));
}

#[test]
fn scope_candidate_failure_after_positive_match_is_not_incomplete() {
    use super::environment_evidence_tests::Scenario;
    let (walk, _, _) = super::orchestration_preservation_tests::run_scenario(
        Scenario::ScopeRecovered,
        Some((2, "AXWindow")),
    );
    assert_eq!(walk.legacy.window_scope, Some(WindowScope::Matched));
    assert_scope_probe_failures(&walk);
    assert!(!has_reason(
        &walk.legacy,
        "provider_failure",
        "attribute_read_failed"
    ));
}

#[test]
fn scope_candidate_failure_while_unresolved_reports_attribute_failure() {
    use super::environment_evidence_tests::Scenario;
    let (walk, _, _) =
        super::orchestration_preservation_tests::run_scenario(Scenario::ScopeUnresolved, None);
    assert!(matches!(
        walk.legacy.window_scope,
        Some(WindowScope::AxUnresolved { .. })
    ));
    assert_scope_probe_failures(&walk);
    assert!(has_reason(&walk.legacy, "unknown", "scope_mismatch"));
    assert!(has_reason(
        &walk.legacy,
        "provider_failure",
        "attribute_read_failed"
    ));
}

#[test]
fn launch_timeout_reports_time_budget() {
    use super::environment_evidence_tests::Scenario;
    let (walk, _, _) =
        super::orchestration_preservation_tests::run_scenario(Scenario::NeverLaunched, None);
    assert!(has_reason(&walk.legacy, "budget", "time_budget"));
}

#[test]
fn chromium_materialization_failure_reports_untrusted_provider() {
    use super::environment_evidence_tests::Scenario;
    for scenario in [Scenario::ProviderFailure, Scenario::CachedFailure] {
        let (walk, _, _) =
            super::orchestration_preservation_tests::run_scenario(scenario, Some((2, "AXWindow")));
        assert!(has_reason(
            &walk.legacy,
            "provider_failure",
            "untrusted_provider"
        ));
    }
    let (walk, _, _) = super::orchestration_preservation_tests::run_scenario(
        Scenario::OptInRejected,
        Some((2, "AXWindow")),
    );
    assert!(!has_reason(
        &walk.legacy,
        "provider_failure",
        "untrusted_provider"
    ));
    let (recovered, _, _) = super::orchestration_preservation_tests::run_scenario(
        Scenario::ProviderRecovered,
        Some((2, "AXWindow")),
    );
    assert!(!has_reason(
        &recovered.legacy,
        "provider_failure",
        "untrusted_provider"
    ));
}

#[test]
fn menu_window_list_failure_reports_enumeration_error() {
    let (walk, _) = super::environment_setup_tests::run_menu_case(None, vec![]);
    assert!(has_reason(
        &walk.legacy,
        "provider_failure",
        "enumeration_error_unclassified"
    ));
}

#[test]
fn menu_discovery_deadline_reports_time_budget() {
    let (walk, _) = super::environment_setup_tests::run_menu_deadline_case();
    assert!(has_reason(&walk.legacy, "budget", "time_budget"));
}

#[test]
fn menu_window_cap_reports_enumeration_error() {
    let frame = [0.0, 0.0, 40.0, 40.0];
    let (walk, _) =
        super::environment_setup_tests::run_menu_case(Some(vec![frame; 5]), vec![None; 4]);
    assert!(has_reason(
        &walk.legacy,
        "provider_failure",
        "enumeration_error_unclassified"
    ));
}

#[test]
fn unresolved_menu_reports_enumeration_error() {
    let frame = [0.0, 0.0, 40.0, 40.0];
    let (walk, _) = super::environment_setup_tests::run_menu_case(Some(vec![frame]), vec![None]);
    assert!(has_reason(
        &walk.legacy,
        "provider_failure",
        "enumeration_error_unclassified"
    ));
}

#[test]
fn unknown_menu_owner_reports_scope_mismatch() {
    use crate::ax::context_menus::MenuOwner;
    let frame = [0.0, 0.0, 40.0, 40.0];
    for reply in [
        Some((3, Some(frame), None)),
        Some((3, None, Some(MenuOwner::Window(99)))),
        Some((
            3,
            Some([500.0, 500.0, 40.0, 40.0]),
            Some(MenuOwner::Window(99)),
        )),
    ] {
        let (walk, _) =
            super::environment_setup_tests::run_menu_case(Some(vec![frame]), vec![reply]);
        assert!(has_reason(&walk.legacy, "unknown", "scope_mismatch"));
    }
}

#[test]
fn duplicate_or_foreign_menu_is_not_an_omission() {
    let (walk, _, _) = super::environment_setup_tests::run_published_menu_case();
    assert_eq!(
        reasons(&walk.legacy),
        json!([{"kind":"unknown", "detail":"not_asserted"}])
    );
}

#[test]
fn offscreen_deferred_rows_report_offscreen() {
    let (tree, _) = super::walker_seam_tests::dense_case_with_entry(false, false, false);
    assert!(has_reason(&tree, "policy_omission", "offscreen"));
    let (searched, _) = super::walker_seam_tests::dense_case_with_entry(true, false, false);
    assert!(!has_reason(&searched, "policy_omission", "offscreen"));
}

#[test]
fn geometry_clipped_rows_report_offscreen() {
    let tree = super::walker_seam_tests::large_container_fixture();
    assert!(has_reason(&tree, "policy_omission", "offscreen"));
}

fn refused_deferred_row(mut budget: WalkBudget) -> Value {
    let source = Fake::new(&[(1, 1, 1, &[])], vec![]);
    let row = dense_rows::DeferredRow {
        anchor: 0,
        seq: 0,
        position: 0,
        collection: 1,
        element: source.handle(1),
        depth: 0,
        parent_index: None,
        parent_role: "AXTable".into(),
        label_path: LabelPath::Inactive,
        in_web_content: false,
        visible: true,
        walked: None,
    };
    let mut counter = 0;
    let mut batching_disabled = false;
    let (lines, nodes, offscreen) = finish_deferred(
        &source,
        vec![],
        vec![],
        vec![row],
        None,
        &mut counter,
        &mut budget,
        &mut batching_disabled,
        10,
        true,
        vec![],
    );
    assert!(lines.is_empty() && nodes.is_empty() && offscreen.is_empty());
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    let mut output = json!({});
    budget.outcome().apply(&mut output);
    output
}

#[test]
fn expired_classification_reports_time_budget_not_offscreen() {
    let plans =
        dense_rows::plan_children(1, |_| panic!("expired row classification"), None, || true);
    assert_eq!(plans, [dense_rows::ChildPlan::Defer { visible: true }]);
    let output = refused_deferred_row(WalkBudget::new(0, 10));
    let reasons = output["observation_incomplete_reasons"].as_array().unwrap();
    assert!(reasons.contains(&json!({"kind":"budget", "detail":"time_budget"})));
    assert!(!reasons.contains(&json!({"kind":"policy_omission", "detail":"offscreen"})));
}

#[test]
fn deferred_budget_refusal_keeps_typed_reason() {
    let output = refused_deferred_row(WalkBudget::nodes_only(0));
    let reasons = output["observation_incomplete_reasons"].as_array().unwrap();
    assert!(reasons.contains(&json!({"kind":"budget", "detail":"node_budget"})));
    assert!(!reasons.contains(&json!({"kind":"policy_omission", "detail":"offscreen"})));
}

#[test]
fn unknown_visibility_falls_back_to_full_walk() {
    use crate::ax::tree_source::ReadFailure;
    let content =
        "AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription";
    let control = "AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected";
    let source = Fake::new(
        &[(1, 1, 1, &[2]), (2, 2, 2, &[])],
        vec![
            step("set 1 AXRows", Reply::Set(Read::Known(vec![2]))),
            step(
                "set 1 AXVisibleRows",
                Reply::Set(Read::Unknown(ReadFailure::WrongType)),
            ),
            step(
                "set 1 AXVisibleChildren",
                Reply::Set(Read::Unknown(ReadFailure::WrongType)),
            ),
            step("contains 2 [2]", Reply::Unit),
            step("timeout 2", Reply::Unit),
            step("text 2 AXRole", Reply::Text(Read::Known("AXRow".into()))),
            step(
                &format!("batch 2 {content}"),
                Reply::Batch(Ok(Batch(vec![Slot::Text(AttrStatus::Absent); 7]))),
            ),
            step("value 2", Reply::Value(Ok(None))),
            step(
                "actions 2",
                Reply::Actions(Read::Known(vec!["AXPress".into()])),
            ),
            step(
                &format!("batch 2 {control}"),
                Reply::Batch(Ok(Batch(vec![
                    Slot::Bool(None),
                    Slot::Point(None),
                    Slot::Size(None),
                    Slot::Text(AttrStatus::Absent),
                    Slot::Number(None),
                    Slot::Number(None),
                    Slot::Bool(None),
                ]))),
            ),
            step("snapshot 2", Reply::Unit),
            step("children 2", Reply::Children(Read::Absent)),
        ],
    );
    let parent = source.handle(1);
    let child = source.handle(2);
    let scope = crate::ax::evidence::EvidenceScope::start();
    let mut budget = WalkBudget::nodes_only(10);
    let plans = dense_rows::plan_collection_children(
        &source,
        &parent,
        &parent,
        "AXTable",
        std::slice::from_ref(&child),
        &budget,
    );
    assert_eq!(plans, [dense_rows::ChildPlan::Defer { visible: true }]);
    let row = dense_rows::DeferredRow {
        anchor: 0,
        seq: 0,
        position: 0,
        collection: 1,
        element: child,
        depth: 0,
        parent_index: None,
        parent_role: "AXTable".into(),
        label_path: LabelPath::Inactive,
        in_web_content: false,
        visible: true,
        walked: None,
    };
    let mut counter = 0;
    let (lines, nodes, offscreen) = finish_deferred(
        &source,
        vec![],
        vec![],
        vec![row],
        None,
        &mut counter,
        &mut budget,
        &mut false,
        10,
        true,
        vec![],
    );
    assert_eq!((lines.len(), nodes.len(), offscreen.len()), (1, 1, 0));
    record_attribute_failures(&mut budget);
    let mut output = json!({});
    budget.outcome().apply(&mut output);
    assert_eq!(
        output["observation_incomplete_reasons"],
        json!([{"kind":"unknown", "detail":"not_asserted"}])
    );
    let _ = scope.finish(());
    drop(parent);
    source.assert_consumed();
    source.assert_lifetimes(&[(2, 1)]);
}

#[test]
fn nested_walks_do_not_leak_completeness_state() {
    let nested = || {
        let source = Fake::new(
            &[(2, 2, 2, &[])],
            vec![
                step("timeout 2", Reply::Unit),
                step(
                    "text 2 AXRole",
                    Reply::Text(Read::Known("AXWebArea".into())),
                ),
            ],
        );
        let root = source.handle(2);
        let inner =
            run_fake_observed(&source, &root, None, 10, WalkBudget::nodes_only(10), false).legacy;
        drop(root);
        source.assert_consumed();
        source.assert_lifetimes(&[]);
        assert!(has_reason(&inner, "policy_omission", "web_content"));
    };
    let source = Fake::new(&[(1, 1, 1, &[])], vec![
        step("timeout 1", Reply::Nested(Box::new(nested))),
        step("timeout 1", Reply::Unit),
        step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
        step("batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", Reply::Batch(Ok(Batch(vec![Slot::Text(AttrStatus::Absent); 7])))),
        step("value 1", Reply::Value(Ok(None))),
        step("actions 1", Reply::Actions(Read::Known(vec![]))),
        step("batch 1 AXPosition,AXSize", Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)])))),
        step("children 1", Reply::Children(Read::Absent)),
    ]);
    let root = source.handle(1);
    let outer =
        run_fake_observed(&source, &root, None, 10, WalkBudget::nodes_only(10), true).legacy;
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    assert_eq!(
        reasons(&outer),
        json!([{"kind":"unknown", "detail":"not_asserted"}])
    );
}

#[test]
fn inactive_tab_compaction_reports_inactive_tab() {
    let tree = super::walker_seam_tests::tab_compaction_fixture();
    assert!(has_reason(&tree, "policy_omission", "inactive_tab"));
}

#[test]
fn web_content_exclusion_reports_web_content() {
    let source = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step(
                "text 1 AXRole",
                Reply::Text(Read::Known("AXWebArea".into())),
            ),
        ],
    );
    let root = source.handle(1);
    let tree =
        run_fake_observed(&source, &root, None, 10, WalkBudget::nodes_only(10), false).legacy;
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    assert!(has_reason(&tree, "policy_omission", "web_content"));
}

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

fn failed_child_walk(max_depth: usize) -> TreeWalkResult {
    use crate::ax::tree_source::ReadFailure;
    let source = Fake::new(&[(1, 1, 1, &[])], vec![
        step("timeout 1", Reply::Unit),
        step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
        step("batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", Reply::Batch(Ok(Batch(vec![Slot::Text(AttrStatus::Absent); 7])))),
        step("value 1", Reply::Value(Ok(None))),
        step("actions 1", Reply::Actions(Read::Known(vec![]))),
        step("batch 1 AXPosition,AXSize", Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)])))),
        step("children 1", Reply::Children(Read::Unknown(ReadFailure::WrongType))),
    ]);
    let root = source.handle(1);
    let tree = run_fake_observed(
        &source,
        &root,
        None,
        max_depth,
        WalkBudget::nodes_only(10),
        true,
    )
    .legacy;
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    tree
}

#[test]
fn failed_depth_probe_reports_child_enumeration_failed() {
    let tree = failed_child_walk(0);
    assert!(has_reason(
        &tree,
        "provider_failure",
        "child_enumeration_failed"
    ));
    assert!(!has_reason(&tree, "policy_omission", "depth_limit"));
}

#[test]
fn child_enumeration_failure_reports_child_enumeration_failed() {
    let tree = failed_child_walk(10);
    assert!(has_reason(
        &tree,
        "provider_failure",
        "child_enumeration_failed"
    ));
    let decoded =
        crate::ax::tree_source::native::decode_child_members([Some(7), None], |member| member);
    let observed = crate::ax::evidence::Observed {
        legacy: Read::Known(decoded.legacy),
        evidence: decoded.evidence,
    };
    let evidence = crate::ax::evidence::EvidenceScope::start();
    let mut budget = WalkBudget::nodes_only(10);
    assert!(
        matches!(record_children_observation(observed, &mut budget), Read::Known(children) if children == [7])
    );
    let mut output = json!({});
    budget.outcome().apply(&mut output);
    assert!(output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"provider_failure", "detail":"child_enumeration_failed"})));
    let _ = evidence.finish(());
}

#[test]
fn node_budget_refusal_reports_node_budget() {
    let source = Fake::new(&[(1, 1, 1, &[])], vec![]);
    let root = source.handle(1);
    let tree = run_fake_observed(&source, &root, None, 10, WalkBudget::nodes_only(0), true).legacy;
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    assert!(has_reason(&tree, "budget", "node_budget"));
}

#[test]
fn late_final_child_enumeration_reports_time_budget() {
    let walk = super::environment_setup_tests::run_late_final_enumeration();
    assert!(has_reason(&walk.legacy, "budget", "time_budget"));
    assert_eq!(walk.legacy.walk.stop, None);
    assert_eq!(walk.legacy.walk.nodes_visited, 1);
}

fn assert_mid_read_timeout(stage: &str) {
    use std::cell::Cell;
    use std::rc::Rc;
    let clock = Rc::new(Cell::new(super::scripted_clock_origin()));
    let expire_clock = clock.clone();
    let content = "batch 2 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription";
    let control =
        "batch 2 AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected";
    let frame = "batch 2 AXPosition,AXSize";
    let target = match stage {
        "content" => content,
        "control" => control,
        "frame" => frame,
        _ => panic!("unknown stage"),
    };
    let mut script = vec![
        step("timeout 2", Reply::Unit),
        step(
            "text 2 AXRole",
            Reply::Text(Read::Known(
                if stage == "control" {
                    "AXButton"
                } else {
                    "AXWindow"
                }
                .into(),
            )),
        ),
    ];
    let expire = step(
        target,
        Reply::Nested(Box::new(move || {
            expire_clock
                .set(super::scripted_clock_origin() + std::time::Duration::from_millis(2_000_000));
        })),
    );
    let mut expire = Some(expire);
    if stage == "content" {
        script.push(expire.take().unwrap());
    }
    script.push(step(
        content,
        Reply::Batch(Ok(Batch(vec![Slot::Text(AttrStatus::Absent); 7]))),
    ));
    script.push(step("value 2", Reply::Value(Ok(None))));
    script.push(step(
        "actions 2",
        Reply::Actions(Read::Known(if stage == "control" {
            vec!["AXPress".into()]
        } else {
            vec![]
        })),
    ));
    if stage == "control" {
        script.push(expire.take().unwrap());
        script.push(step(
            control,
            Reply::Batch(Ok(Batch(vec![
                Slot::Bool(None),
                Slot::Point(None),
                Slot::Size(None),
                Slot::Text(AttrStatus::Absent),
                Slot::Number(None),
                Slot::Number(None),
                Slot::Bool(None),
            ]))),
        ));
        script.push(step("snapshot 2", Reply::Unit));
    } else {
        if stage == "frame" {
            script.push(expire.take().unwrap());
        }
        script.push(step(
            frame,
            Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)]))),
        ));
    }
    script.push(step("children 2", Reply::Children(Read::Absent)));
    let source = Fake::new(&[(2, 2, 2, &[])], script);
    let walk = super::environment_setup_tests::run_timed_source(&source, clock);
    assert_eq!(
        walk.legacy.walk.stop,
        Some(cua_driver_core::walk_budget::WalkStop::Timeout),
        "stage={stage}"
    );
    assert!(has_reason(&walk.legacy, "budget", "time_budget"));
    assert_eq!(walk.legacy.nodes.len(), 1);
    assert!(walk.legacy.nodes[0].element_index.is_none());
    let trace = source.trace();
    assert!(trace.iter().any(|operation| operation == target));
    assert!(!trace.iter().any(|operation| operation == "children 2"));
    assert!(!trace.iter().any(|operation| operation == "snapshot 2"));
    if stage == "content" {
        assert!(!trace.iter().any(|operation| operation == "value 2"));
    }
    source.assert_lifetimes(&[]);
}

#[test]
fn content_timeout_reports_time_budget() {
    assert_mid_read_timeout("content");
}

#[test]
fn control_timeout_reports_time_budget() {
    assert_mid_read_timeout("control");
}

#[test]
fn frame_timeout_reports_time_budget() {
    assert_mid_read_timeout("frame");
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
    for case in ["role_failure", "role_unknown"] {
        let (tree, _) = super::walker_seam_tests::canonical_walk_fixture(case);
        let mut output = json!({});
        tree.walk.apply(&mut output);
        assert!(output["observation_incomplete_reasons"]
            .as_array()
            .unwrap()
            .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"})));
    }
}

#[test]
fn failed_actions_report_attribute_read_failed() {
    use crate::ax::tree_source::ReadFailure;
    let source = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
            step("batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", Reply::Batch(Ok(Batch(vec![Slot::Text(AttrStatus::Absent); 7])))),
            step("value 1", Reply::Value(Ok(None))),
            step("actions 1", Reply::Actions(Read::Unknown(ReadFailure::WrongType))),
            step("batch 1 AXPosition,AXSize", Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)])))),
            step("children 1", Reply::Children(Read::Absent)),
        ],
    );
    let root = source.handle(1);
    let tree = run_fake_observed(&source, &root, None, 10, WalkBudget::nodes_only(10), true).legacy;
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    let mut output = json!({});
    tree.walk.apply(&mut output);
    assert!(output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"})));
}

#[test]
fn failed_scrollbar_probe_reports_attribute_read_failed() {
    use crate::ax::tree_source::ReadFailure;
    let source = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXScrollArea".into()))),
            step("batch 1 AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription", Reply::Batch(Ok(Batch(vec![Slot::Text(AttrStatus::Absent); 7])))),
            step("value 1", Reply::Value(Ok(None))),
            step("element 1 AXVerticalScrollBar", Reply::Element(Read::Unknown(ReadFailure::WrongType))),
            step("element 1 AXHorizontalScrollBar", Reply::Element(Read::Absent)),
            step("children 1", Reply::Children(Read::Absent)),
        ],
    );
    let root = source.handle(1);
    let tree = run_fake_observed(&source, &root, None, 10, WalkBudget::nodes_only(10), true).legacy;
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    let mut output = json!({});
    tree.walk.apply(&mut output);
    assert!(output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"})));
}

#[test]
fn failed_tab_probe_reports_attribute_read_failed() {
    use crate::ax::tree_source::ReadFailure;
    let source = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step(
                "text 1 AXTitle",
                Reply::Text(Read::Unknown(ReadFailure::WrongType)),
            ),
            step("text 1 AXDescription", Reply::Text(Read::Absent)),
            step("text 1 AXRoleDescription", Reply::Text(Read::Absent)),
        ],
    );
    let root = source.handle(1);
    let evidence = crate::ax::evidence::EvidenceScope::start();
    let mut budget = WalkBudget::nodes_only(10);
    let plan =
        plan_macos_tab_strip_observed(&source, &root, "AXGroup", &[], &mut budget, &mut false)
            .into_walk_legacy();
    assert!(plan.is_none());
    record_attribute_failures(&mut budget);
    let mut output = json!({});
    budget.outcome().apply(&mut output);
    assert!(output["observation_incomplete_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"})));
    let _ = evidence.finish(());
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);

    // A failed child classification probe is recovered by a later full walk
    // only when the same child and attribute are successfully read again.
    for (recovered, expected_failure) in [(true, false), (false, true)] {
        let output = tab_role_probe_then_full_walk(recovered);
        assert_eq!(
            output["observation_incomplete_reasons"]
                .as_array()
                .unwrap()
                .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"})),
            expected_failure,
            "recovered={recovered}, output={output}"
        );
    }
    use crate::ax::evidence::{record_status, EvidenceStatus, ReadSite};
    for (probe, later_owner, later_site, later_status, expected_failure) in [
        (
            "AXRole",
            Some(2),
            ReadSite::Role,
            EvidenceStatus::Known,
            true,
        ),
        (
            "AXRole",
            Some(1),
            ReadSite::Content("AXTitle"),
            EvidenceStatus::Known,
            true,
        ),
        (
            "AXRole",
            Some(1),
            ReadSite::Role,
            EvidenceStatus::Absent,
            true,
        ),
        (
            "AXDescription",
            Some(1),
            ReadSite::Content("AXDescription"),
            EvidenceStatus::Absent,
            false,
        ),
        (
            "AXSelected",
            Some(1),
            ReadSite::Control("AXSelected"),
            EvidenceStatus::Known,
            false,
        ),
    ] {
        let scope = crate::ax::evidence::EvidenceScope::start();
        record_status(
            Some(1),
            ReadSite::Tab(probe),
            EvidenceStatus::Unknown(ReadFailure::WrongType),
        );
        record_status(later_owner, later_site, later_status);
        let mut budget = WalkBudget::nodes_only(10);
        record_attribute_failures(&mut budget);
        let mut output = json!({});
        budget.outcome().apply(&mut output);
        assert_eq!(
            output["observation_incomplete_reasons"]
                .as_array()
                .unwrap()
                .contains(&json!({"kind":"provider_failure", "detail":"attribute_read_failed"})),
            expected_failure,
            "probe={probe}, later={later_site:?}, owner={later_owner:?}"
        );
        let _ = scope.finish(());
    }
}

fn tab_role_probe_then_full_walk(recovered: bool) -> Value {
    use crate::ax::tree_source::ReadFailure;
    let ids: Vec<usize> = (2..12).collect();
    let mut nodes: Vec<(usize, usize, usize, &[usize])> = vec![(1, 1, 1, &ids)];
    nodes.extend(ids.iter().map(|&id| (id, id, id, &[][..])));
    let content =
        "AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription";
    let mut script = vec![
        step("timeout 1", Reply::Unit),
        step(
            "text 1 AXRole",
            Reply::Text(Read::Known("AXTabGroup".into())),
        ),
        step(
            &format!("batch 1 {content}"),
            Reply::Batch(Ok(Batch(vec![Slot::Text(AttrStatus::Absent); 7]))),
        ),
        step("value 1", Reply::Value(Ok(None))),
        step("actions 1", Reply::Actions(Read::Known(vec![]))),
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
                Reply::Text(if id == 2 {
                    Read::Unknown(ReadFailure::WrongType)
                } else {
                    Read::Known("AXStaticText".into())
                }),
            ),
            step(
                &format!("text {id} AXRoleDescription"),
                Reply::Text(Read::Absent),
            ),
        ]);
    }
    for &id in &ids {
        let role_failure = id == 2 && !recovered;
        script.extend([
            step(&format!("timeout {id}"), Reply::Unit),
            step(
                &format!("text {id} AXRole"),
                Reply::Text(if role_failure {
                    Read::Unknown(ReadFailure::WrongType)
                } else {
                    Read::Known("AXStaticText".into())
                }),
            ),
            step(
                &format!("batch {id} {content}"),
                Reply::Batch(Ok(Batch(vec![Slot::Text(AttrStatus::Absent); 7]))),
            ),
        ]);
        if !role_failure {
            script.push(step(&format!("value {id}"), Reply::Value(Ok(None))));
        }
        script.extend([
            step(
                &format!("actions {id}"),
                Reply::Actions(Read::Known(vec![])),
            ),
            step(&format!("children {id}"), Reply::Children(Read::Absent)),
        ]);
    }
    let source = Fake::new(&nodes, script);
    let root = source.handle(1);
    let tree =
        run_fake_observed(&source, &root, None, 10, WalkBudget::nodes_only(100), true).legacy;
    drop(root);
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    let mut output = json!({});
    tree.walk.apply(&mut output);
    output
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
