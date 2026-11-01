use super::{create_application_with, AxWalkEnvironment};
use crate::ax::evidence::{EvidenceStatus, ReadSite};
use std::cell::RefCell;

#[test]
fn walker_element_borrow_preserves_credits_and_clone_ownership() {
    use super::walker_seam_tests::Fake;
    use super::WalkElementBorrow;
    use crate::ax::tree_source::AxTreeSource;

    let source = Fake::new(&[(7, 7, 7, &[])], vec![]);
    let owned = source.handle(7);
    source.with_walk_element(&source, 7, |source, borrowed| {
        assert_eq!(source.key(borrowed), 7);
        assert_eq!(source.trace(), ["adopt 7"]);
        let retained = borrowed.clone();
        assert_eq!(source.trace(), ["adopt 7", "retain 7"]);
        drop(retained);
    });
    assert_eq!(source.trace(), ["adopt 7", "retain 7", "release 7"]);
    drop(owned);
    assert_eq!(
        source.trace(),
        ["adopt 7", "retain 7", "release 7", "release 7"]
    );
    source.assert_lifetimes(&[]);
    source.assert_consumed();
}

struct Script {
    application: Option<usize>,
    trace: RefCell<Vec<&'static str>>,
}

impl AxWalkEnvironment for Script {
    type Application = usize;
    type Element = usize;
    fn create_application(&self, pid: i32) -> Option<usize> {
        assert_eq!(pid, 42);
        self.trace.borrow_mut().push("create");
        self.application
    }
    fn set_application_timeout(&self, application: &usize) {
        assert_eq!(*application, 7);
        self.trace.borrow_mut().push("timeout");
    }
    fn application_key(&self, application: &usize) -> usize {
        *application
    }
    fn children(
        &self,
        _: usize,
    ) -> crate::ax::evidence::Observed<crate::ax::tree_source::Read<Vec<usize>>> {
        panic!("unexpected children")
    }
    fn windows(
        &self,
        _: usize,
        _: i32,
        _: Option<u32>,
    ) -> crate::ax::evidence::Observed<Vec<usize>> {
        panic!("unexpected windows")
    }
    fn same(&self, _: usize, _: usize) -> bool {
        panic!("unexpected identity check")
    }
    fn release(&self, _: usize) {
        panic!("unexpected release")
    }
    fn retain(&self, _: usize) {
        panic!("unexpected retain")
    }
    fn element_key(&self, _: usize) -> usize {
        panic!("unexpected element key")
    }
    fn set_element_timeout(&self, _: usize) {
        panic!("unexpected element timeout")
    }
    fn window_id(&self, _: usize) -> Option<u32> {
        panic!("unexpected window id")
    }
    fn window_bounds(&self, _: u32) -> Option<[f64; 4]> {
        panic!("unexpected bounds")
    }
    fn hit(&self, _: i32, _: f64, _: f64) -> crate::ax::evidence::Observed<Option<usize>> {
        panic!("unexpected hit")
    }
    fn parent(&self, _: usize) -> Option<usize> {
        panic!("unexpected parent")
    }
    fn now(&self) -> std::time::Instant {
        panic!("unexpected clock")
    }
    fn enable(&self, _: i32, _: usize) -> crate::ax::evidence::Observed<()> {
        panic!("unexpected enablement")
    }
    fn scope_candidate(
        &self,
        _: usize,
    ) -> crate::ax::evidence::Observed<crate::ax::window_scope::TopLevelCandidate> {
        panic!("unexpected candidate")
    }
    fn window_owner(&self, _: i32, _: u32) -> crate::windows::WindowOwner {
        panic!("unexpected owner")
    }
    fn sheet_role(&self, _: usize) -> crate::ax::tree_source::Read<String> {
        panic!("unexpected sheet role")
    }
    fn sheet_children(
        &self,
        _: usize,
    ) -> crate::ax::evidence::Observed<crate::ax::tree_source::Read<Vec<usize>>> {
        panic!("unexpected sheet children")
    }
    fn is_still_launching(&self, _: i32) -> bool {
        panic!("unexpected launch query")
    }
    fn pause(&self, _: std::time::Duration) {
        panic!("unexpected pause")
    }
}

#[test]
fn application_setup_preserves_early_return_and_single_timeout() {
    for application in [None, Some(7)] {
        let script = Script {
            application,
            trace: RefCell::new(Vec::new()),
        };
        let observed = create_application_with(&script, 42);
        assert_eq!(observed.legacy, application);
        assert_eq!(
            *script.trace.borrow(),
            if application.is_some() {
                vec!["create", "timeout"]
            } else {
                vec!["create"]
            }
        );
        assert_eq!(observed.evidence.len(), 1);
        assert_eq!(observed.evidence[0].site, ReadSite::ApplicationRoot);
        assert_eq!(observed.evidence[0].owner, application);
        assert_eq!(
            observed.evidence[0].status,
            if application.is_some() {
                EvidenceStatus::Known
            } else {
                EvidenceStatus::Unavailable
            }
        );
    }
}

#[test]
fn root_resolution_retries_from_observations_without_extra_retain() {
    use super::environment_evidence_tests::{Environment, Scenario};
    use crate::ax::window_scope::WindowScope;
    use cua_driver_core::walk_budget::WalkBudget;

    let environment = Environment::new(Scenario::Launching);
    let scope = crate::ax::evidence::EvidenceScope::start();
    let (roots, walk, window_scope) = super::resolve_top_level_with(
        &environment,
        1,
        42,
        Some(99),
        &mut WalkBudget::new(200, 100),
    );
    assert_eq!(roots, [2]);
    assert_eq!(walk, [2]);
    assert_eq!(window_scope, Some(WindowScope::Matched));
    let observed = scope.finish(roots);
    super::release_environment(&environment, observed.legacy);
    environment.assert_released();
    let trace = environment.trace.borrow();
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.as_str() == "enable")
            .count(),
        2
    );
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.as_str() == "children")
            .count(),
        2
    );
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.as_str() == "owner")
            .count(),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.as_str() == "pause 50")
            .count(),
        1
    );
    assert!(!trace.iter().any(|entry| entry.starts_with("retain ")));
}

#[test]
fn sheet_scope_is_derived_from_observations_and_credits_transfer() {
    use super::environment_evidence_tests::{Environment, Scenario};
    use crate::ax::window_scope::WindowScope;
    use cua_driver_core::walk_budget::WalkBudget;

    let environment = Environment::new(Scenario::Sheet);
    let scope = crate::ax::evidence::EvidenceScope::start();
    let (roots, walk, window_scope) = super::resolve_top_level_with(
        &environment,
        1,
        42,
        Some(99),
        &mut WalkBudget::nodes_only(100),
    );
    assert_eq!(roots, [3]);
    assert_eq!(walk, [3]);
    assert_eq!(
        window_scope,
        Some(WindowScope::SheetOfWindow {
            parent_window_id: 55
        })
    );
    let observed = scope.finish(roots);
    assert!(
        observed
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::RecoverySheetRole
                && entry.owner == Some(3)
                && entry.status == EvidenceStatus::Known),
        "missing sheet role evidence"
    );
    super::release_environment(&environment, observed.legacy);
    environment.assert_released();
    let trace = environment.trace.borrow();
    assert!(
        trace.iter().position(|entry| entry == "sheet-children")
            < trace.iter().position(|entry| entry == "release 2")
    );
    assert!(!trace.iter().any(|entry| entry.starts_with("retain ")));
}
