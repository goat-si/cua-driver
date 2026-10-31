//! Internal, transport-independent execution of pointer gesture plans.

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Placement {
    pub screen: (f64, f64),
    pub window_local: Option<(f64, f64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Move,
    PrimerDown,
    PrimerUp,
    Down,
    Dragged,
    Up,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Step {
    pub kind: Kind,
    pub placement: Placement,
    pub delay_ms: u64,
    pub phase: i64,
    pub click_state: i64,
}

#[derive(Clone, Copy)]
pub(super) enum RoutePolicy {
    Background,
    Window,
    Desktop,
}

#[derive(Clone, Copy)]
pub(super) enum Preparation {
    Warp,
    Associate,
    Wait40,
    Wait50,
}

pub(super) fn preparation(route: RoutePolicy, kind: Kind) -> &'static [Preparation] {
    use Preparation::*;
    match (route, kind) {
        (RoutePolicy::Window, Kind::Move) => &[Warp, Associate, Wait40],
        (RoutePolicy::Window, Kind::Dragged) => &[Warp],
        (RoutePolicy::Window, Kind::Up) => &[Wait50],
        _ => &[],
    }
}

pub(super) fn observes(route: RoutePolicy, kind: Kind) -> bool {
    !matches!(route, RoutePolicy::Desktop) || kind == Kind::Dragged
}

pub(super) trait GestureIo {
    type Event;
    fn prepare(&mut self, step: &Step);
    fn create(&mut self, step: &Step) -> anyhow::Result<Self::Event>;
    fn stamp(&mut self, event: &mut Self::Event, step: &Step);
    fn post(&mut self, event: &Self::Event, step: &Step);
    fn settle(&mut self, step: &Step);
    fn observe(&mut self, step: &Step);
}

#[derive(Debug, PartialEq)]
pub(super) enum GestureOutcome {
    Completed,
    StoppedEarly { at: Placement },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReleaseReceipt {
    HandedToTransport,
    NotArmed,
}

#[derive(Debug)]
pub(crate) struct GestureFailure {
    pub error: String,
    pub input_posted: bool,
    pub primer_receipt: ReleaseReceipt,
    pub button_receipt: ReleaseReceipt,
}

impl std::fmt::Display for GestureFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}
impl std::error::Error for GestureFailure {}

struct ButtonGuard<E> {
    release: E,
    step: Step,
    armed: bool,
}

impl<E> ButtonGuard<E> {
    fn release_now<I: GestureIo<Event = E>>(&mut self, io: &mut I) -> ReleaseReceipt {
        if !self.armed {
            return ReleaseReceipt::NotArmed;
        }
        self.armed = false;
        io.stamp(&mut self.release, &self.step);
        io.post(&self.release, &self.step);
        ReleaseReceipt::HandedToTransport
    }
}

pub(super) struct Prepared<E> {
    primer: Option<ButtonGuard<E>>,
    button: ButtonGuard<E>,
}

fn empty_failure(error: impl std::fmt::Display) -> GestureFailure {
    GestureFailure {
        error: error.to_string(),
        input_posted: false,
        primer_receipt: ReleaseReceipt::NotArmed,
        button_receipt: ReleaseReceipt::NotArmed,
    }
}

pub(super) fn prepare_gesture<I: GestureIo>(
    io: &mut I,
    plan: &[Step],
) -> Result<Prepared<I::Event>, GestureFailure> {
    let build = |io: &mut I, step: Step| -> Result<ButtonGuard<I::Event>, GestureFailure> {
        let release = io.create(&step).map_err(empty_failure)?;
        Ok(ButtonGuard {
            release,
            step,
            armed: false,
        })
    };
    let primer = plan
        .iter()
        .find(|s| s.kind == Kind::PrimerUp)
        .copied()
        .map(|step| build(io, step))
        .transpose()?;
    let up = plan
        .iter()
        .find(|s| s.kind == Kind::Up)
        .copied()
        .ok_or_else(|| empty_failure("gesture has no release step"))?;
    let button = build(io, up)?;
    Ok(Prepared { primer, button })
}

struct Running<'a, I: GestureIo> {
    io: &'a mut I,
    prepared: Prepared<I::Event>,
    input_posted: bool,
}

