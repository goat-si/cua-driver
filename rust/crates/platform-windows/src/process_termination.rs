use cua_driver_core::protocol::ToolResult;
use serde_json::json;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WaitResult {
    Signaled,
    TimedOut,
    Failed(u32),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TerminationOutcome {
    Exited,
    RequestedNotConfirmed,
    ExitUnobserved { os_error: u32 },
    Rejected,
}

pub(crate) fn termination_outcome(terminated: bool, wait: WaitResult) -> TerminationOutcome {
    match (terminated, wait) {
        (false, _) => TerminationOutcome::Rejected,
        (true, WaitResult::Signaled) => TerminationOutcome::Exited,
        (true, WaitResult::TimedOut) => TerminationOutcome::RequestedNotConfirmed,
        (true, WaitResult::Failed(os_error)) => TerminationOutcome::ExitUnobserved { os_error },
    }
}

pub(crate) fn identity_matches(
    expected: Option<(u64, Option<&str>)>,
    from_handle: Option<(u64, Option<&str>)>,
) -> bool {
    match (expected, from_handle) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some((expected_start, expected_exe)), Some((start, exe))) => {
            expected_start == start
                && match (expected_exe, exe) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TerminationResult {
    OpenFailed(u32),
    Stale,
    Rejected(u32),
    Outcome(TerminationOutcome),
}

pub(crate) fn run_termination<H>(
    open: impl FnOnce() -> Result<H, u32>,
    identity: impl FnOnce(&H) -> Option<(u64, Option<String>)>,
    expected: Option<(u64, Option<String>)>,
    terminate: impl FnOnce(&H) -> Result<(), u32>,
    wait: impl FnOnce(&H) -> WaitResult,
) -> TerminationResult {
    let handle = match open() {
        Ok(handle) => handle,
        Err(error) => return TerminationResult::OpenFailed(error),
    };
    let actual = identity(&handle);
    let expected = expected
        .as_ref()
        .map(|(start, exe)| (*start, exe.as_deref()));
    let actual = actual.as_ref().map(|(start, exe)| (*start, exe.as_deref()));
    if !identity_matches(expected, actual) {
        return TerminationResult::Stale;
    }
    if let Err(error) = terminate(&handle) {
        return TerminationResult::Rejected(error);
    }
    TerminationResult::Outcome(termination_outcome(true, wait(&handle)))
}

pub(crate) fn worker_failure_result() -> ToolResult {
    ToolResult::error("kill_app: the termination worker failed; the process may or may not have been terminated. Check list_apps.")
        .with_structured(json!({"effect": "unverifiable", "verified": false}))
}

#[cfg(test)]
#[path = "process_termination_tests.rs"]
mod tests;
