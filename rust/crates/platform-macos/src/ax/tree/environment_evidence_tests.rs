use crate::ax::bindings::AccessibilityOptIn;
use crate::ax::enablement::{ensure_with, ProcessEnablement, Wait};
use crate::ax::evidence::{EvidenceStatus, ReadSite};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

#[test]
fn menu_discovery_preserves_failure_deadline_and_cap() {
    use super::environment_setup_tests::run_menu_case;
    use crate::ax::context_menus::MenuOwner;
    use crate::ax::evidence::EvidenceScope;
    use cua_driver_core::walk_budget::WalkStop;
    let frame = [0.0, 0.0, 40.0, 40.0];

    let (failed_list, trace) = run_menu_case(None, vec![]);
    assert_eq!(failed_list.legacy.walk.stop, Some(WalkStop::Timeout));
    assert!(failed_list
        .evidence
        .iter()
        .any(|e| e.site == ReadSite::MenuWindowList && e.status == EvidenceStatus::Unavailable));
    assert!(!trace.contains(&"menu-lookup"));

    let scope = EvidenceScope::start();
    assert!(
        super::menu_discovery_deadline(Instant::now(), false, Some(Duration::from_millis(1)))
            .is_none()
    );
    let deadline = scope.finish(());
    assert!(deadline
        .evidence
        .iter()
        .any(|e| e.site == ReadSite::MenuDiscoveryDeadline
            && e.status == EvidenceStatus::DeadlineExpired));

    let (capped, trace) = run_menu_case(Some(vec![frame; 5]), vec![None, None, None, None]);
    assert_eq!(capped.legacy.walk.stop, Some(WalkStop::Timeout));
    assert!(capped
        .evidence
        .iter()
        .any(|e| e.site == ReadSite::MenuWindowCap && e.status == EvidenceStatus::DiscoveryLimit));
    assert_eq!(
        capped
            .evidence
            .iter()
            .filter(
                |e| e.site == ReadSite::MenuResolution && e.status == EvidenceStatus::Unavailable
            )
            .count(),
        4
    );
    assert_eq!(
        trace
            .iter()
            .filter(|entry| **entry == "menu-lookup")
            .count(),
        4
    );

    let (uncertain, trace) = run_menu_case(Some(vec![frame]), vec![Some((3, Some(frame), None))]);
    assert_eq!(uncertain.legacy.walk.stop, Some(WalkStop::Timeout));
    assert!(uncertain
        .evidence
        .iter()
        .any(|e| e.site == ReadSite::MenuOwnership
            && e.owner == Some(3)
            && e.status == EvidenceStatus::Unavailable));
    assert!(trace.contains(&"release-menu"));

    let (missing_frame, trace) = run_menu_case(
        Some(vec![frame]),
        vec![Some((3, None, Some(MenuOwner::Window(99))))],
    );
    assert_eq!(missing_frame.legacy.walk.stop, Some(WalkStop::Timeout));
    assert!(missing_frame
        .evidence
        .iter()
        .any(|e| e.site == ReadSite::MenuFrame
            && e.owner == Some(3)
            && e.status == EvidenceStatus::Unavailable));
    assert!(trace.contains(&"release-menu"));

    let (foreign, trace) = run_menu_case(
        Some(vec![frame]),
        vec![Some((3, Some(frame), Some(MenuOwner::Window(123))))],
    );
    assert_eq!(foreign.legacy.walk.stop, None);
    assert!(foreign
        .evidence
        .iter()
        .any(|e| e.site == ReadSite::MenuOwnership
            && e.owner == Some(3)
            && e.status == EvidenceStatus::Known));
    assert!(trace.contains(&"release-menu"));
}

#[derive(Clone, Copy)]
pub(super) enum Scenario {
    Union,
    Recovered,
    NoMatch,
    FailedHit,
    Sheet,
    Launching,
}

