use super::semantic::{bind_exact_with, WindowReader};
use super::Evidence;

struct Fake {
    pid: Evidence<i32>,
    window: Evidence<u32>,
    window_id: Evidence<u32>,
    calls: Vec<(&'static str, u32)>,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            pid: Evidence::Known(7),
            window: Evidence::Known(13),
            window_id: Evidence::Known(42),
            calls: vec![],
        }
    }
}
impl WindowReader for Fake {
    type Element = u32;
    fn pid(&mut self, target: &u32) -> Evidence<i32> {
        self.calls.push(("pid", *target));
        self.pid.clone()
    }
    fn window(&mut self, target: &u32) -> Evidence<u32> {
        self.calls.push(("window", *target));
        self.window.clone()
    }
    fn window_id(&mut self, window: &u32) -> Evidence<u32> {
        self.calls.push(("window_id", *window));
        self.window_id.clone()
    }
}
#[test]
fn exact_target_and_window_are_bound_in_order() {
    let mut reader = Fake::default();
    assert_eq!(
        bind_exact_with(&mut reader, 73, 7, 42),
        Evidence::Known((73, 13))
    );
    assert_eq!(
        reader.calls,
        [("pid", 73), ("window", 73), ("window_id", 13)]
    );
}
#[test]
fn wrong_or_unknown_pid_never_binds_a_window() {
    for pid in [
        Evidence::Known(8),
        Evidence::Absent,
        Evidence::Unknown("failed".into()),
    ] {
        let mut reader = Fake {
            pid,
            ..Fake::default()
        };
        assert!(matches!(
            bind_exact_with(&mut reader, 73, 7, 42),
            Evidence::Unknown(_)
        ));
        assert_eq!(reader.calls, [("pid", 73)]);
    }
}
#[test]
fn failed_window_reads_remain_unknown() {
    for window in [Evidence::Absent, Evidence::Unknown("failed".into())] {
        let mut reader = Fake {
            window,
            ..Fake::default()
        };
        assert!(matches!(
            bind_exact_with(&mut reader, 73, 7, 42),
            Evidence::Unknown(_)
        ));
        assert_eq!(reader.calls, [("pid", 73), ("window", 73)]);
    }
}
#[test]
fn wrong_or_unavailable_window_id_never_binds() {
    for window_id in [
        Evidence::Known(43),
        Evidence::Absent,
        Evidence::Unknown("failed".into()),
    ] {
        let mut reader = Fake {
            window_id,
            ..Fake::default()
        };
        assert!(matches!(
            bind_exact_with(&mut reader, 73, 7, 42),
            Evidence::Unknown(_)
        ));
        assert_eq!(
            reader.calls,
            [("pid", 73), ("window", 73), ("window_id", 13)]
        );
    }
}
