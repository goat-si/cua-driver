//! Plain-Rust policy for editable text mutations. The adapter owns error classification.
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallClass {
    PreDispatchUnavailable,
    KnownUnsupported,
    PossiblyExecuted,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationResult {
    Acknowledged,
    Refused,
}
pub type MutationReply = Result<MutationResult, CallClass>;

// One atomic encodes both the current state and whether entry ever occurred.
const OPEN: u8 = 0;
const REARMED: u8 = 1;
const DISPATCHING: u8 = 2;
const CLOSED_UNENTERED: u8 = 3;
const CLOSED_ENTERED: u8 = 4;

#[derive(Default)]
pub struct DispatchGate(AtomicU8);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateError {
    Closed,
    Busy,
}
impl DispatchGate {
    pub fn enter_mutator(&self) -> Result<(), GateError> {
        let mut state = self.0.load(Ordering::SeqCst);
        loop {
            match state {
                OPEN | REARMED => match self.0.compare_exchange(
                    state,
                    DISPATCHING,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => return Ok(()),
                    Err(next) => state = next,
                },
                DISPATCHING => return Err(GateError::Busy),
                _ => return Err(GateError::Closed),
            }
        }
    }
    pub fn finish_mutator(&self) {
        let _ = self
            .0
            .compare_exchange(DISPATCHING, REARMED, Ordering::SeqCst, Ordering::SeqCst);
    }
    /// Atomically closes the gate and returns whether a mutator ever entered.
    pub fn close_on_timeout(&self) -> bool {
        let mut state = self.0.load(Ordering::SeqCst);
        loop {
            let (closed, entered) = match state {
                OPEN | CLOSED_UNENTERED => (CLOSED_UNENTERED, false),
                _ => (CLOSED_ENTERED, true),
            };
            match self
                .0
                .compare_exchange(state, closed, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return entered,
                Err(next) => state = next,
            }
        }
    }
}

const SENT: u8 = 1;
const ACKNOWLEDGED: u8 = 2;
const POSSIBLY_EXECUTED: u8 = 4;
#[derive(Default)]
pub struct DispatchLedger(AtomicU8);
impl DispatchLedger {
    pub fn record_sent(&self) {
        self.0.fetch_or(SENT, Ordering::SeqCst);
    }
    pub fn record_result(&self, result: MutationReply) {
        let bit = match result {
            Ok(MutationResult::Acknowledged) => ACKNOWLEDGED,
            Err(CallClass::PossiblyExecuted) => POSSIBLY_EXECUTED,
            _ => 0,
        };
        self.0.fetch_or(bit, Ordering::SeqCst);
    }
    pub fn sent(&self) -> bool {
        self.0.load(Ordering::SeqCst) & SENT != 0
    }
    pub fn acknowledged(&self) -> bool {
        self.0.load(Ordering::SeqCst) & ACKNOWLEDGED != 0
    }
    pub fn possibly_executed(&self) -> bool {
        self.0.load(Ordering::SeqCst) & POSSIBLY_EXECUTED != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Toolkit {
    Gtk,
    Qt,
    Unknown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reason {
    CaretUnavailable,
    InsertUnavailable,
    CountUnavailable,
    UnsupportedUnits,
    LengthOverflow,
    Refused,
    Unsupported,
    GateClosed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    SetText,
    DeleteText,
    InsertText,
    Commit,
    SetValue,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteStep {
    pub step: Step,
    pub bytes: usize,
    pub units: Option<i32>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextWriteOutcome {
    Acknowledged,
    NothingWritten { reason: Reason },
    Uncertain { step: WriteStep },
    Partial { step: WriteStep },
}
impl fmt::Display for TextWriteOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

pub fn checked_length(length: usize) -> Result<i32, Reason> {
    i32::try_from(length).map_err(|_| Reason::LengthOverflow)
}
pub fn text_units(text: &str, toolkit: Toolkit) -> Result<i32, Reason> {
    let length = match toolkit {
        Toolkit::Gtk => text.len(),
        Toolkit::Qt => text.encode_utf16().count(),
        Toolkit::Unknown if text.is_ascii() => text.len(),
        Toolkit::Unknown => return Err(Reason::UnsupportedUnits),
    };
    checked_length(length)
}

pub trait EditableOps {
    fn caret(&mut self) -> Result<i32, CallClass>;
    fn character_count(&mut self) -> Result<i32, CallClass>;
    fn validate_insert(&mut self) -> Result<(), CallClass>;
    fn set_text(&mut self, text: &str) -> MutationReply;
    fn delete_text(&mut self, start: i32, end: i32) -> MutationReply;
    fn insert_text(&mut self, offset: i32, text: &str, length: i32) -> MutationReply;
    fn prepare_commit(&mut self) -> Result<bool, CallClass> {
        Ok(true)
    }
    fn commit(&mut self) -> MutationReply;
}

fn dispatch(
    gate: &DispatchGate,
    ledger: &DispatchLedger,
    f: impl FnOnce() -> MutationReply,
) -> Result<MutationReply, GateError> {
    gate.enter_mutator()?;
    ledger.record_sent();
    let result = f();
    ledger.record_result(result);
    gate.finish_mutator();
    Ok(result)
}
fn no(reason: Reason) -> TextWriteOutcome {
    TextWriteOutcome::NothingWritten { reason }
}
fn step(step: Step, text: &str, units: Option<i32>) -> WriteStep {
    WriteStep {
        step,
        bytes: text.len(),
        units,
    }
}
fn failure_reason(reply: MutationReply) -> Reason {
    match reply {
        Ok(MutationResult::Refused) => Reason::Refused,
        Err(CallClass::KnownUnsupported) => Reason::Unsupported,
        _ => Reason::InsertUnavailable,
    }
}

pub fn insert_route(
    ops: &mut impl EditableOps,
    text: &str,
    toolkit: Toolkit,
    gate: &DispatchGate,
    ledger: &DispatchLedger,
) -> TextWriteOutcome {
    let caret = match ops.caret() {
        Ok(v) => v,
        Err(_) => return no(Reason::CaretUnavailable),
    };
    let units = match text_units(text, toolkit) {
        Ok(v) => v,
        Err(e) => return no(e),
    };
    match dispatch(gate, ledger, || ops.insert_text(caret, text, units)) {
        Ok(Ok(MutationResult::Acknowledged)) => TextWriteOutcome::Acknowledged,
        Ok(Err(CallClass::PossiblyExecuted)) => TextWriteOutcome::Uncertain {
            step: step(Step::InsertText, text, Some(units)),
        },
        Ok(reply) => no(failure_reason(reply)),
        Err(_) => no(Reason::GateClosed),
    }
}

pub fn replace_route(
    ops: &mut impl EditableOps,
    text: &str,
    toolkit: Toolkit,
    gate: &DispatchGate,
    ledger: &DispatchLedger,
) -> TextWriteOutcome {
    // Check any representable insert length before the first mutator; unknown non-ASCII
    // can still be handled by SetTextContents, but cannot use the delete/insert fallback.
    let units = text_units(text, toolkit);
    if units == Err(Reason::LengthOverflow) {
        return no(Reason::LengthOverflow);
    }
    match dispatch(gate, ledger, || ops.set_text(text)) {
        Ok(Ok(MutationResult::Acknowledged)) => return commit(ops, text, units.ok(), gate, ledger),
        Ok(Err(CallClass::PossiblyExecuted)) => {
            return TextWriteOutcome::Uncertain {
                step: step(Step::SetText, text, units.ok()),
            }
        }
        Ok(Err(CallClass::PreDispatchUnavailable)) => return no(Reason::InsertUnavailable),
        Err(_) => return no(Reason::GateClosed),
        _ => {}
    }
    let units = match units {
        Ok(v) => v,
        Err(e) => return no(e),
    };
    if ops.validate_insert().is_err() {
        return no(Reason::InsertUnavailable);
    }
    let count = match ops.character_count() {
        Ok(v) if v >= 0 => v,
        _ => return no(Reason::CountUnavailable),
    };
    match dispatch(gate, ledger, || ops.delete_text(0, count)) {
        Ok(Ok(MutationResult::Acknowledged)) => {}
        Ok(Err(CallClass::PossiblyExecuted)) => {
            return TextWriteOutcome::Uncertain {
                step: step(Step::DeleteText, text, Some(units)),
            }
        }
        Ok(reply) => return no(failure_reason(reply)),
        Err(_) => return no(Reason::GateClosed),
    }
    match dispatch(gate, ledger, || ops.insert_text(0, text, units)) {
        Ok(Ok(MutationResult::Acknowledged)) => commit(ops, text, Some(units), gate, ledger),
        _ => TextWriteOutcome::Partial {
            step: step(Step::InsertText, text, Some(units)),
        },
    }
}
fn commit(
    ops: &mut impl EditableOps,
    text: &str,
    units: Option<i32>,
    gate: &DispatchGate,
    ledger: &DispatchLedger,
) -> TextWriteOutcome {
    match ops.prepare_commit() {
        Ok(false) => return TextWriteOutcome::Acknowledged,
        Ok(true) => {}
        Err(_) => {
            return TextWriteOutcome::Uncertain {
                step: step(Step::Commit, text, units),
            }
        }
    }
    match dispatch(gate, ledger, || ops.commit()) {
        Ok(Ok(MutationResult::Acknowledged)) => TextWriteOutcome::Acknowledged,
        _ => TextWriteOutcome::Uncertain {
            step: step(Step::Commit, text, units),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    type Reply = Result<MutationResult, CallClass>;
    const ACK: Reply = Ok(MutationResult::Acknowledged);
    const NO: Reply = Ok(MutationResult::Refused);
    const UNSUPPORTED: Reply = Err(CallClass::KnownUnsupported);
    const MAYBE: Reply = Err(CallClass::PossiblyExecuted);

    struct Fake {
        calls: Vec<&'static str>,
        set: Reply,
        delete: Reply,
        insert: Reply,
        commit: Reply,
        caret: Result<i32, CallClass>,
        count: Result<i32, CallClass>,
        valid: Result<(), CallClass>,
        insert_args: Vec<(i32, i32)>,
        delete_args: Vec<(i32, i32)>,
        pause: Option<(&'static str, Arc<Barrier>, Arc<Barrier>)>,
        close_on_prepare: Option<Arc<DispatchGate>>,
        no_commit: bool,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                calls: vec![],
                set: ACK,
                delete: ACK,
                insert: ACK,
                commit: ACK,
                caret: Ok(7),
                count: Ok(5),
                valid: Ok(()),
                insert_args: vec![],
                delete_args: vec![],
                pause: None,
                close_on_prepare: None,
                no_commit: false,
            }
        }
    }
    impl Fake {
        fn call(&mut self, name: &'static str) {
            self.calls.push(name);
            if let Some((at, entered, released)) = &self.pause {
                if *at == name {
                    entered.wait();
                    released.wait();
                }
            }
        }
    }
    impl EditableOps for Fake {
        fn caret(&mut self) -> Result<i32, CallClass> {
            self.call("caret");
            self.caret
        }
        fn character_count(&mut self) -> Result<i32, CallClass> {
            self.call("count");
            self.count
        }
        fn validate_insert(&mut self) -> Result<(), CallClass> {
            self.call("validate");
            self.valid
        }
        fn set_text(&mut self, _: &str) -> Reply {
            self.call("set");
            self.set
        }
        fn delete_text(&mut self, start: i32, end: i32) -> Reply {
            self.delete_args.push((start, end));
            self.call("delete");
            self.delete
        }
        fn insert_text(&mut self, offset: i32, _: &str, length: i32) -> Reply {
            self.insert_args.push((offset, length));
            self.call("insert");
            self.insert
        }
        fn prepare_commit(&mut self) -> Result<bool, CallClass> {
            if let Some(gate) = &self.close_on_prepare {
                self.calls.push("prepare");
                gate.close_on_timeout();
            }
            Ok(!self.no_commit)
        }
        fn commit(&mut self) -> Reply {
            self.call("commit");
            self.commit
        }
    }
    fn insert(f: &mut Fake, text: &str) -> TextWriteOutcome {
        insert_route(
            f,
            text,
            Toolkit::Gtk,
            &DispatchGate::default(),
            &DispatchLedger::default(),
        )
    }
    fn replace(f: &mut Fake, text: &str) -> TextWriteOutcome {
        replace_route(
            f,
            text,
            Toolkit::Gtk,
            &DispatchGate::default(),
            &DispatchLedger::default(),
        )
    }
    #[test]
    fn toolkit_length_units_and_overflow() {
        assert_eq!(text_units("é😀", Toolkit::Gtk), Ok(6));
        assert_eq!(text_units("é😀", Toolkit::Qt), Ok(3));
        assert!(text_units("é😀", Toolkit::Unknown).is_err());
        assert_eq!(text_units("abc", Toolkit::Unknown), Ok(3));
        assert!(checked_length(i32::MAX as usize + 1).is_err());
    }
    #[test]
    fn insert_passes_caret_and_toolkit_units_unchanged() {
        for (toolkit, length) in [(Toolkit::Gtk, 6), (Toolkit::Qt, 3)] {
            let mut f = Fake::default();
            assert_eq!(
                insert_route(
                    &mut f,
                    "é😀",
                    toolkit,
                    &DispatchGate::default(),
                    &DispatchLedger::default()
                ),
                TextWriteOutcome::Acknowledged
            );
            assert_eq!(f.insert_args, [(7, length)]);
        }
    }
    #[test]
    fn refused_insert_never_replaces_even_when_empty() {
        for count in [0, 5] {
            let mut f = Fake {
                insert: NO,
                count: Ok(count),
                ..Fake::default()
            };
            assert!(matches!(
                insert(&mut f, "abc"),
                TextWriteOutcome::NothingWritten { .. }
            ));
            assert_eq!(f.calls, ["caret", "insert"]);
        }
    }
    #[test]
    fn insert_possibly_executed_stops() {
        let mut f = Fake {
            insert: MAYBE,
            ..Fake::default()
        };
        assert!(matches!(
            insert(&mut f, "abc"),
            TextWriteOutcome::Uncertain { .. }
        ));
        assert_eq!(f.calls, ["caret", "insert"]);
    }
    #[test]
    fn caret_failure_and_unknown_units_send_nothing() {
        let mut f = Fake {
            caret: Err(CallClass::PreDispatchUnavailable),
            ..Fake::default()
        };
        assert!(matches!(
            insert(&mut f, "abc"),
            TextWriteOutcome::NothingWritten { .. }
        ));
        assert_eq!(f.calls, ["caret"]);
        let mut f = Fake::default();
        let out = insert_route(
            &mut f,
            "é",
            Toolkit::Unknown,
            &DispatchGate::default(),
            &DispatchLedger::default(),
        );
        assert!(matches!(out, TextWriteOutcome::NothingWritten { .. }));
        assert!(!f.calls.contains(&"insert"));
    }
    #[test]
    fn replace_uncertain_set_stops() {
        let mut f = Fake {
            set: MAYBE,
            ..Fake::default()
        };
        assert!(matches!(
            replace(&mut f, "abc"),
            TextWriteOutcome::Uncertain { .. }
        ));
        assert_eq!(f.calls, ["set"]);
    }
    #[test]
    fn refused_or_unsupported_set_deletes_inserts_and_commits() {
        for reply in [NO, UNSUPPORTED] {
            let mut f = Fake {
                set: reply,
                ..Fake::default()
            };
            assert_eq!(replace(&mut f, "abc"), TextWriteOutcome::Acknowledged);
            assert_eq!(
                f.calls,
                ["set", "validate", "count", "delete", "insert", "commit"]
            );
            assert_eq!(f.delete_args, [(0, 5)]);
            assert_eq!(f.insert_args, [(0, 3)]);
        }
    }
    #[test]
    fn delete_refusal_or_unsupported_is_nothing_written() {
        for reply in [NO, UNSUPPORTED] {
            let mut f = Fake {
                set: NO,
                delete: reply,
                ..Fake::default()
            };
            assert!(matches!(
                replace(&mut f, "abc"),
                TextWriteOutcome::NothingWritten { .. }
            ));
            assert_eq!(f.calls, ["set", "validate", "count", "delete"]);
        }
    }
    #[test]
    fn delete_ack_then_failed_insert_is_partial() {
        for reply in [
            NO,
            UNSUPPORTED,
            MAYBE,
            Err(CallClass::PreDispatchUnavailable),
        ] {
            let mut f = Fake {
                set: NO,
                insert: reply,
                ..Fake::default()
            };
            assert!(matches!(
                replace(&mut f, "abc"),
                TextWriteOutcome::Partial { .. }
            ));
            assert_eq!(f.calls, ["set", "validate", "count", "delete", "insert"]);
        }
    }
    #[test]
    fn validate_before_delete_and_uncertain_delete_stops() {
        let mut f = Fake {
            set: NO,
            valid: Err(CallClass::PreDispatchUnavailable),
            ..Fake::default()
        };
        assert!(matches!(
            replace(&mut f, "abc"),
            TextWriteOutcome::NothingWritten { .. }
        ));
        assert_eq!(f.calls, ["set", "validate"]);
        let mut f = Fake {
            set: NO,
            delete: MAYBE,
            ..Fake::default()
        };
        assert!(matches!(
            replace(&mut f, "abc"),
            TextWriteOutcome::Uncertain { .. }
        ));
        assert_eq!(f.calls, ["set", "validate", "count", "delete"]);
    }
    #[test]
    fn commit_only_after_success() {
        let mut f = Fake::default();
        assert_eq!(replace(&mut f, "abc"), TextWriteOutcome::Acknowledged);
        assert_eq!(f.calls, ["set", "commit"]);
        let mut f = Fake {
            set: MAYBE,
            ..Fake::default()
        };
        let _ = replace(&mut f, "abc");
        assert!(!f.calls.contains(&"commit"));
    }
    #[test]
    fn timeout_after_set_ack_blocks_commit() {
        let gate = Arc::new(DispatchGate::default());
        let entered = Arc::new(Barrier::new(2));
        let released = Arc::new(Barrier::new(2));
        let g = gate.clone();
        let a = entered.clone();
        let b = released.clone();
        let thread = std::thread::spawn(move || {
            let mut f = Fake {
                pause: Some(("set", a, b)),
                ..Fake::default()
            };
            let out = replace_route(&mut f, "abc", Toolkit::Gtk, &g, &DispatchLedger::default());
            (out, f.calls)
        });
        entered.wait();
        assert!(gate.close_on_timeout());
        released.wait();
        let (out, calls) = thread.join().unwrap();
        assert!(matches!(
            out,
            TextWriteOutcome::Uncertain {
                step: WriteStep {
                    step: Step::Commit,
                    ..
                }
            }
        ));
        assert_eq!(calls, ["set"]);
    }

    #[test]
    fn timeout_during_commit_preparation_blocks_action() {
        let gate = Arc::new(DispatchGate::default());
        let mut f = Fake {
            close_on_prepare: Some(gate.clone()),
            ..Fake::default()
        };
        let out = replace_route(
            &mut f,
            "abc",
            Toolkit::Gtk,
            &gate,
            &DispatchLedger::default(),
        );
        assert!(matches!(
            out,
            TextWriteOutcome::Uncertain {
                step: WriteStep {
                    step: Step::Commit,
                    ..
                }
            }
        ));
        assert_eq!(f.calls, ["set", "prepare"]);
    }

    #[test]
    fn absent_activation_does_not_dispatch_a_commit() {
        let mut f = Fake {
            no_commit: true,
            ..Fake::default()
        };
        assert_eq!(replace(&mut f, "abc"), TextWriteOutcome::Acknowledged);
        assert_eq!(f.calls, ["set"]);
    }
    #[test]
    fn timeout_before_first_send_closes_gate() {
        let gate = Arc::new(DispatchGate::default());
        let entered = Arc::new(Barrier::new(2));
        let released = Arc::new(Barrier::new(2));
        let g = gate.clone();
        let a = entered.clone();
        let b = released.clone();
        let thread = std::thread::spawn(move || {
            let mut f = Fake {
                pause: Some(("caret", a, b)),
                ..Fake::default()
            };
            let out = insert_route(&mut f, "abc", Toolkit::Gtk, &g, &DispatchLedger::default());
            (out, f.calls)
        });
        entered.wait();
        assert!(!gate.close_on_timeout());
        released.wait();
        let (out, calls) = thread.join().unwrap();
        assert!(matches!(out, TextWriteOutcome::NothingWritten { .. }));
        assert_eq!(calls, ["caret"]);
        assert!(gate.enter_mutator().is_err());
    }
    #[test]
    fn timeout_between_delete_and_insert_is_partial() {
        let gate = Arc::new(DispatchGate::default());
        let entered = Arc::new(Barrier::new(2));
        let released = Arc::new(Barrier::new(2));
        let g = gate.clone();
        let a = entered.clone();
        let b = released.clone();
        let thread = std::thread::spawn(move || {
            let mut f = Fake {
                set: NO,
                pause: Some(("delete", a, b)),
                ..Fake::default()
            };
            let out = replace_route(&mut f, "abc", Toolkit::Gtk, &g, &DispatchLedger::default());
            (out, f.calls)
        });
        entered.wait();
        assert!(gate.close_on_timeout());
        released.wait();
        let (out, calls) = thread.join().unwrap();
        assert!(matches!(out, TextWriteOutcome::Partial { .. }));
        assert_eq!(calls, ["set", "validate", "count", "delete"]);
        assert!(gate.enter_mutator().is_err());
    }
    #[test]
    fn outcome_format_never_contains_input() {
        let secret = "private-é😀";
        let mut f = Fake {
            insert: MAYBE,
            ..Fake::default()
        };
        let out = insert(&mut f, secret);
        assert!(!format!("{out}").contains(secret));
        assert!(!format!("{out:?}").contains(secret));
    }
    #[test]
    fn ledger_and_gate_remain_monotonic() {
        let gate = DispatchGate::default();
        let ledger = DispatchLedger::default();
        gate.enter_mutator().unwrap();
        ledger.record_sent();
        ledger.record_result(MAYBE);
        gate.finish_mutator();
        assert!(gate.close_on_timeout());
        gate.finish_mutator();
        assert!(gate.enter_mutator().is_err());
        assert!(ledger.sent());
        assert!(ledger.possibly_executed());
        ledger.record_result(ACK);
        assert!(ledger.acknowledged());
        assert!(ledger.possibly_executed());
    }
}