pub(super) struct Environment {
    scenario: Scenario,
    pub(super) trace: RefCell<Vec<String>>,
    credits: RefCell<std::collections::HashMap<usize, usize>>,
    now: Cell<Instant>,
    enumerations: Cell<usize>,
}

impl Environment {
    pub(super) fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            trace: RefCell::new(Vec::new()),
            credits: RefCell::new(std::collections::HashMap::new()),
            now: Cell::new(Instant::now()),
            enumerations: Cell::new(0),
        }
    }
    fn adopt(&self, id: usize) {
        *self.credits.borrow_mut().entry(id).or_default() += 1;
        self.trace.borrow_mut().push(format!("adopt {id}"));
    }
    pub(super) fn assert_released(&self) {
        assert!(self.credits.borrow().values().all(|credits| *credits == 0));
    }
}

impl super::AxWalkEnvironment for Environment {
    type Application = usize;
    type Element = usize;
    fn create_application(&self, _: i32) -> Option<usize> {
        panic!("unexpected application creation")
    }
    fn set_application_timeout(&self, _: &usize) {
        panic!("unexpected application timeout")
    }
    fn application_key(&self, application: &usize) -> usize {
        *application
    }
    fn children(
        &self,
        application: usize,
    ) -> crate::ax::evidence::Observed<crate::ax::tree_source::Read<Vec<usize>>> {
        use crate::ax::tree_source::Read;
        assert_eq!(application, 1);
        self.trace.borrow_mut().push("children".into());
        let count = self.enumerations.get();
        self.enumerations.set(count + 1);
        let elements = if matches!(self.scenario, Scenario::Union | Scenario::Sheet)
            || matches!(self.scenario, Scenario::Launching) && count > 0
        {
            vec![2]
        } else {
            vec![]
        };
        for &element in &elements {
            self.adopt(element);
        }
        crate::ax::evidence::observe_read(
            Some(application),
            ReadSite::Children,
            Read::Known(elements),
        )
    }
    fn windows(
        &self,
        application: usize,
        pid: i32,
        _: Option<u32>,
    ) -> crate::ax::evidence::Observed<Vec<usize>> {
        use crate::ax::evidence::{Observed, ReadEvidence};
        assert_eq!((application, pid), (1, 42));
        self.trace.borrow_mut().push("windows".into());
        let elements = if matches!(self.scenario, Scenario::Union) {
            vec![2, 3]
        } else {
            vec![]
        };
        for &element in &elements {
            self.adopt(element);
        }
        Observed {
            legacy: elements,
            evidence: vec![ReadEvidence {
                owner: Some(application),
                site: ReadSite::TopLevelWindows,
                status: EvidenceStatus::Known,
                attempt: 0,
                listing: None,
            }],
        }
    }
    fn same(&self, left: usize, right: usize) -> bool {
        self.trace.borrow_mut().push(format!("same {left} {right}"));
        left == right
    }
    fn release(&self, element: usize) {
        let mut credits = self.credits.borrow_mut();
        let count = credits.get_mut(&element).expect("known retained element");
        assert!(*count > 0);
        *count -= 1;
        self.trace.borrow_mut().push(format!("release {element}"));
    }
    fn retain(&self, element: usize) {
        self.adopt(element);
        self.trace.borrow_mut().push(format!("retain {element}"));
    }
    fn element_key(&self, element: usize) -> usize {
        element
    }
    fn set_element_timeout(&self, element: usize) {
        self.trace.borrow_mut().push(format!("timeout {element}"));
    }
    fn window_id(&self, element: usize) -> Option<u32> {
        self.trace.borrow_mut().push(format!("id {element}"));
        if matches!(self.scenario, Scenario::Sheet) {
            Some(if element == 2 { 55 } else { 99 })
        } else if matches!(self.scenario, Scenario::Recovered | Scenario::Launching) {
            Some(99)
        } else {
            Some(8)
        }
    }
    fn window_bounds(&self, id: u32) -> Option<[f64; 4]> {
        assert_eq!(id, 99);
        self.trace.borrow_mut().push("bounds".into());
        if matches!(self.scenario, Scenario::Sheet | Scenario::Launching) {
            None
        } else {
            Some([10.0, 20.0, 40.0, 60.0])
        }
    }
    fn hit(&self, pid: i32, x: f64, y: f64) -> crate::ax::evidence::Observed<Option<usize>> {
        assert_eq!((pid, x, y), (42, 30.0, 50.0));
        self.trace.borrow_mut().push("hit".into());
        if matches!(self.scenario, Scenario::FailedHit) {
            crate::ax::bindings::recovery_hit_outcome::<usize>(
                crate::ax::bindings::kAXErrorCannotComplete,
                None,
                None,
            )
        } else {
            self.adopt(10);
            crate::ax::bindings::recovery_hit_outcome(
                crate::ax::bindings::kAXErrorSuccess,
                Some(10),
                Some(10),
            )
        }
    }
    fn parent(&self, element: usize) -> Option<usize> {
        self.trace.borrow_mut().push(format!("parent {element}"));
        if element == 10 {
            self.adopt(11);
            Some(11)
        } else {
            None
        }
    }
    fn now(&self) -> Instant {
        self.trace.borrow_mut().push("now".into());
        self.now.get()
    }
    fn enable(&self, pid: i32, application: usize) -> crate::ax::evidence::Observed<()> {
        assert_eq!((pid, application), (42, 1));
        self.trace.borrow_mut().push("enable".into());
        crate::ax::evidence::Observed::legacy_only(())
    }
    fn scope_candidate(
        &self,
        element: usize,
    ) -> crate::ax::evidence::Observed<crate::ax::window_scope::TopLevelCandidate> {
        self.trace.borrow_mut().push(format!("candidate {element}"));
        let id = if matches!(self.scenario, Scenario::Sheet) {
            55
        } else {
            99
        };
        crate::ax::evidence::Observed::legacy_only(crate::ax::window_scope::TopLevelCandidate::new(
            "AXWindow",
            Some(id),
        ))
    }
    fn window_owner(&self, pid: i32, window_id: u32) -> crate::windows::WindowOwner {
        assert_eq!((pid, window_id), (42, 99));
        self.trace.borrow_mut().push("owner".into());
        crate::windows::WindowOwner::SamePid
    }
    fn sheet_role(&self, element: usize) -> crate::ax::tree_source::Read<String> {
        self.trace
            .borrow_mut()
            .push(format!("sheet-role {element}"));
        crate::ax::tree_source::Read::Known(
            if element == 2 { "AXWindow" } else { "AXSheet" }.into(),
        )
    }
    fn sheet_children(
        &self,
        element: usize,
    ) -> crate::ax::evidence::Observed<crate::ax::tree_source::Read<Vec<usize>>> {
        assert_eq!(element, 2);
        self.trace.borrow_mut().push("sheet-children".into());
        self.adopt(3);
        crate::ax::evidence::observe_read(
            Some(element),
            ReadSite::Children,
            crate::ax::tree_source::Read::Known(vec![3]),
        )
    }
    fn is_still_launching(&self, pid: i32) -> bool {
        assert_eq!(pid, 42);
        self.trace.borrow_mut().push("launching".into());
        matches!(self.scenario, Scenario::Launching)
    }
    fn pause(&self, duration: Duration) {
        self.trace
            .borrow_mut()
            .push(format!("pause {}", duration.as_millis()));
        self.now.set(self.now.get() + duration);
    }
}

