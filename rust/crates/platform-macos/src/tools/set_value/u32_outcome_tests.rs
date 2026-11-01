use super::*;
use crate::ax::bindings::{kAXErrorAPIDisabled, kAXErrorCannotComplete, kAXErrorIllegalArgument};
use crate::document_edit::DocumentEdited;
use cua_driver_contract::ErrorDisposition;
use std::cell::Cell;
use std::collections::VecDeque;

struct Fake {
    reads: VecDeque<ValueRead>,
    last: ValueRead,
    settable: Settable,
    results: VecDeque<i32>,
    steps: VecDeque<i32>,
    writes: Vec<(&'static str, String)>,
    reads_after_withheld: usize,
    withheld: bool,
}

impl Fake {
    fn new(before: ValueRead) -> Self {
        Self {
            reads: VecDeque::from([before.clone()]),
            last: before,
            settable: Settable::Yes,
            results: VecDeque::new(),
            steps: VecDeque::new(),
            writes: Vec::new(),
            reads_after_withheld: 0,
            withheld: false,
        }
    }
    fn read(mut self, value: ValueRead) -> Self {
        self.reads.push_back(value);
        self
    }
    fn result(mut self, value: i32) -> Self {
        self.results.push_back(value);
        self
    }
    fn step(mut self, value: i32) -> Self {
        self.steps.push_back(value);
        self
    }
    fn unsettable(mut self) -> Self {
        self.settable = Settable::No;
        self
    }
}

impl AxValueOps for Fake {
    fn read_typed(&mut self) -> ValueRead {
        if self.withheld {
            self.reads_after_withheld += 1;
        }
        if let Some(read) = self.reads.pop_front() {
            self.last = read;
        }
        self.withheld |= matches!(self.last, ValueRead::Withheld);
        self.last.clone()
    }
    fn query_settable(&mut self) -> Settable {
        self.settable
    }
    fn set_number(&mut self, value: f64) -> i32 {
        self.writes.push(("number", value.to_string()));
        self.results.pop_front().unwrap_or(kAXErrorSuccess)
    }
    fn set_string(&mut self, value: &str) -> i32 {
        self.writes.push(("string", value.to_owned()));
        self.results.pop_front().unwrap_or(kAXErrorSuccess)
    }
    fn perform_step(&mut self, action: &str) -> i32 {
        self.writes.push(("step", action.to_owned()));
        self.steps.pop_front().unwrap_or(kAXErrorSuccess)
    }
    fn settle_pause(&mut self) {}
}

fn num(n: f64) -> ValueRead {
    ValueRead::Allowed(ValueObs::Number(n))
}
fn text(s: &str) -> ValueRead {
    ValueRead::Allowed(ValueObs::Text(s.to_owned()))
}
fn run(fake: &mut Fake, value: &str, role: &str, can_step: bool) -> LadderOutcome {
    write_value_ladder(
        fake,
        value,
        &LadderContext {
            secure: false,
            can_step,
            role,
        },
    )
}

fn project(
    out: LadderOutcome,
    role: &str,
    mark_count: &Cell<usize>,
) -> (ToolResult, bool, bool, bool) {
    let maybe_landed = out.maybe_landed;
    let not_applied = out.not_applied;
    let withheld = out.verification_withheld;
    let result = finish_ladder_with_document_readers(
        out,
        1,
        role,
        false,
        || {
            mark_count.set(mark_count.get() + 1);
            DocumentEdited::Marked
        },
        || false,
        || DocumentEdited::NotMarked,
    );
    (
        set_value_tool_result_with_toolkit(Ok(result), false, String::new(), 1, || false),
        not_applied,
        maybe_landed,
        withheld,
    )
}

fn assert_unsafe(result: &ToolResult) {
    assert_eq!(result.disposition, Some(ErrorDisposition::SUSPECTED_NOOP));
    let serialized = serde_json::to_value(result).unwrap();
    let expected = serde_json::json!({"delivery":"delivery_uncertain", "retry":"unsafe"});
    assert_eq!(serialized["structuredContent"]["disposition"], expected);
    let wire = cua_driver_core::mcp_result::conforming_tool_result("set_value", serialized);
    assert_eq!(wire["structuredContent"]["disposition"], expected);
}

#[test]
fn setter_illegal_argument_after_change_is_never_not_delivered() {
    let mut fake = Fake::new(text("old"))
        .read(text("new"))
        .result(kAXErrorIllegalArgument);
    let out = run(&mut fake, "new", "AXTextField", false);
    let (result, _, _, _) = project(out, "AXTextField", &Cell::new(0));
    assert_eq!(fake.writes, [("string", "new".to_owned())]);
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .and_then(|s| s.get("effect")),
        Some(&serde_json::json!("unverifiable"))
    );
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .and_then(|s| s.get("verified")),
        Some(&serde_json::json!(false))
    );
    assert_unsafe(&result);
}

