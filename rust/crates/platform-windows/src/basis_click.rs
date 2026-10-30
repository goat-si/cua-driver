use crate::input::guarded_dispatch::GuardedResult;
use cua_driver_core::window_basis::{BasisRefusal, DispatchEffect, LiveFrame, WindowBasis};
use serde_json::{json, Value};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    Foreground,
    Background,
    Unavailable,
}

pub(crate) fn transport(foreground: bool, modifiers: bool, posted_unavailable: bool) -> Transport {
    if foreground || modifiers {
        Transport::Foreground
    } else if posted_unavailable {
        Transport::Unavailable
    } else {
        Transport::Background
    }
}

pub(crate) fn transport_allowed(mode: &Transport, background_only: bool) -> bool {
    !background_only || *mode != Transport::Foreground
}

pub(crate) fn early_point(
    basis: &WindowBasis,
    frame: Result<LiveFrame, BasisRefusal>,
    x: f64,
    y: f64,
) -> Result<(i64, i64), BasisRefusal> {
    basis.check_and_map(&frame?, x, y)
}

pub(crate) fn dispatch_check(
    basis: &WindowBasis,
    frame: Result<LiveFrame, BasisRefusal>,
) -> Result<(), BasisRefusal> {
    basis.check(&frame?)
}

pub(crate) fn background_refusal(
    mut result: cua_driver_core::protocol::ToolResult,
) -> cua_driver_core::protocol::ToolResult {
    if let Some(value) = result.structured_content.as_mut() {
        value["effect"] = json!("refused");
        value["nothing_sent"] = json!(true);
        value["prepared_side_effects"] = json!([]);
    }
    result
}