#[test]
fn top_level_enumeration_preserves_unknown_and_empty() {
    use super::walker_seam_tests::{step, Fake, Reply};
    use crate::ax::bindings::{
        kAXErrorCannotComplete, kAXErrorNoValue, kAXErrorSuccess, window_list_status,
    };
    use crate::ax::evidence::{EvidenceScope, Observed, ReadEvidence};
    use crate::ax::tree_source::{AxTreeSource, Read, ReadFailure};

    assert_eq!(
        window_list_status(kAXErrorCannotComplete, false, false),
        EvidenceStatus::Unknown(ReadFailure::Ax(kAXErrorCannotComplete))
    );
    assert_eq!(
        window_list_status(kAXErrorNoValue, false, false),
        EvidenceStatus::Absent
    );
    assert_eq!(
        window_list_status(kAXErrorSuccess, false, false),
        EvidenceStatus::Absent
    );
    assert_eq!(
        window_list_status(kAXErrorSuccess, true, false),
        EvidenceStatus::Unknown(ReadFailure::WrongType)
    );
    assert_eq!(
        window_list_status(kAXErrorSuccess, true, true),
        EvidenceStatus::Known
    );

    for child_failure in [false, true] {
        let child_reply = if child_failure {
            Read::Unknown(ReadFailure::Ax(kAXErrorCannotComplete))
        } else {
            Read::Known(vec![])
        };
        let fake = Fake::new(
            &[(1, 1, 1, &[])],
            vec![step("children 1", Reply::Children(child_reply))],
        );
        let root = fake.handle(1);
        let scope = EvidenceScope::start();
        let ledger = super::SkipLedger::start();
        let children = fake.observed_children(&root);
        let union = super::top_level_union_with(
            children,
            || {
                assert_eq!(
                    super::WALK_SKIPPED.with(std::cell::Cell::get),
                    child_failure
                );
                Observed {
                    legacy: Vec::new(),
                    evidence: vec![ReadEvidence {
                        owner: Some(1),
                        site: ReadSite::TopLevelWindows,
                        status: EvidenceStatus::Known,
                        attempt: 0,
                        listing: None,
                    }],
                }
            },
            |_, _| panic!("no windows"),
            |_| panic!("no duplicate"),
        );
        assert!(union.is_empty());
        assert_eq!(ledger.finish(), child_failure);
        let observed = scope.finish(union);
        assert!(observed
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::Children
                && entry.owner == Some(1)
                && entry.status
                    == if child_failure {
                        EvidenceStatus::Unknown(ReadFailure::Ax(kAXErrorCannotComplete))
                    } else {
                        EvidenceStatus::Known
                    }));
        assert!(observed
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::TopLevelWindows
                && entry.status == EvidenceStatus::Known));
        fake.assert_consumed();
        drop(root);
        fake.assert_lifetimes(&[]);
    }

    for window_failure in [false, true] {
        let fake = Fake::new(
            &[(1, 1, 1, &[])],
            vec![step("children 1", Reply::Children(Read::Absent))],
        );
        let root = fake.handle(1);
        let scope = EvidenceScope::start();
        let ledger = super::SkipLedger::start();
        let children = fake.observed_children(&root);
        let status = if window_failure {
            EvidenceStatus::Unknown(ReadFailure::Ax(kAXErrorCannotComplete))
        } else {
            EvidenceStatus::Known
        };
        let union = super::top_level_union_with(
            children,
            || Observed {
                legacy: Vec::new(),
                evidence: vec![ReadEvidence {
                    owner: Some(1),
                    site: ReadSite::TopLevelWindows,
                    status,
                    attempt: 0,
                    listing: None,
                }],
            },
            |_, _| panic!("no windows"),
            |_| panic!("no duplicate"),
        );
        assert!(union.is_empty());
        assert!(!ledger.finish());
        let observed = scope.finish(union);
        assert!(observed
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::TopLevelWindows
                && entry.owner == Some(1)
                && entry.status == status));
        fake.assert_consumed();
        drop(root);
        fake.assert_lifetimes(&[]);
    }

    let fake = Fake::new(
        &[(1, 1, 1, &[2]), (2, 2, 2, &[]), (3, 3, 3, &[])],
        vec![step("children 1", Reply::Children(Read::Known(vec![2])))],
    );
    let root = fake.handle(1);
    let children = fake.observed_children(&root);
    let duplicate = fake.handle(2);
    let extra = fake.handle(3);
    let windows = || Observed::legacy_only(vec![duplicate, extra]);
    let union = super::top_level_union_with(
        children,
        windows,
        |left, right| fake.key(left) == fake.key(right),
        drop,
    );
    assert_eq!(
        union.iter().map(|item| fake.key(item)).collect::<Vec<_>>(),
        [2, 3]
    );
    fake.assert_consumed();
    drop(union);
    drop(root);
    fake.assert_lifetimes(&[]);

    let environment = Environment::new(Scenario::Union);
    let scope = EvidenceScope::start();
    let ledger = super::SkipLedger::start();
    let roots = super::top_level_with(&environment, 1, 42, None, None);
    assert_eq!(roots, [2, 3]);
    assert!(!ledger.finish());
    let observed = scope.finish(roots);
    assert!(observed
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::Children && entry.status == EvidenceStatus::Known));
    assert!(observed
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::TopLevelWindows
            && entry.status == EvidenceStatus::Known));
    super::release_environment(&environment, observed.legacy);
    environment.assert_released();
    assert_eq!(
        *environment.trace.borrow(),
        [
            "children",
            "adopt 2",
            "windows",
            "adopt 2",
            "adopt 3",
            "same 2 2",
            "release 2",
            "same 2 3",
            "release 2",
            "release 3"
        ]
    );
}

