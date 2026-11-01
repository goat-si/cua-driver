use crate::process_termination::{
    run_termination, TerminationOutcome, TerminationResult, WaitResult,
};
use cua_driver_core::protocol::ToolResult;
use serde_json::json;
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn win32_error(error: windows::core::Error) -> u32 {
    let code = error.code().0 as u32;
    if code & 0xffff_0000 == 0x8007_0000 {
        code & 0xffff
    } else {
        code
    }
}

pub(super) fn terminate_pid(
    pid: u32,
    expected: Option<(u64, Option<String>)>,
) -> TerminationResult {
    run_termination(
        || {
            unsafe {
                OpenProcess(
                    PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                    false,
                    pid,
                )
            }
            .map(ProcessHandle)
            .map_err(win32_error)
        },
        |handle| crate::browser_platform::process_identity_from_handle(handle.0).ok(),
        expected,
        |handle| unsafe { TerminateProcess(handle.0, 1) }.map_err(win32_error),
        |handle| {
            let status = unsafe { WaitForSingleObject(handle.0, 2000) };
            if status == WAIT_OBJECT_0 {
                WaitResult::Signaled
            } else if status == WAIT_TIMEOUT {
                WaitResult::TimedOut
            } else {
                WaitResult::Failed(unsafe { GetLastError().0 })
            }
        },
    )
}

pub(super) fn tool_result(pid: u32, result: TerminationResult) -> ToolResult {
    match result {
        TerminationResult::Stale => super::kill_app_stale_process_refusal("the process at this pid is not the approved instance".to_owned()),
        TerminationResult::OpenFailed(code) => {
            let detail = if code == 5 { "access denied; " } else { "" };
            ToolResult::error(format!("kill_app: OpenProcess(pid={pid}) failed: {detail}OS error {code}."))
                .with_structured(json!({"effect":"refused", "os_error":code}))
        }
        TerminationResult::Rejected(code) => ToolResult::error(format!("kill_app: TerminateProcess(pid={pid}) failed: OS error {code}. The handle was opened successfully but the kill itself was rejected; this is unusual for PROCESS_TERMINATE rights and may indicate a protected-process target (PPL, e.g. csrss / system services).")),
        TerminationResult::Outcome(TerminationOutcome::Exited) => ToolResult::text(format!("✅ Terminated pid {pid}.")),
        TerminationResult::Outcome(TerminationOutcome::RequestedNotConfirmed) => ToolResult::text(format!("Termination of pid {pid} was requested but the process had not exited within 2 s. Check list_apps before relying on it.")).with_structured(json!({"effect":"unverifiable", "verified":false})),
        TerminationResult::Outcome(TerminationOutcome::ExitUnobserved { os_error }) => ToolResult::text(format!("Termination of pid {pid} was requested, but its exit could not be observed (WaitForSingleObject failed: {os_error}). Check list_apps before relying on it.")).with_structured(json!({"effect":"unverifiable", "verified":false})),
        TerminationResult::Outcome(TerminationOutcome::Rejected) => unreachable!("driver handles termination errors before waiting"),
    }
}

#[cfg(test)]
#[path = "kill_app_termination_tests.rs"]
mod tests;
