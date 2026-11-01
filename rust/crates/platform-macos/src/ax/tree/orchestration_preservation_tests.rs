use super::environment_setup_tests::{
    run_application_failure, run_menu_case, run_menu_deadline_case, run_nested_menu_case,
    run_published_menu_case,
};
use super::*;
use cua_driver_core::walk_budget::WalkStop;
use serde_json::json;
use std::cell::RefCell;
use std::collections::HashMap;

struct ScenarioWalk<'a> {
    environment: super::environment_evidence_tests::Environment,
    source: &'a super::walker_seam_tests::Fake,
    credits: RefCell<HashMap<usize, Vec<super::walker_seam_tests::Handle>>>,
}

impl ScenarioWalk<'_> {
    fn adopt(&self, id: usize) {
        self.credits
            .borrow_mut()
            .entry(id)
            .or_default()
            .push(self.source.handle(id));
    }
}

impl AxWalkEnvironment for ScenarioWalk<'_> {
    type Application = usize;
    type Element = usize;
    fn create_application(&self, pid: i32) -> Option<usize> {
        assert_eq!(pid, 42);
        self.environment.trace.borrow_mut().push("create".into());
        Some(1)
    }
    fn set_application_timeout(&self, application: &usize) {
        assert_eq!(*application, 1);
        self.environment
            .trace
            .borrow_mut()
            .push("app-timeout".into());
    }
    fn application_key(&self, application: &usize) -> usize {
        *application
    }
    fn children(&self, application: usize) -> super::super::evidence::Observed<Read<Vec<usize>>> {
        let observed = self.environment.children(application);
        if let Read::Known(elements) = &observed.legacy {
            for &element in elements {
                self.adopt(element);
            }
        }
        observed
    }
    fn windows(
        &self,
        application: usize,
        pid: i32,
        window_id: Option<u32>,
    ) -> super::super::evidence::Observed<Vec<usize>> {
        let observed = self.environment.windows(application, pid, window_id);
        for &element in &observed.legacy {
            self.adopt(element);
        }
        observed
    }
    fn same(&self, left: usize, right: usize) -> bool {
        self.environment.same(left, right)
    }
    fn release(&self, element: usize) {
        self.environment.release(element);
        drop(
            self.credits
                .borrow_mut()
                .get_mut(&element)
                .expect("known credit")
                .pop()
                .expect("live credit"),
        );
    }
    fn retain(&self, element: usize) {
        self.environment.retain(element);
        let mut credits = self.credits.borrow_mut();
        let retained = credits
            .get(&element)
            .and_then(|items| items.last())
            .expect("live credit")
            .clone();
        credits.get_mut(&element).unwrap().push(retained);
    }
    fn element_key(&self, element: usize) -> usize {
        self.environment.element_key(element)
    }
    fn set_element_timeout(&self, element: usize) {
        self.environment.set_element_timeout(element)
    }
    fn window_id(&self, element: usize) -> Option<u32> {
        self.environment.window_id(element)
    }
    fn window_bounds(&self, window_id: u32) -> Option<[f64; 4]> {
        self.environment.window_bounds(window_id)
    }
    fn hit(&self, pid: i32, x: f64, y: f64) -> super::super::evidence::Observed<Option<usize>> {
        let observed = self.environment.hit(pid, x, y);
        if let Some(element) = observed.legacy {
            self.adopt(element);
        }
        observed
    }
    fn parent(&self, element: usize) -> Option<usize> {
        let parent = self.environment.parent(element);
        if let Some(element) = parent {
            self.adopt(element);
        }
        parent
    }
    fn now(&self) -> Instant {
        self.environment.now()
    }
    fn enable(&self, pid: i32, application: usize) -> super::super::evidence::Observed<()> {
        self.environment.enable(pid, application)
    }
    fn scope_candidate(
        &self,
        element: usize,
    ) -> super::super::evidence::Observed<TopLevelCandidate> {
        self.environment.scope_candidate(element)
    }
    fn window_owner(&self, pid: i32, window_id: u32) -> crate::windows::WindowOwner {
        self.environment.window_owner(pid, window_id)
    }
    fn sheet_role(&self, element: usize) -> Read<String> {
        self.environment.sheet_role(element)
    }
    fn sheet_children(&self, element: usize) -> super::super::evidence::Observed<Read<Vec<usize>>> {
        let observed = self.environment.sheet_children(element);
        if let Read::Known(elements) = &observed.legacy {
            for &element in elements {
                self.adopt(element);
            }
        }
        observed
    }
    fn is_still_launching(&self, pid: i32) -> bool {
        self.environment.is_still_launching(pid)
    }
    fn pause(&self, duration: std::time::Duration) {
        self.environment.pause(duration)
    }
}