#[test]
fn recovery_probe_preserves_failure_and_no_match() {
    use crate::ax::bindings::{kAXErrorCannotComplete, kAXErrorSuccess, recovery_hit_outcome};
    use crate::ax::tree_source::ReadFailure;
    let failed = recovery_hit_outcome::<usize>(kAXErrorCannotComplete, None, None);
    assert!(failed.legacy.is_none());
    assert!(
        failed
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::RecoveryHit
                && entry.status
                    == EvidenceStatus::Unknown(ReadFailure::Ax(kAXErrorCannotComplete))),
        "missing failed hit evidence"
    );
    let absent = recovery_hit_outcome::<usize>(kAXErrorSuccess, None, None);
    assert!(absent.legacy.is_none());
    assert!(
        absent
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::RecoveryHit
                && entry.status == EvidenceStatus::Absent)
    );
    let found = recovery_hit_outcome(kAXErrorSuccess, Some(42usize), Some(42));
    assert_eq!(found.legacy, Some(42));
    assert!(found
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::RecoveryHit && entry.status == EvidenceStatus::Known));
    let now = Instant::now();
    let scope = crate::ax::evidence::EvidenceScope::start();
    let skipped: Option<()> =
        super::window_center_hit_if_live(Some(now), now, || panic!("no hit after deadline"));
    let observed = scope.finish(skipped);
    assert!(observed.legacy.is_none());
    assert!(observed
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::RecoveryDeadline
            && entry.status == EvidenceStatus::DeadlineExpired));
    for (ids, expected, status) in [
        (vec![Some(1), Some(2)], None, EvidenceStatus::Absent),
        (vec![Some(1), None], None, EvidenceStatus::Unavailable),
        (
            vec![Some(1), Some(42), Some(42)],
            Some(2),
            EvidenceStatus::Known,
        ),
    ] {
        let result = super::recovery_root_outcome(&ids, 42);
        assert_eq!(result.legacy, expected);
        assert!(result
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::RecoveryRoot && entry.status == status));
    }

    for (scenario, expected_roots, expected_site, expected_status) in [
        (
            Scenario::Recovered,
            vec![11],
            ReadSite::RecoveryRoot,
            EvidenceStatus::Known,
        ),
        (
            Scenario::NoMatch,
            vec![],
            ReadSite::RecoveryRoot,
            EvidenceStatus::Absent,
        ),
        (
            Scenario::FailedHit,
            vec![],
            ReadSite::RecoveryHit,
            EvidenceStatus::Unknown(ReadFailure::Ax(kAXErrorCannotComplete)),
        ),
    ] {
        let environment = Environment::new(scenario);
        let scope = crate::ax::evidence::EvidenceScope::start();
        let roots = super::top_level_with(&environment, 1, 42, Some(99), None);
        assert_eq!(roots, expected_roots);
        let observed = scope.finish(roots);
        assert!(observed
            .evidence
            .iter()
            .any(|entry| entry.site == expected_site && entry.status == expected_status));
        super::release_environment(&environment, observed.legacy);
        environment.assert_released();
        let trace = environment.trace.borrow();
        assert!(
            trace.iter().position(|event| event == "bounds")
                < trace.iter().position(|event| event == "hit")
        );
        if matches!(scenario, Scenario::FailedHit) {
            assert!(!trace.iter().any(|event| event.starts_with("parent ")));
        } else {
            assert!(
                trace.iter().position(|event| event == "parent 10")
                    < trace.iter().position(|event| event == "id 10")
            );
            assert_eq!(
                trace
                    .iter()
                    .filter(|event| event.as_str() == "retain 11")
                    .count(),
                usize::from(matches!(scenario, Scenario::Recovered))
            );
        }
    }
}

