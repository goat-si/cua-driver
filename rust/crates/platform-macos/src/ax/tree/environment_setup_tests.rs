use super::{create_application_with, AxWalkEnvironment};
use crate::ax::evidence::{EvidenceStatus, ReadSite};
use std::cell::RefCell;

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