impl WalkElementBorrow<super::walker_seam_tests::Fake> for ScenarioWalk<'_> {
    type Raw = usize;
    fn with_walk_element<R>(
        &self,
        source: &super::walker_seam_tests::Fake,
        raw: usize,
        f: impl FnOnce(&super::walker_seam_tests::Fake, &super::walker_seam_tests::Handle) -> R,
    ) -> R {
        assert!(std::ptr::eq(self.source, source));
        assert!(self
            .credits
            .borrow()
            .get(&raw)
            .is_some_and(|items| !items.is_empty()));
        source.with_walk_element(source, raw, f)
    }
}

impl WalkOrchestration<super::walker_seam_tests::Fake> for ScenarioWalk<'_> {
    fn release_application(&self, application: usize) {
        assert_eq!(application, 1);
        self.environment
            .trace
            .borrow_mut()
            .push("release-app".into());
    }
    fn menu_frames(&self, pid: i32) -> super::super::evidence::Observed<Option<Vec<[f64; 4]>>> {
        assert_eq!(pid, 42);
        self.environment.trace.borrow_mut().push("menu-list".into());
        menu_list_observed(Some(vec![]))
    }
    fn menu_in_window(
        &self,
        _: i32,
        _: [f64; 4],
        _: Instant,
    ) -> super::super::evidence::Observed<Option<MenuObservation<usize>>> {
        panic!("unexpected menu lookup")
    }
}

fn run_scenario(
    scenario: super::environment_evidence_tests::Scenario,
    root: Option<(usize, &str)>,
) -> (CompletedWalk, Vec<String>, Vec<String>) {
    use super::super::content_attrs::AttrStatus;
    use super::environment_evidence_tests::Environment;
    use super::walker_seam_tests::{step, Fake, Reply};
    use super::walker_test_support::{Batch, Slot};
    let mut steps = vec![];
    if let Some((id, role)) = root {
        let mut attrs = vec![Slot::Text(AttrStatus::Absent); 7];
        attrs[0] = Slot::Text(AttrStatus::Known("Window".into()));
        steps.extend([
            step(&format!("timeout {id}"), Reply::Unit),
            step(&format!("text {id} AXRole"), Reply::Text(Read::Known(role.into()))),
            step(&format!("batch {id} AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription"), Reply::Batch(Ok(Batch(attrs)))),
            step(&format!("value {id}"), Reply::Value(Ok(None))),
            step(&format!("actions {id}"), Reply::Actions(Read::Known(vec![]))),
            step(&format!("batch {id} AXPosition,AXSize"), Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)])))),
            step(&format!("children {id}"), Reply::Children(Read::Absent)),
        ]);
    }
    let source = Fake::new(
        &[
            (2, 2, 2, &[]),
            (3, 3, 3, &[]),
            (10, 10, 10, &[]),
            (11, 11, 11, &[]),
        ],
        steps,
    );
    let environment = ScenarioWalk {
        environment: Environment::new(scenario),
        source: &source,
        credits: RefCell::new(HashMap::new()),
    };
    let result = walk_with_environment(
        &source,
        &environment,
        WalkRequest {
            pid: 42,
            window_id: Some(99),
            query: None,
            max_depth: 10,
            include_web_content: true,
        },
        WalkBudget::new(200, 100),
    );
    assert!(environment.credits.borrow().values().all(Vec::is_empty));
    environment.environment.assert_released();
    source.assert_consumed();
    source.assert_lifetimes(&[]);
    let trace = environment.environment.trace.borrow().clone();
    let source_trace = source.trace();
    (result, trace, source_trace)
}