#[test]
fn scope_candidate_reads_preserve_unknown_and_absent() {
    use super::scope_candidate_with;
    use super::walker_seam_tests::{step, Fake, Reply};
    use crate::ax::tree_source::{Read, ReadFailure};
    use crate::ax::window_scope::{decide_window_scope, WindowScope};
    use crate::windows::WindowOwner;

    for name in ["AXRole", "AXSubrole", "AXIdentifier"] {
        for case in 0..4 {
            let make_read = |target: &str, known: &str| {
                if target != name {
                    return Read::Known(known.to_owned());
                }
                match case {
                    0 => {
                        Read::Unknown(ReadFailure::Ax(crate::ax::bindings::kAXErrorCannotComplete))
                    }
                    1 => Read::Known(known.to_owned()),
                    2 => Read::Absent,
                    _ => Read::Unknown(ReadFailure::WrongType),
                }
            };
            let fake = Fake::new(
                &[(1, 1, 1, &[])],
                vec![
                    step("timeout 1", Reply::Unit),
                    step(
                        "text 1 AXRole",
                        Reply::Text(make_read("AXRole", "AXWindow")),
                    ),
                    step(
                        "text 1 AXSubrole",
                        Reply::Text(make_read("AXSubrole", "ordinary")),
                    ),
                    step(
                        "text 1 AXIdentifier",
                        Reply::Text(make_read("AXIdentifier", "ordinary")),
                    ),
                ],
            );
            let element = fake.handle(1);
            let id_calls = Cell::new(0);
            let observed = scope_candidate_with(&fake, &element, || {
                id_calls.set(id_calls.get() + 1);
                Some(42)
            });
            let role_present = name != "AXRole" || case == 1;
            assert_eq!(id_calls.get(), usize::from(role_present));
            let decision = decide_window_scope(&[observed.legacy], 42, || WindowOwner::SamePid);
            if role_present {
                assert_eq!(decision.scope, WindowScope::Matched);
            } else {
                assert!(decision.walk.is_empty());
            }
            fake.assert_consumed();
            drop(element);
            fake.assert_lifetimes(&[]);
            let status = match case {
                0 => EvidenceStatus::Unknown(ReadFailure::Ax(
                    crate::ax::bindings::kAXErrorCannotComplete,
                )),
                1 => EvidenceStatus::Known,
                2 => EvidenceStatus::Absent,
                _ => EvidenceStatus::Unknown(ReadFailure::WrongType),
            };
            assert!(
                observed.evidence.iter().any(|entry| entry.owner == Some(1)
                    && entry.site == ReadSite::ScopeCandidate(name)
                    && entry.status == status),
                "missing candidate evidence for {name}"
            );
        }
    }
    let fake = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
            step("text 1 AXSubrole", Reply::Text(Read::Absent)),
            step("text 1 AXIdentifier", Reply::Text(Read::Absent)),
        ],
    );
    let element = fake.handle(1);
    let observed = scope_candidate_with(&fake, &element, || None);
    assert_eq!(observed.legacy.ax_window_id, None);
    fake.assert_consumed();
    drop(element);
    fake.assert_lifetimes(&[]);
    assert!(observed.evidence.iter().any(|entry| entry.owner == Some(1)
        && entry.site == ReadSite::ScopeCandidate("AXWindowId")
        && entry.status == EvidenceStatus::Unavailable));
}