#[test]
fn rejected_numeric_write_allows_only_one_string_alternate() {
    for (after, code) in [
        (num(5.0), kAXErrorSuccess),
        (num(0.0), kAXErrorSuccess),
        (num(0.0), kAXErrorCannotComplete),
    ] {
        let mut fake = Fake::new(num(0.0))
            .read(after.clone())
            .result(kAXErrorIllegalArgument)
            .result(code);
        let out = run(&mut fake, "5", "AXSlider", false);
        let (result, _, _, _) = project(out, "AXSlider", &Cell::new(0));
        assert_eq!(
            fake.writes,
            [("number", "5".into()), ("string", "5".into())]
        );
        if code == kAXErrorSuccess {
            let expected = if after == num(5.0) {
                "confirmed"
            } else {
                "unverifiable"
            };
            assert_eq!(
                result.structured_content.as_ref().unwrap()["effect"],
                expected
            );
            assert_ne!(
                result.structured_content.as_ref().unwrap()["code"],
                "write_not_applied"
            );
        }
    }
    for code in [kAXErrorAPIDisabled, kAXErrorCannotComplete] {
        let mut fake = Fake::new(num(0.0)).result(code);
        let _ = run(&mut fake, "5", "AXSlider", false);
        assert_eq!(fake.writes, [("number", "5".into())]);
    }
    let mut reverse = Fake::new(text("0")).result(kAXErrorIllegalArgument);
    let _ = run(&mut reverse, "5", "AXUnknown", false);
    assert_eq!(reverse.writes, [("string", "5".into())]);
    let mut step = Fake::new(num(0.0))
        .unsettable()
        .step(kAXErrorIllegalArgument);
    let _ = run(&mut step, "5", "AXSlider", true);
    assert_eq!(step.writes, [("step", "AXIncrement".into())]);
}

#[tokio::test]
async fn uncertain_delayed_match_stays_unverifiable_and_blocks_document_and_return() {
    let mark = Cell::new(0);
    let mut fake = Fake::new(num(0.0))
        .read(num(0.0))
        .read(num(5.0))
        .result(kAXErrorCannotComplete);
    let out = run(&mut fake, "5", "AXSlider", false);
    let (result, not_applied, maybe_landed, withheld) = project(out, "AXSlider", &mark);
    let returns = Cell::new(0);
    let result = super::super::set_value_autosubmit::finish(
        true,
        1,
        result,
        not_applied,
        maybe_landed,
        withheld,
        |_, _| async {
            returns.set(returns.get() + 1);
            Ok(true)
        },
    )
    .await;
    assert_eq!(
        result.structured_content.as_ref().unwrap()["effect"],
        "unverifiable"
    );
    assert_eq!(
        result.structured_content.as_ref().unwrap()["verified"],
        false
    );
    assert_eq!((mark.get(), returns.get()), (0, 0));
    let mut retry = Fake::new(num(0.0))
        .read(num(5.0))
        .result(kAXErrorIllegalArgument);
    let out = run(&mut retry, "5", "AXSlider", false);
    let (result, _, _, _) = project(out, "AXSlider", &mark);
    assert_eq!(result.structured_content.unwrap()["effect"], "confirmed");
    assert_eq!(mark.get(), 1);
}