impl<I: GestureIo> Running<'_, I> {
    fn release(&mut self) -> (ReleaseReceipt, ReleaseReceipt) {
        let primer = self
            .prepared
            .primer
            .as_mut()
            .map_or(ReleaseReceipt::NotArmed, |guard| guard.release_now(self.io));
        let button = self.prepared.button.release_now(self.io);
        (primer, button)
    }
}
impl<I: GestureIo> Drop for Running<'_, I> {
    fn drop(&mut self) {
        self.release();
    }
}

pub(super) fn run_prepared<I: GestureIo>(
    io: &mut I,
    plan: Vec<Step>,
    prepared: Prepared<I::Event>,
    mut keep_going: impl FnMut() -> bool,
) -> Result<GestureOutcome, GestureFailure> {
    let mut running = Running {
        io,
        prepared,
        input_posted: false,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> anyhow::Result<GestureOutcome> {
            let mut stopped = None;
            for mut step in plan {
                if let Some(at) = stopped {
                    match step.kind {
                        Kind::Dragged => continue,
                        Kind::Up => step.placement = at,
                        _ => {}
                    }
                }
                running.io.prepare(&step);
                let mut event = running.io.create(&step)?;
                running.io.stamp(&mut event, &step);
                running.io.post(&event, &step);
                running.input_posted = true;
                match step.kind {
                    Kind::PrimerDown => {
                        if let Some(guard) = &mut running.prepared.primer {
                            guard.armed = true;
                            guard.step.placement = step.placement;
                        }
                    }
                    Kind::PrimerUp => {
                        if let Some(guard) = &mut running.prepared.primer {
                            guard.armed = false;
                        }
                    }
                    Kind::Down => {
                        running.prepared.button.armed = true;
                        running.prepared.button.step.placement = step.placement;
                    }
                    Kind::Dragged => running.prepared.button.step.placement = step.placement,
                    Kind::Up => running.prepared.button.armed = false,
                    Kind::Move => {}
                }
                if matches!(step.kind, Kind::Down | Kind::Dragged) {
                    running.io.observe(&step);
                }
                running.io.settle(&step);
                if step.kind == Kind::Dragged && stopped.is_none() && !keep_going() {
                    stopped = Some(step.placement);
                }
            }
            Ok(match stopped {
                Some(at) => GestureOutcome::StoppedEarly { at },
                None => GestureOutcome::Completed,
            })
        },
    ));
    match result {
        Ok(Ok(outcome)) => Ok(outcome),
        failure => {
            let error = match failure {
                Ok(Err(error)) => error.to_string(),
                Err(payload) => payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "gesture panicked".to_owned()),
                Ok(Ok(_)) => unreachable!(),
            };
            let (primer_receipt, button_receipt) = running.release();
            Err(GestureFailure {
                error,
                input_posted: running.input_posted,
                primer_receipt,
                button_receipt,
            })
        }
    }
}

