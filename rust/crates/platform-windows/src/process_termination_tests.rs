use super::*;
use std::cell::Cell;

#[test]
fn process_termination_wait_outcomes() {
    assert_eq!(
        termination_outcome(true, WaitResult::Signaled),
        TerminationOutcome::Exited
    );
    assert_eq!(
        termination_outcome(true, WaitResult::TimedOut),
        TerminationOutcome::RequestedNotConfirmed
    );
    assert_eq!(
        termination_outcome(true, WaitResult::Failed(6)),
        TerminationOutcome::ExitUnobserved { os_error: 6 }
    );
    for wait in [
        WaitResult::Signaled,
        WaitResult::TimedOut,
        WaitResult::Failed(6),
    ] {
        assert_eq!(
            termination_outcome(false, wait),
            TerminationOutcome::Rejected
        );
    }
}

#[test]
fn process_termination_identity_table() {
    let expected = Some((123, Some("app.exe")));
    assert!(identity_matches(None, None));
    assert!(!identity_matches(expected, None));
    assert!(!identity_matches(expected, Some((124, Some("app.exe")))));
    assert!(!identity_matches(expected, Some((123, Some("other.exe")))));
    assert!(identity_matches(expected, Some((123, Some("app.exe")))));
    assert!(identity_matches(expected, Some((123, None))));
    assert!(identity_matches(
        Some((123, None)),
        Some((123, Some("app.exe")))
    ));
}

#[test]
fn process_termination_refuses_before_destructive_call() {
    for actual in [
        None,
        Some((124, Some("app.exe".to_owned()))),
        Some((123, Some("other.exe".to_owned()))),
    ] {
        let terminated = Cell::new(0);
        let waited = Cell::new(0);
        let result = run_termination(
            || Ok(7),
            |_| actual,
            Some((123, Some("app.exe".to_owned()))),
            |_| {
                terminated.set(terminated.get() + 1);
                Ok(())
            },
            |_| {
                waited.set(waited.get() + 1);
                WaitResult::Signaled
            },
        );
        assert_eq!(result, TerminationResult::Stale);
        assert_eq!((terminated.get(), waited.get()), (0, 0));
    }
    let called = Cell::new(0);
    let result = run_termination(
        || Err::<u32, _>(5),
        |_| {
            called.set(called.get() + 1);
            None
        },
        None,
        |_| {
            called.set(called.get() + 1);
            Ok(())
        },
        |_| {
            called.set(called.get() + 1);
            WaitResult::Signaled
        },
    );
    assert_eq!(result, TerminationResult::OpenFailed(5));
    assert_eq!(called.get(), 0);
}

#[test]
fn process_termination_uses_same_handle_and_stops_on_errors() {
    let waits = Cell::new(0);
    let address = Cell::new(0);
    let result = run_termination(
        || Ok(123),
        |h| {
            assert_eq!(*h, 123);
            address.set(h as *const i32 as usize);
            Some((1, None))
        },
        Some((1, None)),
        |h| {
            assert_eq!(address.get(), h as *const i32 as usize);
            Err(5)
        },
        |_| {
            waits.set(waits.get() + 1);
            WaitResult::Signaled
        },
    );
    assert_eq!(result, TerminationResult::Rejected(5));
    assert_eq!(waits.get(), 0);
    let result = run_termination(
        || Ok(123),
        |h| {
            assert_eq!(*h, 123);
            Some((1, None))
        },
        Some((1, None)),
        |h| {
            assert_eq!(*h, 123);
            Ok(())
        },
        |h| {
            assert_eq!(*h, 123);
            WaitResult::Failed(6)
        },
    );
    assert_eq!(
        result,
        TerminationResult::Outcome(TerminationOutcome::ExitUnobserved { os_error: 6 })
    );
}

#[test]
fn process_termination_worker_failure_is_unverifiable() {
    let result = worker_failure_result();
    let encoded = serde_json::to_value(result).unwrap();
    assert_eq!(encoded["isError"], true);
    assert_eq!(encoded["structuredContent"]["effect"], "unverifiable");
    assert_eq!(encoded["structuredContent"]["verified"], false);
    assert_eq!(encoded["content"][0]["text"], "kill_app: the termination worker failed; the process may or may not have been terminated. Check list_apps.");
}

#[test]
fn process_termination_identity_refuses_unreadable_and_changed_process() {
    assert!(!identity_matches(Some((1, Some("app.exe"))), None));
    assert!(!identity_matches(
        Some((1, Some("app.exe"))),
        Some((2, Some("app.exe")))
    ));
}

#[test]
fn process_termination_changed_identity_never_calls_terminate() {
    let calls = Cell::new(0);
    let result = run_termination(
        || Ok(7),
        |_| Some((2, None)),
        Some((1, None)),
        |_| {
            calls.set(calls.get() + 1);
            Ok(())
        },
        |_| WaitResult::Signaled,
    );
    assert_eq!(calls.get(), 0);
    assert_eq!(result, TerminationResult::Stale);
}

#[test]
fn process_termination_without_fingerprint_keeps_existing_access() {
    let identity_calls = Cell::new(0);
    let termination_calls = Cell::new(0);
    let result = run_termination(
        || Ok(42),
        |_| {
            identity_calls.set(identity_calls.get() + 1);
            None
        },
        None,
        |_| {
            termination_calls.set(termination_calls.get() + 1);
            Ok(())
        },
        |_| WaitResult::Signaled,
    );
    assert_eq!(
        result,
        TerminationResult::Outcome(TerminationOutcome::Exited)
    );
    assert_eq!((identity_calls.get(), termination_calls.get()), (1, 1));
}
