use cua_driver_testkit::{
    conformance::{
        self, ConformanceFixture, Contract, ContractOutcome, ContractReport, SessionPair,
    },
    Driver, ToolResponse,
};
use serde_json::{json, Value};
use std::{cell::RefCell, rc::Rc};
#[derive(Default)]
struct State {
    dispatched: Vec<String>,
    destroyed: bool,
    failing_reads: bool,
    read_errors: u64,
    generation: u64,
    incomplete_reads: bool,
    incomplete_count: u64,
    incomplete_violation: bool,
    missing_candidate: bool,
    dispatch_incomplete: bool,
}
struct Fixture(Rc<RefCell<State>>);
impl ConformanceFixture for Fixture {
    fn pid(&self) -> i32 {
        42
    }
    fn window_id(&self) -> u64 {
        7
    }
    fn destructible_index(&self) -> u64 {
        3
    }
    fn text_field_name(&self) -> &str {
        "Text"
    }
    fn disabled_button_name(&self) -> &str {
        "Disabled"
    }
    fn destroy_element(&mut self) -> Result<(), String> {
        self.0.borrow_mut().destroyed = true;
        Ok(())
    }
    fn dispatch_count(&self) -> Result<u64, String> {
        Ok(self.0.borrow().dispatched.len() as u64)
    }
    fn start_read_failure(&mut self) -> Result<(), String> {
        let mut s = self.0.borrow_mut();
        s.failing_reads = true;
        s.read_errors = 0;
        Ok(())
    }
    fn finish_read_failure(&mut self) -> Result<u64, String> {
        let mut s = self.0.borrow_mut();
        s.failing_reads = false;
        Ok(s.read_errors)
    }
    fn start_incomplete_observation(&mut self) -> Result<(), String> {
        let mut s = self.0.borrow_mut();
        s.incomplete_reads = true;
        s.incomplete_count = 0;
        Ok(())
    }
    fn finish_incomplete_observation(&mut self) -> Result<u64, String> {
        let mut s = self.0.borrow_mut();
        s.incomplete_reads = false;
        Ok(s.incomplete_count)
    }
    fn absent_element_name(&self) -> Result<&str, String> {
        Ok("Missing sentinel")
    }
}
struct Fake {
    state: Rc<RefCell<State>>,
    session: &'static str,
    violation: Option<Contract>,
}
fn response(payload: Value, text: &str, error: bool) -> ToolResponse {
    ToolResponse::from_structured(text, payload, error)
}
impl Driver for Fake {
    fn call(&mut self, tool: &str, args: Value) -> ToolResponse {
        let mut s = self.state.borrow_mut();
        let disp = if self.violation == Some(Contract::Disposition) {
            json!({"delivery":"not_delivered","retry":"unsafe"})
        } else {
            json!({"delivery":"not_delivered","retry":"safe"})
        };
        match tool {
            "get_window_state" => {
                s.generation += 1;
                let token = format!("{}:{}", self.session, s.generation);
                response(
                    json!({"snapshot_id":s.generation.to_string(),"elements":if s.destroyed { vec![] } else { vec![json!({"element_index":3,"element_token":token,"label":"Text"})] }}),
                    "",
                    false,
                )
            }
            "click" => {
                let foreign = !args["element_token"]
                    .as_str()
                    .unwrap()
                    .starts_with(self.session);
                let contract = if foreign {
                    Contract::ForeignSession
                } else {
                    Contract::Stale
                };
                if self.violation == Some(contract) {
                    s.dispatched.push(tool.into());
                    return response(json!({"effect":"confirmed"}), "clicked", false);
                }
                let text = if foreign {
                    "snapshot belongs to another session"
                } else {
                    "stale token"
                };
                response(
                    json!({"refusal":{"code":"stale_element_token","message":text},"disposition":disp}),
                    text,
                    true,
                )
            }
            "run_steps" if args["steps"][0].get("wait_for").is_some() => {
                if s.incomplete_reads {
                    s.incomplete_count += 1;
                    let code = if s.incomplete_violation {
                        "not_found"
                    } else {
                        "incomplete_observation"
                    };
                    return response(json!({"steps":[{"code":code}]}), "incomplete", true);
                }
                assert!(s.failing_reads);
                s.read_errors += 1;
                if self.violation == Some(Contract::GoneReadError) {
                    return response(json!({"steps":[{"ok":true}]}), "gone", false);
                }
                response(
                    json!({"steps":[{"code":"gone_unconfirmed"}],"disposition":disp}),
                    "read failed",
                    true,
                )
            }
            "run_steps" => {
                if s.incomplete_reads {
                    s.incomplete_count += 1;
                    if s.incomplete_violation || s.dispatch_incomplete {
                        s.dispatched.push("click".into());
                    }
                    let text = if s.incomplete_violation || s.missing_candidate {
                        "Text is uncertain; observed candidates:"
                    } else {
                        "observed candidates: [3] AXTextField \"Text\""
                    };
                    return response(
                        json!({"steps":[{"code":"incomplete_observation"}]}),
                        text,
                        true,
                    );
                }
                if self.violation == Some(Contract::Ambiguity) {
                    s.dispatched.push("click".into());
                    return response(json!({"steps":[{"ok":true}]}), "clicked", false);
                }
                response(
                    json!({"steps":[{"code":"ambiguous"}],"disposition":disp}),
                    "2 elements match; nth 0: Save; nth 1: Save",
                    true,
                )
            }
            _ => panic!("unexpected tool {tool}"),
        }
    }
}