#[test]
fn production_root_creation_failure_follows_legacy_return() {
    let (observed, trace) = run_application_failure();
    let result = observed.legacy;
    assert_eq!(trace, ["create"]);
    assert_eq!(result.tree_markdown, "");
    assert!(result.nodes.is_empty());
    assert!(result.offscreen_rows.is_empty());
    assert_eq!(
        result.window_scope,
        Some(WindowScope::AxUnresolved { ax_window_count: 0 })
    );
    assert!(result.nodes_skipped);
    assert!(!result.truncated);
    assert_eq!(
        (
            result.walk.stop,
            result.walk.nodes_visited,
            result.walk.nodes_pending
        ),
        (None, 0, 0)
    );
    let mut fields = json!({});
    result.walk.apply(&mut fields);
    assert_eq!(fields["observation_complete"], false);
    assert_eq!(
        fields["observation_incomplete_reasons"],
        json!([{"kind":"unknown","detail":"not_asserted"}])
    );
    assert!(fields.get("observation_scope_exclusions").is_none());
}

#[test]
fn production_scope_is_computed_from_scripted_candidates() {
    let (observed, trace) = run_menu_case(Some(vec![]), vec![]);
    let result = observed.legacy;
    assert_eq!(result.window_scope, Some(WindowScope::Matched));
    assert_eq!(
        trace,
        [
            "create",
            "app-timeout",
            "now",
            "enable",
            "children",
            "windows",
            "element-timeout",
            "window-id",
            "candidate",
            "menu-list",
            "release-root",
            "release-app"
        ]
    );
    assert_eq!(result.tree_markdown, "- AXWindow \"Window\"\n");
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(result.nodes[0].role, "AXWindow");
    assert_eq!(result.nodes[0].title.as_deref(), Some("Window"));
    assert_eq!(result.nodes[0].depth, 0);
    assert_eq!(result.nodes[0].element_index, None);
    assert!(result.offscreen_rows.is_empty());
    assert!(!result.nodes_skipped);
    assert!(!result.truncated);
    assert_eq!(
        (
            result.walk.stop,
            result.walk.nodes_visited,
            result.walk.nodes_pending
        ),
        (None, 1, 0)
    );
    assert!(result.nodes.iter().all(|node| node.element_index.is_none()));
}

#[test]
fn production_unresolved_scope_follows_legacy_return() {
    let (observed, trace, source_trace) =
        run_scenario(super::environment_evidence_tests::Scenario::NoMatch, None);
    assert_eq!(
        observed.legacy.window_scope,
        Some(WindowScope::AxUnresolved { ax_window_count: 0 })
    );
    assert!(observed.legacy.nodes.is_empty());
    assert_eq!(observed.legacy.tree_markdown, "");
    assert!(!observed.legacy.truncated);
    assert!(!observed.legacy.nodes_skipped);
    assert_eq!(
        (
            observed.legacy.walk.nodes_visited,
            observed.legacy.walk.nodes_pending
        ),
        (0, 0)
    );
    assert_eq!(
        source_trace,
        ["adopt 10", "adopt 11", "release 10", "release 11"]
    );
    assert_eq!(
        trace.iter().filter(|item| item.as_str() == "owner").count(),
        1
    );
    assert!(trace.iter().position(|s| s == "hit") < trace.iter().position(|s| s == "owner"));
    assert_eq!(trace.last().map(String::as_str), Some("release-app"));
    assert!(!trace.iter().any(|s| s == "menu-list"));
}

#[test]
fn production_launch_retry_replays_the_same_requests() {
    let (observed, trace, source_trace) = run_scenario(
        super::environment_evidence_tests::Scenario::Launching,
        Some((2, "AXWindow")),
    );
    assert_eq!(observed.legacy.window_scope, Some(WindowScope::Matched));
    assert_eq!(observed.legacy.tree_markdown, "- AXWindow \"Window\"\n");
    assert_eq!(observed.legacy.nodes.len(), 1);
    assert_eq!(observed.legacy.nodes[0].element_index, None);
    assert!(!observed.legacy.truncated);
    assert!(!observed.legacy.nodes_skipped);
    assert_eq!(trace.iter().filter(|s| s.as_str() == "enable").count(), 2);
    assert_eq!(trace.iter().filter(|s| s.as_str() == "children").count(), 2);
    assert_eq!(trace.iter().filter(|s| s.as_str() == "windows").count(), 2);
    let first = trace.iter().position(|s| s == "enable").unwrap();
    let pause = trace.iter().position(|s| s == "pause 50").unwrap();
    let second = trace.iter().rposition(|s| s == "enable").unwrap();
    assert!(first < pause && pause < second);
    assert_eq!(source_trace.first().map(String::as_str), Some("adopt 2"));
    assert_eq!(source_trace.last().map(String::as_str), Some("release 2"));
    assert!(!source_trace.iter().any(|s| s.starts_with("retain ")));
}

