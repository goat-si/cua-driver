use super::{create_application_with, AxWalkEnvironment};
use crate::ax::evidence::{EvidenceStatus, ReadSite};
use std::cell::RefCell;

struct Script {
    application: Option<usize>,
    trace: RefCell<Vec<&'static str>>,
}

impl AxWalkEnvironment for Script {
    type Application = usize;
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