#[test]
fn incomplete_tree_probes_require_injection_candidates_and_zero_dispatch() {
    for violate in [false, true] {
        let state = Rc::new(RefCell::new(State {
            incomplete_violation: violate,
            ..Default::default()
        }));
        let mut fixture = Fixture(state.clone());
        let mut driver = Fake {
            state,
            session: "a",
            violation: None,
        };
        for outcome in [
            conformance::incomplete_absence(&mut driver, &mut fixture),
            conformance::incomplete_uniqueness(&mut driver, &mut fixture),
        ] {
            assert_eq!(
                matches!(outcome, ContractOutcome::Pass),
                !violate,
                "{outcome:?}"
            );
        }
    }
    for (missing_candidate, dispatch_incomplete) in [(true, false), (false, true)] {
        let state = Rc::new(RefCell::new(State {
            missing_candidate,
            dispatch_incomplete,
            ..Default::default()
        }));
        let mut fixture = Fixture(state.clone());
        let mut driver = Fake {
            state,
            session: "a",
            violation: None,
        };
        assert!(matches!(
            conformance::incomplete_uniqueness(&mut driver, &mut fixture),
            ContractOutcome::Fail { .. }
        ));
    }
}
fn run(c: Contract, violation: Option<Contract>) -> ContractOutcome {
    let state = Rc::new(RefCell::new(State::default()));
    let mut fixture = Fixture(state.clone());
    let mut a = Fake {
        state: state.clone(),
        session: "a",
        violation,
    };
    let mut b = Fake {
        state,
        session: "b",
        violation,
    };
    let mut pair = SessionPair {
        a: &mut a,
        b: &mut b,
        session_evidence: Ok(()),
    };
    match c {
        Contract::Ambiguity => conformance::ambiguity(pair.a, &fixture),
        Contract::Stale => conformance::stale(pair.a, &mut fixture),
        Contract::ForeignSession => conformance::foreign_session(&mut pair, &fixture),
        Contract::GoneReadError => conformance::gone_read_error(pair.a, &mut fixture),
        Contract::Disposition => conformance::dispositions(&mut pair, &mut fixture),
        _ => unreachable!(),
    }
}
#[test]
fn each_wired_contract_detects_its_own_violation() {
    for c in [
        Contract::Ambiguity,
        Contract::Stale,
        Contract::ForeignSession,
        Contract::GoneReadError,
        Contract::Disposition,
    ] {
        assert_eq!(run(c, None), ContractOutcome::Pass, "conforming {c:?}");
        assert!(
            matches!(run(c, Some(c)), ContractOutcome::Fail { .. }),
            "nonconforming {c:?}"
        );
    }
}
#[test]
fn unwired_and_unsupported_never_satisfy_required() {
    let mut report = ContractReport::new();
    report.insert(Contract::Actionability, conformance::actionability());
    report.insert(Contract::Completeness, conformance::completeness());
    report.insert(
        Contract::ForeignSession,
        ContractOutcome::NotSupported {
            reason: "no continuity proof".into(),
        },
    );
    for c in [
        Contract::Actionability,
        Contract::Completeness,
        Contract::ForeignSession,
        Contract::Stale,
    ] {
        assert!(
            std::panic::catch_unwind(|| conformance::assert_required(&report, &[c])).is_err(),
            "{c:?}"
        );
    }
    report.insert(Contract::Stale, ContractOutcome::Pass);
    conformance::assert_required(&report, &[Contract::Stale]);
}