#[test]
fn production_sheet_recovery_keeps_legacy_selection() {
    let (observed, trace, source_trace) = run_scenario(
        super::environment_evidence_tests::Scenario::Sheet,
        Some((3, "AXSheet")),
    );
    assert_eq!(
        observed.legacy.window_scope,
        Some(WindowScope::SheetOfWindow {
            parent_window_id: 55
        })
    );
    assert_eq!(observed.legacy.tree_markdown, "- AXSheet \"Window\"\n");
    assert_eq!(observed.legacy.nodes.len(), 1);
    assert_eq!(observed.legacy.nodes[0].role, "AXSheet");
    assert!(!observed.legacy.truncated);
    assert!(!observed.legacy.nodes_skipped);
    assert!(
        trace.iter().position(|s| s == "sheet-children")
            < trace.iter().position(|s| s == "release 2")
    );
    assert!(
        trace.iter().position(|s| s == "release 2") < trace.iter().position(|s| s == "release 3")
    );
    assert_eq!(source_trace.first().map(String::as_str), Some("adopt 2"));
    assert!(source_trace.iter().any(|s| s == "adopt 3"));
    assert_eq!(source_trace.last().map(String::as_str), Some("release 3"));
}

#[test]
fn production_menu_cap_keeps_legacy_timeout_behavior() {
    let frame = [0.0, 0.0, 40.0, 40.0];
    let (observed, trace) = run_menu_case(Some(vec![frame; 5]), vec![None, None, None, None]);
    assert_eq!(observed.legacy.window_scope, Some(WindowScope::Matched));
    assert_eq!(observed.legacy.walk.stop, Some(WalkStop::Timeout));
    assert!(observed.legacy.truncated);
    assert!(!observed.legacy.nodes_skipped);
    assert_eq!(observed.legacy.nodes.len(), 1);
    assert_eq!(
        trace.iter().filter(|item| **item == "menu-lookup").count(),
        4
    );
    assert_eq!(&trace[trace.len() - 2..], ["release-root", "release-app"]);

    let (deadline, trace) = run_menu_deadline_case();
    assert_eq!(deadline.legacy.walk.stop, Some(WalkStop::Timeout));
    assert_eq!(deadline.legacy.walk.nodes_visited, 1);
    assert_eq!(deadline.legacy.walk.nodes_pending, 0);
    assert_eq!(deadline.completion.final_deadline_sample, Some(true));
    assert!(trace.contains(&"menu-list"));
    assert!(!trace.contains(&"menu-lookup"));
    assert_eq!(&trace[trace.len() - 2..], ["release-root", "release-app"]);
}

#[test]
fn production_context_menu_discovery_keeps_legacy_order() {
    let (observed, trace, source_trace) = run_published_menu_case();
    assert_eq!(observed.legacy.window_scope, Some(WindowScope::Matched));
    assert_eq!(
        observed.legacy.tree_markdown,
        "- AXWindow \"Window\"\n- AXMenu \"Menu\"\n"
    );
    assert_eq!(
        observed
            .legacy
            .nodes
            .iter()
            .map(|node| node.role.as_str())
            .collect::<Vec<_>>(),
        ["AXWindow", "AXMenu"]
    );
    assert!(observed
        .legacy
        .nodes
        .iter()
        .all(|node| node.element_index.is_none()));
    assert_eq!(
        (
            observed.legacy.walk.nodes_visited,
            observed.legacy.walk.nodes_pending
        ),
        (2, 0)
    );
    assert!(!observed.legacy.truncated);
    assert!(!observed.legacy.nodes_skipped);
    assert_eq!(
        trace
            .iter()
            .filter(|entry| **entry == "menu-lookup")
            .count(),
        3
    );
    assert_eq!(
        &trace[trace.len() - 5..],
        [
            "release-root",
            "release-menu",
            "release-menu",
            "release-menu",
            "release-app"
        ]
    );
    assert!(
        source_trace.iter().position(|item| item == "children 2")
            < source_trace.iter().position(|item| item == "timeout 3")
    );
}

#[test]
fn production_duplicate_and_foreign_menus_keep_legacy_skip() {
    let (observed, _, source_trace) = run_published_menu_case();
    assert_eq!(observed.legacy.nodes.len(), 2);
    assert!(!observed.legacy.truncated);
    assert!(!observed.legacy.nodes_skipped);
    assert!(source_trace.iter().any(|item| item == "adopt 4"));
    assert!(source_trace.iter().any(|item| item == "adopt 5"));
    assert!(!source_trace
        .iter()
        .any(|item| item == "timeout 4" || item == "timeout 5"));
    assert_eq!(
        &source_trace[source_trace.len() - 4..],
        ["release 2", "release 3", "release 4", "release 5"]
    );
}

