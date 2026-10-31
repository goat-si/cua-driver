use crate::window_basis::{BasisRefusal, DispatchEffect, DispatchOutcome};
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

/// Activate a foreground click's target during preparation.
///
/// `activate` must change the foreground without emitting input.
/// `assist` may emit synthetic input (the no-name key that earns the
/// foreground token) and returns (activated, emitted input). It is used only
/// when `allow_input_assist` is set: a guarded dispatch must not emit input
/// before its guard runs, so it never assists. Returns whether the target
/// became foreground and the side effects preparation performed.
pub fn prepare_activation(
    allow_input_assist: bool,
    activate: impl FnOnce() -> bool,
    assist: impl FnOnce() -> (bool, bool),
) -> (bool, Vec<&'static str>) {
    if activate() {
        return (true, vec!["activation"]);
    }
    if !allow_input_assist {
        return (false, vec![]);
    }
    // `assist` reports whether it activated and whether it emitted input;
    // record the input side effect only when input was actually emitted.
    let (active, emitted) = assist();
    let mut effects = vec![];
    if emitted {
        effects.push("activation_input_assist");
    }
    if active {
        effects.push("activation");
    }
    (active, effects)
}

/// The foreground sequence reserves event 0 for the pointer move. When that
/// move failed, the slot sent nothing: drop it from `events_sent`, and when
/// nothing else was sent either, the dispatch was refused with nothing sent
/// rather than partially delivered.
pub fn discount_unmoved_pointer(outcome: &mut DispatchOutcome, pointer_moved: bool) {
    if pointer_moved || outcome.events_sent == 0 {
        return;
    }
    outcome.events_sent -= 1;
    if outcome.events_sent == 0 && outcome.effect == DispatchEffect::Partial {
        outcome.effect = DispatchEffect::Refused;
    }
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

#[cfg(test)]
mod activation_tests {
    use super::prepare_activation;
    use std::cell::Cell;

    #[test]
    fn guarded_activation_never_emits_assist_input() {
        let assisted = Cell::new(false);
        let (active, effects) = prepare_activation(
            false,
            || false,
            || {
                assisted.set(true);
                (true, true)
            },
        );
        assert!(!active);
        assert!(effects.is_empty());
        assert!(
            !assisted.get(),
            "assist emits input and must not run before the guard"
        );
    }

    #[test]
    fn legacy_activation_reports_assist_input_as_a_side_effect() {
        let (active, effects) = prepare_activation(true, || false, || (true, true));
        assert!(active);
        assert_eq!(effects, ["activation_input_assist", "activation"]);
        let (active, effects) = prepare_activation(true, || false, || (false, true));
        assert!(!active);
        assert_eq!(effects, ["activation_input_assist"]);
        // The assist helper retries plain activation first; when that wins,
        // no input was emitted and none is reported.
        let (active, effects) = prepare_activation(true, || false, || (true, false));
        assert!(active);
        assert_eq!(effects, ["activation"]);
        let assisted = Cell::new(false);
        let (active, effects) = prepare_activation(
            true,
            || true,
            || {
                assisted.set(true);
                (true, true)
            },
        );
        assert!(active);
        assert_eq!(effects, ["activation"]);
        assert!(!assisted.get());
    }
}

#[cfg(test)]
mod pointer_slot_tests {
    use super::discount_unmoved_pointer;
    use crate::window_basis::{DispatchEffect, DispatchOutcome};

    fn outcome(events_sent: u32, effect: DispatchEffect) -> DispatchOutcome {
        DispatchOutcome {
            events_planned: 4,
            events_sent,
            prepared_side_effects: vec![],
            effect,
            cleanup_results: vec![],
        }
    }

    #[test]
    fn failed_pointer_move_with_nothing_else_sent_is_a_refusal() {
        let mut o = outcome(1, DispatchEffect::Partial);
        discount_unmoved_pointer(&mut o, false);
        assert_eq!((o.events_sent, o.effect), (0, DispatchEffect::Refused));
        assert_eq!(o.to_json()["nothing_sent"], true);
    }

    #[test]
    fn failed_pointer_move_is_not_counted_when_input_followed() {
        let mut o = outcome(3, DispatchEffect::Partial);
        discount_unmoved_pointer(&mut o, false);
        assert_eq!((o.events_sent, o.effect), (2, DispatchEffect::Partial));
        let mut o = outcome(4, DispatchEffect::Unverifiable);
        discount_unmoved_pointer(&mut o, false);
        assert_eq!((o.events_sent, o.effect), (3, DispatchEffect::Unverifiable));
    }

    #[test]
    fn a_moved_pointer_keeps_its_slot() {
        let mut o = outcome(1, DispatchEffect::Partial);
        discount_unmoved_pointer(&mut o, true);
        assert_eq!((o.events_sent, o.effect), (1, DispatchEffect::Partial));
    }
}

#[cfg(test)]
mod unknown_tests {
    use super::*;
    struct UnknownSink { fail: usize, cleaned: Option<usize> }
    impl InputSink for UnknownSink {
        fn prepare(&mut self) -> Vec<&'static str> { vec![] }
        fn send(&mut self, i: usize) -> Result<(), SendFailure> {
            if i == self.fail { Err(SendFailure { error: "lost reply".into(), emitted: Emitted::Unknown }) } else { Ok(()) }
        }
        fn cleanup(&mut self, sent: usize) -> Vec<Value> { self.cleaned = Some(sent); vec![json!("cleaned")] }
    }
    #[test]
    fn unknown_preserves_confirmed_prefix_and_cleans_up() {
        for i in [0, 2] {
            let mut sink = UnknownSink { fail: i, cleaned: None };
            let GuardedResult::Sent { outcome } = run_guarded(&mut sink, 3, || Ok(())) else { panic!() };
            assert_eq!(outcome.effect, DispatchEffect::Uncertain);
            assert_eq!(outcome.events_sent, i as u32);
            assert!(outcome.to_json().get("nothing_sent").is_none());
            assert_eq!(sink.cleaned, Some(i));
            assert_eq!(outcome.cleanup_results, [json!("cleaned"), json!({"error":"lost reply"})]);
        }
    }
}
