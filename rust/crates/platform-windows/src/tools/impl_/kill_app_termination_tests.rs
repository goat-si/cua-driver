use super::*;
use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command, Stdio};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn child_fixture() {
    if std::env::var_os("CUA_W5_TEST_CHILD").is_some() {
        loop {
            std::thread::park();
        }
    }
}

fn child() -> ChildGuard {
    ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::impl_::kill_app_termination::tests::child_fixture",
            ])
            .env("CUA_W5_TEST_CHILD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn child_termination_requires_same_handle_identity() {
    let mut child = child();
    let handle = HANDLE(child.0.as_raw_handle());
    let (start, executable) =
        crate::browser_platform::process_identity_from_handle(handle).unwrap();
    let wrong = (start ^ 1, executable.clone());
    assert_eq!(
        terminate_pid(child.0.id(), Some(wrong)),
        TerminationResult::Stale
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "stale identity must not terminate child"
    );
    assert_eq!(
        terminate_pid(child.0.id(), Some((start, executable))),
        TerminationResult::Outcome(TerminationOutcome::Exited)
    );
    assert!(child.0.wait().is_ok());
}

#[test]
fn open_access_denied_is_reported_as_refusal() {
    let result = serde_json::to_value(tool_result(123, TerminationResult::OpenFailed(5))).unwrap();
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["effect"], "refused");
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("access denied"));
}

#[test]
fn termination_result_reports_unobserved_exits() {
    for (outcome, expected) in [
        (
            TerminationOutcome::RequestedNotConfirmed,
            "had not exited within 2 s",
        ),
        (
            TerminationOutcome::ExitUnobserved { os_error: 6 },
            "WaitForSingleObject failed: 6",
        ),
    ] {
        let result =
            serde_json::to_value(tool_result(123, TerminationResult::Outcome(outcome))).unwrap();
        assert_eq!(result["structuredContent"]["effect"], "unverifiable");
        assert_eq!(result["structuredContent"]["verified"], false);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(expected));
    }
}

#[test]
fn confirmed_exit_preserves_success_text() {
    let result = serde_json::to_value(tool_result(
        123,
        TerminationResult::Outcome(TerminationOutcome::Exited),
    ))
    .unwrap();
    assert_eq!(result["content"][0]["text"], "✅ Terminated pid 123.");
}
