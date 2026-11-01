use super::scope::{NativeScope, RawScope};
use super::*;
use cua_driver_core::actionability::{Clock, Rect};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
};

fn rect(x: f64) -> Rect {
    Rect {
        x,
        y: 0.0,
        width: 20.0,
        height: 20.0,
    }
}
struct FakeRaw {
    parents: HashMap<u32, u32>,
    frames: HashMap<u32, Rect>,
    roles: HashMap<u32, &'static str>,
    hidden: Evidence<bool>,
    minimized: Evidence<bool>,
    space: Evidence<bool>,
    hit: Evidence<u32>,
    hit_window: Evidence<u32>,
    calls: Vec<&'static str>,
}
impl Default for FakeRaw {
    fn default() -> Self {
        Self {
            parents: [(1, 2), (3, 1), (4, 2)].into(),
            frames: [
                (1, rect(0.0)),
                (2, rect(0.0)),
                (3, rect(0.0)),
                (4, rect(0.0)),
            ]
            .into(),
            roles: [
                (1, "AXButton"),
                (2, "AXWindow"),
                (3, "AXStaticText"),
                (4, "AXButton"),
            ]
            .into(),
            hidden: Evidence::Known(false),
            minimized: Evidence::Known(false),
            space: Evidence::Known(true),
            hit: Evidence::Known(1),
            hit_window: Evidence::Known(2),
            calls: vec![],
        }
    }
}
impl RawScope<u32> for FakeRaw {
    type Node = u32;
    fn node(&mut self, e: u32) -> u32 {
        self.calls.push("node");
        e
    }
    fn same(&mut self, a: &u32, b: &u32) -> bool {
        self.calls.push("same");
        a == b
    }
    fn parent(&mut self, n: &u32) -> Evidence<u32> {
        self.calls.push("parent");
        self.parents
            .get(n)
            .copied()
            .map_or_else(|| Evidence::Unknown("parent".into()), Evidence::Known)
    }
    fn window(&mut self, _: &u32) -> Evidence<u32> {
        self.calls.push("window");
        self.hit_window.clone()
    }
    fn role(&mut self, n: &u32) -> Evidence<String> {
        self.calls.push("role");
        self.roles.get(n).map_or_else(
            || Evidence::Unknown("role".into()),
            |r| Evidence::Known((*r).into()),
        )
    }
    fn frame(&mut self, n: &u32) -> Evidence<Rect> {
        self.calls.push("frame");
        self.frames
            .get(n)
            .copied()
            .map_or_else(|| Evidence::Unknown("frame".into()), Evidence::Known)
    }
    fn hidden(&mut self) -> Evidence<bool> {
        self.calls.push("hidden");
        self.hidden.clone()
    }
    fn minimized(&mut self, _: &u32) -> Evidence<bool> {
        self.calls.push("minimized");
        self.minimized.clone()
    }
    fn current_space(&mut self, _: &u32) -> Evidence<bool> {
        self.calls.push("space");
        self.space.clone()
    }
    fn hit(&mut self, _: &u32, s: Scope, _: Point) -> Evidence<u32> {
        self.calls.push(match s {
            Scope::Foreground => "foreground_hit",
            Scope::WindowInternal => "window_hit",
        });
        self.hit.clone()
    }
}
struct FakeReader {
    frame: Rect,
}
impl Reader for FakeReader {
    type Element = u32;
    fn boolean(&mut self, _: u32, _: &str) -> Evidence<bool> {
        Evidence::Known(true)
    }
    fn string(&mut self, _: u32, _: &str) -> Evidence<String> {
        Evidence::Known("AXButton".into())
    }
    fn actions(&mut self, _: u32) -> Evidence<Vec<String>> {
        Evidence::Known(vec!["AXPress".into()])
    }
    fn frame(&mut self, _: u32) -> Evidence<Rect> {
        Evidence::Known(self.frame)
    }
    fn settable(&mut self, _: u32, _: &str) -> Evidence<bool> {
        Evidence::Known(false)
    }
}
#[derive(Default)]
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            self.0.fetch_add(d.as_millis() as u64, Ordering::SeqCst);
        })
    }
}
fn facts(raw: &mut NativeScope<FakeRaw>, scope: Scope) -> ElementFacts {
    Adapter::new(1, 2, scope).facts(
        &mut FakeReader { frame: rect(0.0) },
        raw,
        &TestClock::default(),
    )
}
fn hits(f: ElementFacts) -> cua_driver_core::actionability::HitEvidence {
    match f.receives_events {
        Evidence::Known(h) => h,
        other => panic!("missing hits: {other:?}"),
    }
}
#[test]
fn hidden_minimized_and_visibility_preserve_unknown() {
    let mut s = NativeScope {
        raw: FakeRaw::default(),
    };
    s.raw.hidden = Evidence::Known(true);
    assert_eq!(
        facts(&mut s, Scope::Foreground).visible,
        Evidence::Known(false)
    );
    s.raw.hidden = Evidence::Known(false);
    s.raw.minimized = Evidence::Known(true);
    assert_eq!(
        facts(&mut s, Scope::Foreground).visible,
        Evidence::Known(false)
    );
    s.raw.minimized = Evidence::Unknown("failed".into());
    assert!(matches!(
        facts(&mut s, Scope::Foreground).visible,
        Evidence::Unknown(_)
    ));
    s.raw.minimized = Evidence::Known(false);
    s.raw.frames.insert(2, rect(100.0));
    let clipped = facts(&mut s, Scope::WindowInternal);
    assert_eq!(clipped.visible, Evidence::Known(false));
    assert_eq!(clipped.offscreen, Evidence::Known(true));
}
#[test]
fn unproven_ancestry_never_proves_same_window() {
    let mut s = NativeScope {
        raw: FakeRaw::default(),
    };
    s.raw.parents.remove(&1);
    let f = facts(&mut s, Scope::WindowInternal);
    assert!(matches!(f.visible, Evidence::Unknown(_)));
    assert_eq!(hits(f).unknown, 5);
    assert!(!s.raw.calls.contains(&"window_hit"));
}
#[test]
fn current_space_is_not_inferred_from_space_free_inventory() {
    let mut s = NativeScope {
        raw: FakeRaw::default(),
    };
    s.raw.space = Evidence::Unknown("no membership".into());
    assert!(matches!(
        facts(&mut s, Scope::Foreground).offscreen,
        Evidence::Unknown(_)
    ));
    assert_eq!(
        facts(&mut s, Scope::WindowInternal).offscreen,
        Evidence::Known(false)
    );
    s.raw.space = Evidence::Known(false);
    assert_eq!(
        facts(&mut s, Scope::Foreground).offscreen,
        Evidence::Known(true)
    );
}
#[test]
fn failed_hit_is_unknown_not_miss() {
    let mut s = NativeScope {
        raw: FakeRaw::default(),
    };
    s.raw.hit = Evidence::Unknown("failed".into());
    let h = hits(facts(&mut s, Scope::Foreground));
    assert_eq!((h.unknown, h.occluded), (5, 0));
    s.raw.hit = Evidence::Known(4);
    let h = hits(facts(&mut s, Scope::Foreground));
    assert_eq!((h.unknown, h.occluded), (0, 5));
}
#[test]
fn window_hit_cannot_authorize_foreground() {
    // The actual native adapter does not turn an application-level hit into
    // window-local proof; this branch performs no live accessibility read.
    let mut native = NativeScope::new(0);
    let window = unsafe { crate::ax::snapshot::RetainedElement::retain(0) };
    assert!(matches!(
        <super::scope::NativeRaw as RawScope<crate::ax::bindings::AXUIElementRef>>::hit(
            &mut native.raw,
            &window,
            Scope::WindowInternal,
            Point { x: 1.0, y: 1.0 },
        ),
        Evidence::Unknown(_)
    ));
    let mut s = NativeScope {
        raw: FakeRaw::default(),
    };
    let window = hits(facts(&mut s, Scope::WindowInternal));
    assert!(window.target_window);
    assert!(window.verified_point.is_some());
    s.raw.hit = Evidence::Unknown("desktop hit failed".into());
    let foreground = hits(facts(&mut s, Scope::Foreground));
    assert!(!foreground.target_window);
    assert_eq!(foreground.unknown, 5);
    assert!(s.raw.calls.contains(&"window_hit"));
    assert!(s.raw.calls.contains(&"foreground_hit"));
}
#[test]
fn descendant_and_container_identity_are_bounded() {
    let mut s = NativeScope {
        raw: FakeRaw::default(),
    };
    s.raw.hit = Evidence::Known(3);
    assert!(hits(facts(&mut s, Scope::WindowInternal))
        .verified_point
        .is_some());
    s.raw.parents.insert(1, 5);
    s.raw.parents.insert(5, 2);
    s.raw.frames.insert(5, rect(0.0));
    s.raw.roles.insert(5, "AXCell");
    s.raw.hit = Evidence::Known(5);
    assert!(hits(facts(&mut s, Scope::WindowInternal))
        .verified_point
        .is_some());
    s.raw.roles.remove(&5);
    assert_eq!(hits(facts(&mut s, Scope::WindowInternal)).unknown, 5);
    s.raw.parents.insert(5, 5);
    assert_eq!(hits(facts(&mut s, Scope::WindowInternal)).unknown, 5);
}
#[test]
fn geometry_changes_reset_samples_and_fail_revalidation() {
    let mut s = NativeScope {
        raw: FakeRaw::default(),
    };
    let clock = TestClock::default();
    let mut a = Adapter::new(1, 2, Scope::WindowInternal);
    let mut r = FakeReader { frame: rect(0.0) };
    for t in [0, 17, 34] {
        clock.0.store(t, Ordering::SeqCst);
        let f = a.facts(&mut r, &mut s, &clock);
        if t == 34 {
            assert_eq!(f.stable, Evidence::Known(true));
        }
    }
    r.frame = rect(2.0);
    s.raw.frames.insert(1, rect(2.0));
    clock.0.store(51, Ordering::SeqCst);
    assert_eq!(
        a.facts(&mut r, &mut s, &clock).stable,
        Evidence::Known(false)
    );
    r.frame = rect(3.0);
    assert_eq!(a.revalidate(&mut r), Evidence::Known(false));
}
#[test]
fn scope_probe_never_reads_values_or_occluder_names() {
    let mut s = NativeScope {
        raw: FakeRaw::default(),
    };
    s.raw.hit = Evidence::Known(4);
    let h = hits(facts(&mut s, Scope::Foreground));
    assert_eq!(h.occluder.unwrap().name, None);
    assert!(s.raw.calls.iter().all(|c| [
        "node",
        "same",
        "parent",
        "window",
        "role",
        "frame",
        "hidden",
        "minimized",
        "space",
        "foreground_hit"
    ]
    .contains(c)));
}
