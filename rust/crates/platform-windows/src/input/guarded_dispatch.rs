use cua_driver_core::window_basis::{BasisRefusal, DispatchEffect, DispatchOutcome};
use serde_json::{json, Value};

pub trait InputSink {
    fn prepare(&mut self) -> Vec<&'static str>;
    fn send(&mut self, event_index: usize) -> Result<(), String>;
    fn cleanup(&mut self, sent: usize) -> Vec<Value>;
}

#[derive(Debug)]
pub enum GuardedResult {
    Refused {
        refusal: BasisRefusal,
        outcome: DispatchOutcome,
    },
    Sent {
        outcome: DispatchOutcome,
    },
}

pub fn run_guarded(
    sink: &mut impl InputSink,
    planned: usize,
    guard: impl FnOnce() -> Result<(), BasisRefusal>,
) -> GuardedResult {
    let prepared_side_effects = sink.prepare().into_iter().map(str::to_owned).collect();
    let mut outcome = DispatchOutcome {
        events_planned: u32::try_from(planned).expect("event count exceeds u32"),
        events_sent: 0,
        prepared_side_effects,
        effect: DispatchEffect::Refused,
        cleanup_results: vec![],
    };
    if let Err(refusal) = guard() {
        outcome.cleanup_results = sink.cleanup(0);
        return GuardedResult::Refused { refusal, outcome };
    }
    for i in 0..planned {
        if let Err(error) = sink.send(i) {
            outcome.effect = if i == 0 {
                DispatchEffect::Refused
            } else {
                DispatchEffect::Partial
            };
            outcome.cleanup_results = sink.cleanup(i);
            outcome.cleanup_results.push(json!({"error": error}));
            return GuardedResult::Sent { outcome };
        }
        outcome.events_sent += 1;
    }
    outcome.effect = DispatchEffect::Unverifiable;
    GuardedResult::Sent { outcome }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    struct Fake {
        log: Rc<RefCell<Vec<String>>>,
        fail: Option<usize>,
    }
    impl InputSink for Fake {
        fn prepare(&mut self) -> Vec<&'static str> {
            self.log.borrow_mut().push("prepare".into());
            vec!["cursor"]
        }
        fn send(&mut self, i: usize) -> Result<(), String> {
            self.log.borrow_mut().push(format!("send {i}"));
            if self.fail == Some(i) {
                Err("failed".into())
            } else {
                Ok(())
            }
        }
        fn cleanup(&mut self, sent: usize) -> Vec<Value> {
            self.log.borrow_mut().push(format!("cleanup {sent}"));
            vec![json!("cleaned")]
        }
    }
    fn fake(fail: Option<usize>) -> (Fake, Rc<RefCell<Vec<String>>>) {
        let log = Rc::new(RefCell::new(vec![]));
        (
            Fake {
                log: log.clone(),
                fail,
            },
            log,
        )
    }
    #[test]
    fn refusal_cleans_preparation_without_sending() {
        let (mut sink, log) = fake(None);
        let result = run_guarded(&mut sink, 3, || {
            log.borrow_mut().push("guard".into());
            Err(BasisRefusal {
                code: "stale_basis",
                reason: "window_moved",
            })
        });
        let GuardedResult::Refused { outcome, .. } = result else {
            panic!()
        };
        assert_eq!(*log.borrow(), ["prepare", "guard", "cleanup 0"]);
        assert_eq!(outcome.effect, DispatchEffect::Refused);
        assert_eq!(outcome.events_sent, 0);
        assert_eq!(outcome.prepared_side_effects, ["cursor"]);
        assert_eq!(outcome.cleanup_results, [json!("cleaned")]);
    }
    #[test]
    fn success_checks_once_before_sending() {
        let (mut sink, log) = fake(None);
        let result = run_guarded(&mut sink, 3, || {
            log.borrow_mut().push("guard".into());
            Ok(())
        });
        let GuardedResult::Sent { outcome } = result else {
            panic!()
        };
        assert_eq!(
            *log.borrow(),
            ["prepare", "guard", "send 0", "send 1", "send 2"]
        );
        assert_eq!(outcome.effect, DispatchEffect::Unverifiable);
        assert_eq!(outcome.events_sent, 3);
        assert_eq!(outcome.prepared_side_effects, ["cursor"]);
    }
    #[test]
    fn failures_account_for_each_prefix() {
        for i in 0..3 {
            let (mut sink, log) = fake(Some(i));
            let GuardedResult::Sent { outcome } = run_guarded(&mut sink, 3, || Ok(())) else {
                panic!()
            };
            assert_eq!(outcome.events_sent, i as u32);
            assert_eq!(
                outcome.effect,
                if i == 0 {
                    DispatchEffect::Refused
                } else {
                    DispatchEffect::Partial
                }
            );
            assert_eq!(log.borrow().last().unwrap(), &format!("cleanup {i}"));
            assert_eq!(
                outcome.cleanup_results,
                [json!("cleaned"), json!({"error":"failed"})]
            );
            assert_eq!(outcome.prepared_side_effects, ["cursor"]);
        }
    }
}