#[test]
fn production_finalizer_samples_deadline_without_enforcing_it() {
    let ledger = SkipLedger::start();
    let (result, sampled) = finalize_with_deadline_sample(
        vec![],
        vec![],
        vec![],
        None,
        WalkBudget::nodes_only(10),
        ledger,
        None,
        |_| true,
    );
    assert!(sampled);
    assert!(!result.truncated);
    assert!(!result.nodes_skipped);
    assert_eq!(result.walk.stop, None);
    assert_eq!(
        (result.walk.nodes_visited, result.walk.nodes_pending),
        (0, 0)
    );
    assert_eq!(result.tree_markdown, "");
    assert!(result.nodes.is_empty());
}

#[test]
fn production_finalizer_never_asserts_exhaustive_in_2a() {
    let (observed, _) = run_menu_case(Some(vec![]), vec![]);
    let mut payload = json!({});
    observed.legacy.walk.apply(&mut payload);
    assert_eq!(payload["observation_complete"], false);
    assert_eq!(
        payload["observation_incomplete_reasons"],
        json!([{"kind":"unknown","detail":"not_asserted"}])
    );
    assert_eq!(payload["truncated"], false);
    assert_eq!(payload["nodes_visited"], 1);
    assert_eq!(payload["nodes_pending"], 0);
    assert!(payload.get("truncation_reason").is_none());
    assert!(payload.get("observation_scope_exclusions").is_none());
}

#[test]
fn production_deferred_pass_keeps_order_and_ownership() {
    use super::walker_seam_tests::dense_case_with_entry;
    for search in [false, true] {
        let (baseline, baseline_trace) = dense_case_with_entry(search, false, false);
        let (shared, shared_trace) = dense_case_with_entry(search, false, true);
        assert_eq!(shared_trace, baseline_trace);
        assert_eq!(shared.tree_markdown, baseline.tree_markdown);
        assert_eq!(
            shared
                .nodes
                .iter()
                .map(|node| format!("{node:?}"))
                .collect::<Vec<_>>(),
            baseline
                .nodes
                .iter()
                .map(|node| format!("{node:?}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(shared.offscreen_rows, baseline.offscreen_rows);
        assert_eq!(shared.nodes_skipped, baseline.nodes_skipped);
        assert_eq!(shared.truncated, baseline.truncated);
        assert_eq!(shared.window_scope, baseline.window_scope);
        let mut old_fields = json!({});
        let mut new_fields = json!({});
        baseline.walk.apply(&mut old_fields);
        shared.walk.apply(&mut new_fields);
        let old_elapsed = old_fields["walk_elapsed_ms"].as_u64().unwrap();
        let new_elapsed = new_fields["walk_elapsed_ms"].as_u64().unwrap();
        assert!(old_elapsed < 2000 && new_elapsed < 2000);
        old_fields["walk_elapsed_ms"] = json!(new_elapsed);
        assert_eq!(new_fields, old_fields);
    }
}

#[test]
fn production_nested_orchestration_restores_legacy_state() {
    let (baseline, baseline_trace, baseline_source) = run_published_menu_case();
    let (nested, nested_trace, nested_source) = run_nested_menu_case();
    assert_eq!(nested_trace, baseline_trace);
    assert_eq!(nested_source, baseline_source);
    assert_eq!(nested.legacy.tree_markdown, baseline.legacy.tree_markdown);
    assert_eq!(
        nested
            .legacy
            .nodes
            .iter()
            .map(|node| format!("{node:?}"))
            .collect::<Vec<_>>(),
        baseline
            .legacy
            .nodes
            .iter()
            .map(|node| format!("{node:?}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(nested.legacy.window_scope, baseline.legacy.window_scope);
    assert_eq!(nested.legacy.nodes_skipped, baseline.legacy.nodes_skipped);
    assert_eq!(nested.legacy.truncated, baseline.legacy.truncated);
    assert_eq!(
        nested.completion.read_evidence,
        baseline.completion.read_evidence
    );
    assert_eq!(
        nested.completion.final_deadline_sample,
        baseline.completion.final_deadline_sample
    );
}