pub(super) fn run_gesture<I: GestureIo>(
    io: &mut I,
    plan: Vec<Step>,
    keep_going: impl FnMut() -> bool,
) -> Result<GestureOutcome, GestureFailure> {
    let prepared = prepare_gesture(io, &plan)?;
    run_prepared(io, plan, prepared, keep_going)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::RoutePolicy as Route;
    struct Fake {
        route: Route,
        ops: Vec<String>,
        fail: Option<Kind>,
        fail_at: Option<usize>,
        creates: usize,
        panic_at: Option<Placement>,
        posted: Vec<(Kind, Placement)>,
    }
    impl Fake {
        fn new(route: Route) -> Self {
            Self {
                route,
                ops: vec![],
                fail: None,
                fail_at: None,
                creates: 0,
                panic_at: None,
                posted: vec![],
            }
        }
    }
    impl GestureIo for Fake {
        type Event = Kind;
        fn prepare(&mut self, step: &Step) {
            for op in preparation(self.route, step.kind) {
                self.ops.push(
                    match op {
                        Preparation::Warp => "warp",
                        Preparation::Associate => "associate",
                        Preparation::Wait40 => "wait40",
                        Preparation::Wait50 => "wait50",
                    }
                    .into(),
                );
            }
        }
        fn create(&mut self, step: &Step) -> anyhow::Result<Kind> {
            self.ops.push(format!("create:{:?}", step.kind));
            self.creates += 1;
            if self.fail == Some(step.kind) || self.fail_at == Some(self.creates) {
                return Err(anyhow::anyhow!("injected create failure"));
            }
            Ok(step.kind)
        }
        fn stamp(&mut self, _: &mut Kind, step: &Step) {
            self.ops.push(format!("stamp:{:?}", step.kind));
        }
        fn post(&mut self, _: &Kind, step: &Step) {
            self.ops.push(format!("post:{:?}", step.kind));
            self.posted.push((step.kind, step.placement));
        }
        fn settle(&mut self, step: &Step) {
            if step.delay_ms > 0 {
                self.ops.push(format!("wait{}", step.delay_ms));
            }
        }
        fn observe(&mut self, step: &Step) {
            if observes(self.route, step.kind) {
                self.ops.push(format!("observe:{:?}", step.kind));
                assert_ne!(
                    self.panic_at,
                    Some(step.placement),
                    "injected observer panic"
                );
            }
        }
    }
    fn ops(route: Route, plan: Vec<Step>) -> Vec<String> {
        let mut fake = Fake::new(route);
        if matches!(route, Route::Desktop) {
            fake.ops
                .extend(["warp", "associate", "observe:start", "wait40"].map(str::to_owned));
        }
        let prepared = prepare_gesture(&mut fake, &plan).unwrap();
        if !matches!(route, Route::Background) {
            fake.ops.push("modifiers:down".into());
        }
        run_prepared(&mut fake, plan, prepared, || true).unwrap();
        if !matches!(route, Route::Background) {
            fake.ops
                .extend(["modifiers:up", "wait100"].map(str::to_owned));
        }
        fake.ops
    }
    #[test]
    fn a_create_failure_after_down_posts_exactly_one_release() {
        let mut fake = Fake::new(Route::Desktop);
        fake.fail = Some(Kind::Dragged);
        let plan = super::super::mouse::test_foreground_gesture_plan(true);
        assert!(run_gesture(&mut fake, plan, || true).is_err());
        assert_eq!(
            fake.ops
                .iter()
                .filter(|op| op.as_str() == "post:Up")
                .count(),
            1
        );
    }

    fn background_plan() -> Vec<Step> {
        super::super::mouse::background_gesture_plan((0.0, 0.0), (10.0, 10.0), 2, 20)
    }
    fn posted(fake: &Fake, kind: Kind) -> Vec<Placement> {
        fake.posted
            .iter()
            .filter_map(|(k, p)| (*k == kind).then_some(*p))
            .collect()
    }

    #[test]
    fn a_primer_up_failure_releases_the_left_button() {
        let mut fake = Fake::new(Route::Background);
        fake.fail_at = Some(5);
        let failure = run_gesture(&mut fake, background_plan(), || true).unwrap_err();
        assert_eq!(posted(&fake, Kind::PrimerDown).len(), 1);
        assert_eq!(posted(&fake, Kind::PrimerUp).len(), 1);
        assert!(posted(&fake, Kind::Down).is_empty());
        assert_eq!(failure.primer_receipt, ReleaseReceipt::HandedToTransport);
        assert_eq!(failure.button_receipt, ReleaseReceipt::NotArmed);
    }
    #[test]
    fn a_release_that_cannot_be_built_sends_no_down() {
        for fail_at in [1, 2] {
            let mut fake = Fake::new(Route::Background);
            fake.fail_at = Some(fail_at);
            let failure = run_gesture(&mut fake, background_plan(), || true).unwrap_err();
            assert!(fake.posted.is_empty());
            assert!(!failure.input_posted);
        }
    }
    #[test]
    fn primers_completed_then_a_later_failure_still_reports_input_posted() {
        let mut fake = Fake::new(Route::Background);
        fake.fail_at = Some(6);
        let failure = run_gesture(&mut fake, background_plan(), || true).unwrap_err();
        assert!(failure.input_posted);
        assert_eq!(failure.primer_receipt, ReleaseReceipt::NotArmed);
        assert_eq!(failure.button_receipt, ReleaseReceipt::NotArmed);
        assert_eq!(posted(&fake, Kind::PrimerUp).len(), 1);
    }
    #[test]
    fn observer_panic_at_down_and_each_dragged_releases_at_that_point() {
        let plan = background_plan();
        for expected in plan
            .iter()
            .filter(|s| matches!(s.kind, Kind::Down | Kind::Dragged))
            .map(|s| s.placement)
        {
            let mut fake = Fake::new(Route::Background);
            fake.panic_at = Some(expected);
            let failure = run_gesture(&mut fake, plan.clone(), || true).unwrap_err();
            assert_eq!(posted(&fake, Kind::Up), [expected]);
            assert_eq!(failure.button_receipt, ReleaseReceipt::HandedToTransport);
        }
    }
    #[test]
    fn keep_going_panic_releases() {
        let mut fake = Fake::new(Route::Background);
        let failure = run_gesture(&mut fake, background_plan(), || {
            panic!("injected keep going panic")
        })
        .unwrap_err();
        assert_eq!(posted(&fake, Kind::Up), posted(&fake, Kind::Dragged));
        assert_eq!(failure.button_receipt, ReleaseReceipt::HandedToTransport);
    }
    #[test]
    fn a_stopped_drag_whose_up_fails_to_build_releases_once() {
        let mut fake = Fake::new(Route::Background);
        fake.fail_at = Some(8);
        let failure = run_gesture(&mut fake, background_plan(), || false).unwrap_err();
        assert_eq!(posted(&fake, Kind::Up), posted(&fake, Kind::Dragged));
        assert_eq!(failure.button_receipt, ReleaseReceipt::HandedToTransport);
    }
    #[test]
    fn normal_and_stopped_completion_post_exactly_one_up() {
        for go in [true, false] {
            let mut fake = Fake::new(Route::Background);
            run_gesture(&mut fake, background_plan(), || go).unwrap();
            assert_eq!(posted(&fake, Kind::Up).len(), 1);
            assert_eq!(
                posted(&fake, Kind::Up).last(),
                posted(&fake, Kind::Dragged).last()
            );
        }
    }
    #[test]
    fn the_receipt_reports_handed_to_transport_not_confirmed() {
        let mut fake = Fake::new(Route::Background);
        fake.fail_at = Some(7);
        let failure = run_gesture(&mut fake, background_plan(), || true).unwrap_err();
        assert_eq!(failure.button_receipt, ReleaseReceipt::HandedToTransport);
    }

    #[test]
    fn foreground_err_and_panic_release_button_before_modifiers() {
        use core_graphics::event::CGEventFlags;
        use std::{cell::RefCell, rc::Rc};
        struct Keys(Rc<RefCell<Vec<String>>>);
        impl super::super::keyboard::ModifierIo for Keys {
            fn post_key(&mut self, _: u16, down: bool, flags: CGEventFlags) -> anyhow::Result<()> {
                self.0.borrow_mut().push(format!(
                    "key:{down}:{}",
                    flags.contains(CGEventFlags::CGEventFlagShift)
                ));
                Ok(())
            }
            fn pause(&mut self, _: std::time::Duration) {}
        }
        struct Pointer {
            log: Rc<RefCell<Vec<String>>>,
            flags: CGEventFlags,
            panic: bool,
        }
        impl GestureIo for Pointer {
            type Event = Kind;
            fn prepare(&mut self, _: &Step) {}
            fn create(&mut self, step: &Step) -> anyhow::Result<Kind> {
                if !self.panic && step.kind == Kind::Dragged {
                    return Err(anyhow::anyhow!("injected"));
                }
                Ok(step.kind)
            }
            fn stamp(&mut self, _: &mut Kind, _: &Step) {}
            fn post(&mut self, _: &Kind, step: &Step) {
                if matches!(step.kind, Kind::Down | Kind::Up) {
                    self.log.borrow_mut().push(format!(
                        "button:{:?}:{}",
                        step.kind,
                        self.flags.contains(CGEventFlags::CGEventFlagShift)
                    ));
                }
            }
            fn settle(&mut self, _: &Step) {}
            fn observe(&mut self, step: &Step) {
                if self.panic && step.kind == Kind::Down {
                    panic!("injected observer panic");
                }
            }
        }
        for panic in [false, true] {
            let log = Rc::new(RefCell::new(Vec::new()));
            let mut keys = Keys(log.clone());
            let result = super::super::keyboard::with_modifier_io(&mut keys, &["shift"], |flags| {
                let mut pointer = Pointer {
                    log: log.clone(),
                    flags,
                    panic,
                };
                run_gesture(
                    &mut pointer,
                    super::super::mouse::test_foreground_gesture_plan(false),
                    || true,
                )
                .map(|_| ())
                .map_err(Into::into)
            });
            assert!(result.is_err());
            assert_eq!(
                *log.borrow(),
                [
                    "key:true:true",
                    "button:Down:true",
                    "button:Up:true",
                    "key:false:false"
                ]
            );
        }
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut keys = Keys(log.clone());
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: anyhow::Result<()> =
                super::super::keyboard::with_modifier_io(&mut keys, &["shift"], |_| {
                    panic!("outer panic")
                });
        }));
        assert!(unwind.is_err());
        assert_eq!(*log.borrow(), ["key:true:true", "key:false:false"]);
    }

    #[test]
    fn every_creation_failure_after_down_releases_at_the_last_handoff() {
        for index in 7..=9 {
            let mut fake = Fake::new(Route::Background);
            fake.fail_at = Some(index);
            let failure = run_gesture(&mut fake, background_plan(), || true).unwrap_err();
            let expected = fake
                .posted
                .iter()
                .rev()
                .find(|(kind, _)| matches!(kind, Kind::Down | Kind::Dragged))
                .unwrap()
                .1;
            assert_eq!(posted(&fake, Kind::Up), [expected]);
            assert_eq!(failure.button_receipt, ReleaseReceipt::HandedToTransport);
        }
    }
    #[test]
    fn release_now_is_idempotent() {
        let mut fake = Fake::new(Route::Background);
        let step = *background_plan()
            .iter()
            .find(|step| step.kind == Kind::Up)
            .unwrap();
        let mut guard = ButtonGuard {
            release: Kind::Up,
            step,
            armed: true,
        };
        assert_eq!(
            guard.release_now(&mut fake),
            ReleaseReceipt::HandedToTransport
        );
        assert_eq!(guard.release_now(&mut fake), ReleaseReceipt::NotArmed);
        assert_eq!(posted(&fake, Kind::Up).len(), 1);
    }

    #[test]
    fn operation_order_matches_the_current_background() {
        let actual = ops(
            Route::Background,
            super::super::mouse::background_gesture_plan((0.0, 0.0), (10.0, 10.0), 1, 20),
        );
        assert_eq!(
            actual,
            [
                "create:PrimerUp",
                "create:Up",
                "create:Move",
                "stamp:Move",
                "post:Move",
                "wait15",
                "create:PrimerDown",
                "stamp:PrimerDown",
                "post:PrimerDown",
                "wait1",
                "create:PrimerUp",
                "stamp:PrimerUp",
                "post:PrimerUp",
                "wait100",
                "create:Down",
                "stamp:Down",
                "post:Down",
                "observe:Down",
                "wait30",
                "create:Dragged",
                "stamp:Dragged",
                "post:Dragged",
                "observe:Dragged",
                "wait50",
                "create:Up",
                "stamp:Up",
                "post:Up",
                "wait100"
            ],
            "mouse.rs:1102-1143,1254-1310 at 5e907ef1"
        );
    }
    #[test]
    fn operation_order_matches_the_current_window_foreground() {
        let actual = ops(
            Route::Window,
            super::super::mouse::test_foreground_gesture_plan(false),
        );
        assert_eq!(
            actual,
            [
                "create:Up",
                "modifiers:down",
                "warp",
                "associate",
                "wait40",
                "create:Move",
                "stamp:Move",
                "post:Move",
                "wait12",
                "create:Down",
                "stamp:Down",
                "post:Down",
                "observe:Down",
                "wait16",
                "warp",
                "create:Dragged",
                "stamp:Dragged",
                "post:Dragged",
                "observe:Dragged",
                "wait20",
                "wait50",
                "create:Up",
                "stamp:Up",
                "post:Up",
                "modifiers:up",
                "wait100"
            ],
            "mouse.rs:1409-1482 at 5e907ef1; keyboard.rs:717-761"
        );
    }
    #[test]
    fn operation_order_matches_the_current_desktop_foreground() {
        let actual = ops(
            Route::Desktop,
            super::super::mouse::test_foreground_gesture_plan(true),
        );
        assert_eq!(
            actual,
            [
                "warp",
                "associate",
                "observe:start",
                "wait40",
                "create:Up",
                "modifiers:down",
                "create:Move",
                "stamp:Move",
                "post:Move",
                "wait30",
                "create:Down",
                "stamp:Down",
                "post:Down",
                "wait16",
                "create:Dragged",
                "stamp:Dragged",
                "post:Dragged",
                "observe:Dragged",
                "wait20",
                "create:Up",
                "stamp:Up",
                "post:Up",
                "modifiers:up",
                "wait100"
            ],
            "mouse.rs:1534-1586 at 5e907ef1; keyboard.rs:717-761"
        );
    }
}
