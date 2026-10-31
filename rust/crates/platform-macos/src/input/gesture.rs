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

pub(super) fn run_gesture<I: GestureIo>(
    io: &mut I,
    plan: Vec<Step>,
    mut keep_going: impl FnMut() -> bool,
) -> anyhow::Result<GestureOutcome> {
    let mut stopped = None;
    for mut step in plan {
        if let Some(at) = stopped {
            match step.kind {
                Kind::Dragged => continue,
                Kind::Up => step.placement = at,
                _ => {}
            }
        }
        io.prepare(&step);
        let mut event = io.create(&step)?;
        io.stamp(&mut event, &step);
        io.post(&event, &step);
        if matches!(step.kind, Kind::Down | Kind::Dragged) {
            io.observe(&step);
        }
        io.settle(&step);
        if step.kind == Kind::Dragged && stopped.is_none() && !keep_going() {
            stopped = Some(step.placement);
        }
    }
    Ok(match stopped {
        Some(at) => GestureOutcome::StoppedEarly { at },
        None => GestureOutcome::Completed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::RoutePolicy as Route;
    struct Fake {
        route: Route,
        ops: Vec<String>,
        fail: Option<Kind>,
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
            if self.fail == Some(step.kind) {
                return Err(anyhow::anyhow!("injected create failure"));
            }
            Ok(step.kind)
        }
        fn stamp(&mut self, _: &mut Kind, step: &Step) {
            self.ops.push(format!("stamp:{:?}", step.kind));
        }
        fn post(&mut self, _: &Kind, step: &Step) {
            self.ops.push(format!("post:{:?}", step.kind));
        }
        fn settle(&mut self, step: &Step) {
            if step.delay_ms > 0 {
                self.ops.push(format!("wait{}", step.delay_ms));
            }
        }
        fn observe(&mut self, step: &Step) {
            if observes(self.route, step.kind) {
                self.ops.push(format!("observe:{:?}", step.kind));
            }
        }
    }
    fn ops(route: Route, plan: Vec<Step>) -> Vec<String> {
        let mut fake = Fake {
            route,
            ops: vec![],
            fail: None,
        };
        if matches!(route, Route::Desktop) {
            fake.ops
                .extend(["warp", "associate", "observe:start", "wait40"].map(str::to_owned));
        }
        if !matches!(route, Route::Background) {
            fake.ops.push("modifiers:down".into());
        }
        run_gesture(&mut fake, plan, || true).unwrap();
        if !matches!(route, Route::Background) {
            fake.ops
                .extend(["modifiers:up", "wait100"].map(str::to_owned));
        }
        fake.ops
    }
    #[test]
    fn a_create_failure_after_down_posts_exactly_one_release() {
        let mut fake = Fake {
            route: Route::Desktop,
            ops: vec![],
            fail: Some(Kind::Dragged),
        };
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

    #[test]
    fn operation_order_matches_the_current_background() {
        let actual = ops(
            Route::Background,
            super::super::mouse::background_gesture_plan((0.0, 0.0), (10.0, 10.0), 1, 20),
        );
        assert_eq!(
            actual,
            [
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