#[test]
fn step_success_then_timeout_preserves_prior_delivery() {
    for error in [kAXErrorCannotComplete, kAXErrorAPIDisabled] {
        let mut fake = Fake::new(num(0.0))
            .unsettable()
            .read(num(1.0))
            .step(kAXErrorSuccess)
            .step(error);
        let out = run(&mut fake, "3", "AXSlider", true);
        let (result, _, _, _) = project(out, "AXSlider", &Cell::new(0));
        assert_eq!(
            fake.writes,
            [
                ("step", "AXIncrement".into()),
                ("step", "AXIncrement".into())
            ]
        );
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .and_then(|s| s.get("effect")),
            Some(&serde_json::json!("unverifiable"))
        );
        assert_unsafe(&result);
        assert!(format!("{:?}", result.content).contains("may have"));
    }
}

#[tokio::test]
async fn withheld_verification_never_compares_or_sends_followup() {
    struct SecureStep {
        reader: crate::ax::label_reader::fake::FakeReader,
        steps: usize,
    }
    impl AxValueOps for SecureStep {
        fn read_typed(&mut self) -> ValueRead {
            if self.steps > 0 {
                self.reader.set(0, "AXRole", "AXSecureTextField");
            }
            match read_value_if_allowed(&mut self.reader, 0) {
                ValueRead::Allowed(ValueObs::Text(value)) => {
                    ValueRead::Allowed(ValueObs::Number(value.parse().unwrap()))
                }
                other => other,
            }
        }
        fn query_settable(&mut self) -> Settable {
            Settable::No
        }
        fn set_number(&mut self, _: f64) -> i32 {
            panic!("unexpected setter")
        }
        fn set_string(&mut self, _: &str) -> i32 {
            panic!("unexpected setter")
        }
        fn perform_step(&mut self, _: &str) -> i32 {
            self.steps += 1;
            kAXErrorSuccess
        }
        fn settle_pause(&mut self) {
            panic!("unexpected comparison retry")
        }
    }
    let mut reader = crate::ax::label_reader::fake::FakeReader::default();
    reader.set(0, "AXRole", "AXSlider");
    reader.set(0, "AXValue", "0");
    let mut fake = SecureStep { reader, steps: 0 };
    let out = write_value_ladder(
        &mut fake,
        "2",
        &LadderContext {
            secure: false,
            can_step: true,
            role: "AXSlider",
        },
    );
    let (result, not_applied, maybe_landed, withheld) = project(out, "AXSlider", &Cell::new(0));
    let result = super::super::set_value_autosubmit::finish(
        true,
        1,
        result,
        not_applied,
        maybe_landed,
        withheld,
        |_, _| async { panic!("Return after withheld value") },
    )
    .await;
    assert!(withheld);
    assert_eq!(fake.steps, 1);
    assert_eq!(
        fake.reader
            .trace
            .iter()
            .filter(|(_, name)| name == "AXValue")
            .count(),
        1
    );
    assert_eq!(
        result.structured_content.as_ref().unwrap()["effect"],
        "unverifiable"
    );
    assert!(format!("{:?}", result.content).contains("saving or closing"));
}

#[test]
fn unchanged_text_preserves_not_applied_and_unsafe_disposition() {
    let mut fake = Fake::new(text("old"));
    let out = run(&mut fake, "new", "AXTextField", false);
    let (result, _, _, _) = project(out, "AXTextField", &Cell::new(0));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["code"],
        "write_not_applied"
    );
    assert_eq!(
        result.structured_content.as_ref().unwrap()["effect"],
        "not_applied"
    );
    assert_unsafe(&result);
    for retry in [false, true] {
        let mut numeric = Fake::new(num(0.0));
        if retry {
            numeric = numeric.result(kAXErrorIllegalArgument);
        }
        let out = run(&mut numeric, "5", "AXSlider", false);
        let (result, _, _, _) = project(out, "AXSlider", &Cell::new(0));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["effect"],
            "unverifiable"
        );
        assert_ne!(
            result.structured_content.as_ref().unwrap()["code"],
            "write_not_applied"
        );
    }
}