pub(crate) fn result_json(result: &GuardedResult, snapshot_id: &str, point: (i64, i64)) -> Value {
    let (outcome, refusal) = match result {
        GuardedResult::Refused { outcome, refusal } => (outcome, Some(refusal)),
        GuardedResult::Sent { outcome } => (outcome, None),
    };
    let mut value = outcome.to_json();
    value["path"] = json!("pixel_basis");
    value["basis_checked_at_dispatch"] = json!(true);
    value["snapshot_id"] = json!(snapshot_id);
    value["screen_point"] = json!({"x":point.0,"y":point.1});
    if let Some(refusal) = refusal {
        value["code"] = json!(refusal.code);
        value["reason"] = json!(refusal.reason);
    } else if outcome.effect == DispatchEffect::Partial {
        value["code"] = json!("partial_delivery");
    } else if outcome.effect == DispatchEffect::Uncertain {
        value["code"] = json!("uncertain");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::guarded_dispatch::{run_guarded, InputSink};
    use cua_driver_core::window_basis::DispatchOutcome;
    use std::cell::RefCell;
    fn assert_refusal(args: Value, expected: &str) {
        let result = validate_arguments("click", &args).unwrap_err();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(serde_json::to_value(&result.content).unwrap(), json!([{"type":"text", "text":expected}]));
        assert_eq!(result.structured_content, Some(json!({"code":"invalid_arguments", "effect":"refused", "nothing_sent":true})));
    }

    #[test]
    fn invalid_pid_refusal() {
        for pid in [json!(null), json!("1"), json!(i64::from(i32::MAX) + 1), json!(i64::from(i32::MIN) - 1)] {
            assert_refusal(json!({"pid":pid,"snapshot_id":"s"}), "snapshot_id needs the pid of the window it was read from: pass pid from get_window_state.");
        }
    }

    #[test]
    fn invalid_snapshot_refusal() {
        for snapshot in [json!(null), json!(42)] {
            assert_refusal(json!({"pid":1,"snapshot_id":snapshot}), "snapshot_id must be the window_basis.snapshot_id string from get_window_state.");
        }
    }

    #[test]
    fn invalid_count_refusal() {
        for count in [0, 4] {
            assert_refusal(json!({"pid":1,"snapshot_id":"s","count":count}), "click: count must be 1 (single), 2 (double) or 3 (triple).");
        }
    }

    #[test]
    fn invalid_button_refusal() {
        assert_refusal(json!({"pid":1,"snapshot_id":"s","button":"other"}), "click: unknown button \"other\" — expected one of left, right, middle.");
    }

    #[test]
    fn accepts_boundaries_and_buttons() {
        for pid in [i32::MIN, 0, i32::MAX] {
            for count in [1, 3] {
                for button in ["left", "right", "middle"] {
                    let args = json!({"pid":pid,"snapshot_id":"s","count":count,"button":button});
                    let parsed = validate_arguments("click", &args).unwrap();
                    assert_eq!((parsed.pid, parsed.snapshot_id, parsed.count, parsed.button), (pid, "s", count as usize, button));
                }
            }
        }
        let args = json!({"pid":1,"snapshot_id":"s", "count":-1, "button": 42});
        let parsed = validate_arguments("click", &args).unwrap();
        assert_eq!((parsed.count, parsed.button), (1, "left"));
    }

    #[test]
    fn specialized_clicks_ignore_count_and_button() {
        let args = json!({"pid":1,"snapshot_id":"s","count":4,"button":"other"});
        for (tool, count, button) in [("double_click", 2, "left"), ("right_click", 1, "right")] {
            let parsed = validate_arguments(tool, &args).unwrap();
            assert_eq!((parsed.count, parsed.button), (count, button));
        }
    }
    fn basis() -> WindowBasis {
        WindowBasis {
            window_id: 2,
            pid: 1,
            snapshot_id: "s00000001.f1".into(),
            x: 10,
            y: 20,
            width: 100,
            height: 80,
            scale_x: 1.0,
            scale_y: 1.0,
            capture_width: 100,
            capture_height: 80,
            display_scale: Some(1.0),
            captured_at_ms: 0,
        }
    }
    fn frame() -> LiveFrame {
        LiveFrame {
            x: 10,
            y: 20,
            width: 100,
            height: 80,
            display_scale: Some(1.0),
        }
    }
    struct Sink<'a>(&'a RefCell<Vec<&'static str>>);
    impl InputSink for Sink<'_> {
        fn prepare(&mut self) -> Vec<&'static str> {
            vec![]
        }
        fn send(&mut self, _: usize) -> Result<(), String> {
            self.0.borrow_mut().push("send");
            Ok(())
        }
        fn cleanup(&mut self, _: usize) -> Vec<Value> {
            vec![]
        }
    }
    #[test]
    fn frame_checks_precede_dispatch_and_events() {
        let b = basis();
        let mut stale = frame();
        stale.x += 1;
        let log = RefCell::new(vec![]);
        if early_point(&b, Ok(stale), 1.0, 2.0).is_ok() {
            log.borrow_mut().push("dispatch");
        }
        assert!(log.borrow().is_empty());
        assert!(early_point(&b, Ok(frame()), 1.0, 2.0).is_ok());
        let refused = run_guarded(&mut Sink(&log), 1, || {
            log.borrow_mut().push("guard");
            dispatch_check(&b, Ok(stale))
        });
        assert!(matches!(refused, GuardedResult::Refused { .. }));
        assert_eq!(*log.borrow(), ["guard"]);
        log.borrow_mut().clear();
        let sent = run_guarded(&mut Sink(&log), 1, || {
            log.borrow_mut().push("guard");
            dispatch_check(&b, Ok(frame()))
        });
        assert!(matches!(sent, GuardedResult::Sent { .. }));
        assert_eq!(*log.borrow(), ["guard", "send"]);
    }
    #[test]
    fn transport_and_result_contract() {
        let unavailable = background_refusal(
            cua_driver_core::protocol::ToolResult::error("unavailable")
                .with_structured(json!({"code":"background_unavailable"})),
        );
        assert_eq!(
            unavailable.structured_content.unwrap(),
            json!({"code":"background_unavailable","effect":"refused","nothing_sent":true,"prepared_side_effects":[]})
        );
        assert!(!transport_allowed(&Transport::Foreground, true));
        assert!(transport_allowed(&Transport::Background, true));
        assert!(transport_allowed(&Transport::Foreground, false));
        assert_eq!(transport(false, false, false), Transport::Background);
        assert_eq!(transport(false, false, true), Transport::Unavailable);
        assert_eq!(transport(false, true, true), Transport::Foreground);
        assert_eq!(transport(true, false, true), Transport::Foreground);
        for effect in [
            DispatchEffect::Refused,
            DispatchEffect::Partial,
            DispatchEffect::Uncertain,
            DispatchEffect::Unverifiable,
        ] {
            let outcome = DispatchOutcome {
                events_planned: 2,
                events_sent: match effect {
                    DispatchEffect::Refused => 0,
                    DispatchEffect::Unverifiable => 2,
                    _ => 1,
                },
                prepared_side_effects: vec![],
                effect,
                cleanup_results: vec![],
            };
            let result = if effect == DispatchEffect::Refused {
                GuardedResult::Refused {
                    outcome,
                    refusal: BasisRefusal {
                        code: "stale_basis",
                        reason: "window_moved",
                    },
                }
            } else {
                GuardedResult::Sent { outcome }
            };
            let value = result_json(&result, "s00000001.f1", (11, 22));
            assert_eq!(value["path"], "pixel_basis");
            assert_eq!(
                value["effect"],
                match effect {
                    DispatchEffect::Refused => "refused",
                    DispatchEffect::Partial => "partial",
                    DispatchEffect::Uncertain => "uncertain",
                    DispatchEffect::Unverifiable => "unverifiable",
                }
            );
            if effect == DispatchEffect::Refused {
                assert_eq!(
                    value,
                    json!({"events_planned":2,"events_sent":0,"prepared_side_effects":[],"effect":"refused","cleanup_results":[],"nothing_sent":true,"path":"pixel_basis","basis_checked_at_dispatch":true,"snapshot_id":"s00000001.f1","screen_point":{"x":11,"y":22},"code":"stale_basis","reason":"window_moved"})
                );
                assert_eq!(value["nothing_sent"], true);
                assert_eq!(value["code"], "stale_basis");
            }
            if effect == DispatchEffect::Unverifiable {
                assert_eq!(
                    value,
                    json!({"events_planned":2,"events_sent":2,"prepared_side_effects":[],"effect":"unverifiable","cleanup_results":[],"path":"pixel_basis","basis_checked_at_dispatch":true,"snapshot_id":"s00000001.f1","screen_point":{"x":11,"y":22}})
                );
            }
            if effect == DispatchEffect::Partial {
                assert_eq!(value["code"], "partial_delivery");
            }
            if effect == DispatchEffect::Uncertain {
                assert_eq!(value["code"], "uncertain");
            }
        }
    }
}