#[test]
fn provider_enablement_preserves_failed_and_cached_outcomes() {
    let stamp = (10, 20);
    let now = Instant::now();
    let stored = Cell::new(None);
    let trace = RefCell::new(Vec::new());
    let failed = ensure_with(
        None,
        Some(stamp),
        now,
        || {
            trace.borrow_mut().push("opt-in");
            AccessibilityOptIn::ManualAccessibility
        },
        || {
            trace.borrow_mut().push("wait");
            false
        },
        |stamp, wait| {
            trace.borrow_mut().push("cache-write");
            stored.set(Some(ProcessEnablement { stamp, wait }));
        },
        Some(7),
    );
    assert_eq!(*trace.borrow(), ["opt-in", "wait", "cache-write"]);
    assert!(matches!(
        stored.get().expect("timeout cached").wait,
        Wait::TimedOut { attempts: 1, .. }
    ));
    let cached = ensure_with(
        stored.get(),
        Some(stamp),
        now + Duration::from_secs(1),
        || panic!("cached failure must not re-enable"),
        || panic!("cached failure must not wait"),
        |_, _| panic!("cached failure must not write"),
        Some(7),
    );
    assert!(
        failed.evidence.iter().any(|entry| entry.owner == Some(7)
            && entry.site == ReadSite::ProviderEnablement
            && entry.status == EvidenceStatus::Unavailable
            && entry.attempt == 1),
        "missing failed materialization evidence"
    );
    assert!(
        cached.evidence.iter().any(|entry| entry.owner == Some(7)
            && entry.site == ReadSite::ProviderCachedEnablement
            && entry.status == EvidenceStatus::Unavailable
            && entry.attempt == 1),
        "missing cached failure evidence"
    );

    let ready_cache = Cell::new(None);
    let ready = ensure_with(
        None,
        Some(stamp),
        now,
        || AccessibilityOptIn::EnhancedUserInterface,
        || true,
        |stamp, wait| ready_cache.set(Some(ProcessEnablement { stamp, wait })),
        Some(7),
    );
    assert!(ready
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::ProviderEnablement
            && entry.status == EvidenceStatus::Known
            && entry.owner == Some(7)));
    let cached_ready = ensure_with(
        ready_cache.get(),
        Some(stamp),
        now + Duration::from_secs(1),
        || panic!("cached ready must not re-enable"),
        || panic!("cached ready must not wait"),
        |_, _| panic!("cached ready must not write"),
        Some(7),
    );
    assert!(cached_ready
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::ProviderCachedEnablement
            && entry.status == EvidenceStatus::Known
            && entry.owner == Some(7)));
    let rejected = ensure_with(
        None,
        Some(stamp),
        now,
        || AccessibilityOptIn::NotAccepted,
        || panic!("rejected opt-in must not wait"),
        |_, _| panic!("rejected opt-in must not cache"),
        Some(7),
    );
    assert!(rejected
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::ProviderOptIn
            && entry.status == EvidenceStatus::Unavailable
            && entry.owner == Some(7)));
}
