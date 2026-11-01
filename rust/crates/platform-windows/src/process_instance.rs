use cua_driver_core::element_token::ResolvedElement;
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::snapshot_store::{
    CaptureTicket, InstanceCapture, InstanceEvidence, SnapshotPayload, SnapshotStore,
};
use cua_driver_core::window_basis::WindowBasis;

use cua_driver_core::browser::types::ProcessFingerprint;

pub(crate) struct Probe {
    pub(crate) fingerprint: Result<ProcessFingerprint, String>,
    pub(crate) window_owner: Result<u32, String>,
}

pub(crate) fn instance_capture(pid: u32, before: Probe, after: Probe) -> InstanceCapture {
    let capture = || -> Result<ProcessFingerprint, String> {
        let before_owner = before.window_owner?;
        let after_owner = after.window_owner?;
        if before_owner != pid || after_owner != pid {
            return Err(format!("the window no longer belongs to process {pid}"));
        }
        let before = before.fingerprint?;
        let after = after.fingerprint?;
        if before.start_time.is_none() || after.start_time.is_none() {
            return Err("no process start time".into());
        }
        if before.pid != i64::from(pid) || after.pid != i64::from(pid) || !before.matches(&after) {
            return Err("the process changed during the read".into());
        }
        Ok(after)
    };
    match capture() {
        Ok(fp) => InstanceCapture::Captured(fp),
        Err(reason) => InstanceCapture::Failed(reason),
    }
}

pub(crate) fn evidence_for(
    args: &serde_json::Value,
    fetch: impl FnOnce() -> Result<ProcessFingerprint, String>,
) -> InstanceEvidence {
    if args.get("element_token").is_none() {
        InstanceEvidence::NotChecked
    } else {
        match fetch() {
            Ok(fp) => InstanceEvidence::Live(fp),
            Err(reason) => InstanceEvidence::Unavailable(reason),
        }
    }
}

#[cfg(windows)]
pub(crate) fn process_fingerprint(pid: u32) -> Result<ProcessFingerprint, String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, FILETIME, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
        PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
        .map_err(|e| format!("OpenProcess({pid}): {e}"))?;
        let result = (|| {
            match WaitForSingleObject(handle, 0) {
                WAIT_TIMEOUT => (),
                WAIT_OBJECT_0 => return Err(format!("process {pid} has exited")),
                WAIT_FAILED => {
                    return Err(format!("WaitForSingleObject({pid}): {:?}", GetLastError()))
                }
                other => return Err(format!("WaitForSingleObject({pid}): unexpected {other:?}")),
            }
            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)
                .map_err(|e| format!("GetProcessTimes({pid}): {e}"))?;
            let mut path = vec![0u16; 32768];
            let mut len = path.len() as u32;
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_FORMAT(0),
                PWSTR(path.as_mut_ptr()),
                &mut len,
            )
            .map_err(|e| format!("QueryFullProcessImageNameW({pid}): {e}"))?;
            let executable =
                String::from_utf16(&path[..len as usize]).map_err(|e| e.to_string())?;
            Ok(ProcessFingerprint {
                pid: i64::from(pid),
                start_time: Some(
                    (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
                ),
                executable: Some(executable),
            })
        })();
        let _ = CloseHandle(handle);
        result
    }
}

#[cfg(windows)]
fn window_owner(hwnd: u64) -> Result<u32, String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow};
    unsafe {
        let window = HWND(hwnd as usize as *mut _);
        if !IsWindow(window).as_bool() {
            return Err("the window no longer exists".into());
        }
        let mut pid = 0;
        if GetWindowThreadProcessId(window, Some(&mut pid)) == 0 || pid == 0 {
            return Err(format!(
                "cannot read window owner: {:?}",
                windows::Win32::Foundation::GetLastError()
            ));
        }
        Ok(pid)
    }
}

#[cfg(windows)]
pub(crate) fn probe(pid: u32, hwnd: u64) -> Probe {
    Probe {
        fingerprint: process_fingerprint(pid),
        window_owner: window_owner(hwnd),
    }
}

/// Capture recorded when the accessibility walk outlives its backstop. The
/// detached walk worker never publishes; its result is dropped.
pub(crate) fn walk_timed_out_capture() -> InstanceCapture {
    InstanceCapture::Failed(
        "the accessibility walk timed out before process identity could be confirmed".into(),
    )
}

/// Every Windows element-snapshot publication goes through these adapters,
/// so each published snapshot carries the process instance it was read from.
pub(crate) fn publish_window_snapshot<S: SnapshotPayload>(
    store: &SnapshotStore<S>,
    pid: i32,
    hwnd: u64,
    payload: S,
    session: Option<&str>,
    screenshot_scale: Option<f64>,
    capture: InstanceCapture,
) -> Option<(u32, Vec<u32>)> {
    store.publish_for_session_with_instance(pid, hwnd, payload, session, screenshot_scale, capture)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_window_snapshot_with_basis<S: SnapshotPayload>(
    store: &SnapshotStore<S>,
    pid: i32,
    hwnd: u64,
    payload: S,
    session: Option<&str>,
    screenshot_scale: Option<f64>,
    basis: Option<WindowBasis>,
    ticket: &CaptureTicket<S>,
    capture: InstanceCapture,
) -> Option<(u32, Vec<u32>, Option<WindowBasis>)> {
    store.publish_basis_for_session_with_instance(
        pid,
        hwnd,
        payload,
        session,
        screenshot_scale,
        basis,
        ticket,
        capture,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_capture_snapshot_with_basis<S: SnapshotPayload>(
    store: &SnapshotStore<S>,
    pid: i32,
    hwnd: u64,
    payload: S,
    session: Option<&str>,
    screenshot_scale: Option<f64>,
    basis: Option<WindowBasis>,
    ticket: &CaptureTicket<S>,
    capture: InstanceCapture,
) -> Option<(u32, Vec<u32>, Option<WindowBasis>)> {
    store.publish_capture_basis_for_session_with_instance(
        pid,
        hwnd,
        payload,
        session,
        screenshot_scale,
        basis,
        ticket,
        capture,
    )
}

pub(crate) fn resolve_with_evidence<S: SnapshotPayload>(
    store: &SnapshotStore<S>,
    pid: i32,
    args: &serde_json::Value,
    evidence: InstanceEvidence,
) -> Result<ResolvedElement<S::Element>, ToolResult> {
    store.resolve_for_instance(pid, args, evidence)
}

/// Evidence for an async caller: the fingerprint read runs as a blocking
/// task; a task that fails to report is unavailable evidence, never a match.
pub(crate) async fn evidence_from_task<F, Fut>(
    args: &serde_json::Value,
    task: F,
) -> InstanceEvidence
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<
        Output = Result<Result<ProcessFingerprint, String>, tokio::task::JoinError>,
    >,
{
    if args.get("element_token").is_none() {
        return InstanceEvidence::NotChecked;
    }
    match task().await {
        Ok(Ok(fp)) => InstanceEvidence::Live(fp),
        Ok(Err(reason)) => InstanceEvidence::Unavailable(reason),
        Err(error) => {
            InstanceEvidence::Unavailable(format!("process fingerprint task failed: {error}"))
        }
    }
}

/// Resolve an element token against the live process instance. The store
/// lock is taken only by the synchronous resolve after the read completes.
#[cfg(windows)]
pub(crate) async fn resolve_live<S: SnapshotPayload>(
    store: &SnapshotStore<S>,
    pid: i32,
    args: &serde_json::Value,
) -> Result<ResolvedElement<S::Element>, ToolResult> {
    let evidence = evidence_from_task(args, || {
        tokio::task::spawn_blocking(move || process_fingerprint(pid as u32))
    })
    .await;
    resolve_with_evidence(store, pid, args, evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::browser::types::ProcessFingerprint;
    struct Payload;
    impl SnapshotPayload for Payload {
        type Element = usize;
        fn len(&self) -> usize {
            1
        }
        fn retain(&self, index: usize) -> Option<usize> {
            (index == 0).then_some(index)
        }
    }
    fn fp(start: u64) -> ProcessFingerprint {
        ProcessFingerprint {
            pid: 42,
            start_time: Some(start),
            executable: Some("app".into()),
        }
    }
    #[cfg(windows)]
    #[test]
    fn live_process_fingerprints_and_exited_child() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let pid = std::process::id();
        let first = process_fingerprint(pid).unwrap();
        let second = process_fingerprint(pid).unwrap();
        assert!(first.start_time.is_some());
        assert!(first.matches(&second));
        let mut child = Command::new("cmd")
            .args(["/c", "set /p line="])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let child_pid = child.id();
        assert!(process_fingerprint(child_pid).unwrap().start_time.is_some());
        writeln!(child.stdin.as_mut().unwrap(), "release").unwrap();
        child.wait().unwrap();
        assert!(process_fingerprint(child_pid).is_err());
    }

    #[test]
    fn capture_requires_stable_instance_and_owner() {
        fn probe(
            fingerprint: Result<ProcessFingerprint, String>,
            window_owner: Result<u32, String>,
        ) -> Probe {
            Probe {
                fingerprint,
                window_owner,
            }
        }
        let good = || probe(Ok(fp(1)), Ok(42));
        let mut no_exe = fp(1);
        no_exe.executable = None;
        let mut no_start = fp(1);
        no_start.start_time = None;
        let mut other_exe = fp(1);
        other_exe.executable = Some("other".into());
        let cases = vec![
            (good(), good(), true),
            (good(), probe(Ok(fp(2)), Ok(42)), false),
            (good(), probe(Ok(other_exe), Ok(42)), false),
            (good(), probe(Ok(no_exe), Ok(42)), true),
            (probe(Err("before".into()), Ok(42)), good(), false),
            (good(), probe(Err("after".into()), Ok(42)), false),
            (good(), probe(Ok(no_start), Ok(42)), false),
            (good(), probe(Ok(fp(1)), Ok(43)), false),
            (good(), probe(Ok(fp(1)), Err("owner error".into())), false),
            (probe(Ok(fp(1)), Err("window gone".into())), good(), false),
        ];
        for (before, after, expected) in cases {
            assert_eq!(
                matches!(
                    instance_capture(42, before, after),
                    InstanceCapture::Captured(_)
                ),
                expected
            );
        }
    }

    #[test]
    fn evidence_skips_fetch_without_token_and_preserves_result() {
        let no_token = serde_json::json!({});
        assert!(matches!(
            evidence_for(&no_token, || panic!("must not fetch")),
            InstanceEvidence::NotChecked
        ));
        let token = serde_json::json!({"element_token":"1:0"});
        assert!(matches!(
            evidence_for(&token, || Ok(fp(1))),
            InstanceEvidence::Live(_)
        ));
        assert!(matches!(
            evidence_for(&token, || Err("unavailable".into())),
            InstanceEvidence::Unavailable(_)
        ));
    }

    #[test]
    fn restarted_process_is_refused_through_adapter() {
        let store = SnapshotStore::new();
        let id = publish_window_snapshot(
            &store,
            42,
            7,
            Payload,
            Some("owner"),
            None,
            InstanceCapture::Captured(fp(1)),
        )
        .unwrap()
        .0;
        let args = serde_json::json!({"element_token": cua_driver_core::element_token::token_for(id, 0), "_session_id": "owner"});
        let result = resolve_with_evidence(&store, 42, &args, InstanceEvidence::Live(fp(2)));
        assert!(result.is_err(), "restarted process must be refused");
        assert_eq!(
            result.unwrap_err().structured_content.unwrap()["refusal"]["code"],
            "process_instance_mismatch"
        );
    }

    fn refusal_code(
        result: Result<ResolvedElement<usize>, ToolResult>,
    ) -> (String, Option<cua_driver_contract::ErrorDisposition>) {
        let err = match result {
            Ok(_) => panic!("expected a refusal"),
            Err(err) => err,
        };
        let code = err.structured_content.as_ref().unwrap()["refusal"]["code"]
            .as_str()
            .unwrap()
            .to_owned();
        (code, err.disposition)
    }

    fn token_args(id: u32) -> serde_json::Value {
        serde_json::json!({"element_token": cua_driver_core::element_token::token_for(id, 0), "_session_id": "owner"})
    }

    #[test]
    fn walk_timeout_capture_refuses_existing_element_as_unverified() {
        let store = SnapshotStore::new();
        let id = publish_window_snapshot(
            &store,
            42,
            7,
            Payload,
            Some("owner"),
            None,
            walk_timed_out_capture(),
        )
        .unwrap()
        .0;
        let (code, disposition) = refusal_code(resolve_with_evidence(
            &store,
            42,
            &token_args(id),
            InstanceEvidence::Live(fp(1)),
        ));
        assert_eq!(code, "process_instance_unverified");
        assert_eq!(
            disposition,
            Some(cua_driver_contract::ErrorDisposition::NOT_DELIVERED)
        );
    }

    #[test]
    fn ticketed_publications_carry_their_capture() {
        let store = SnapshotStore::new();
        let ticket = store.begin_capture(42, 7);
        let id = publish_window_snapshot_with_basis(
            &store,
            42,
            7,
            Payload,
            Some("owner"),
            Some(1.0),
            None,
            &ticket,
            InstanceCapture::Captured(fp(1)),
        )
        .unwrap()
        .0;
        assert!(
            resolve_with_evidence(&store, 42, &token_args(id), InstanceEvidence::Live(fp(1)))
                .is_ok()
        );
        let (code, _) = refusal_code(resolve_with_evidence(
            &store,
            42,
            &token_args(id),
            InstanceEvidence::Live(fp(2)),
        ));
        assert_eq!(code, "process_instance_mismatch");

        // A screenshot refresh of that snapshot keeps its original instance.
        let refresh = store.begin_capture(42, 7);
        assert!(store
            .refresh_basis_for_session(42, 7, Some("owner"), 2.0, None, None, &refresh)
            .is_some());
        assert!(
            resolve_with_evidence(&store, 42, &token_args(id), InstanceEvidence::Live(fp(1)))
                .is_ok()
        );
        let (code, _) = refusal_code(resolve_with_evidence(
            &store,
            42,
            &token_args(id),
            InstanceEvidence::Live(fp(2)),
        ));
        assert_eq!(code, "process_instance_mismatch");

        let other = SnapshotStore::new();
        let ticket = other.begin_capture(42, 7);
        let published = publish_capture_snapshot_with_basis(
            &other,
            42,
            7,
            Payload,
            Some("owner"),
            Some(1.0),
            None,
            &ticket,
            InstanceCapture::Failed("identity read failed".into()),
        );
        let id = published.expect("capture-only snapshot published").0;
        assert!(
            resolve_with_evidence(&other, 42, &token_args(id), InstanceEvidence::Live(fp(1)))
                .is_err()
        );
    }

    #[tokio::test]
    async fn fingerprint_task_failure_is_unverified_and_tokenless_reads_skip_it() {
        let store = SnapshotStore::new();
        let id = publish_window_snapshot(
            &store,
            42,
            7,
            Payload,
            Some("owner"),
            None,
            InstanceCapture::Captured(fp(1)),
        )
        .unwrap()
        .0;
        let args = token_args(id);
        let evidence = evidence_from_task(&args, || async {
            tokio::spawn(async { panic!("fingerprint worker") })
                .await
                .map(|()| Ok(fp(1)))
        })
        .await;
        assert!(matches!(evidence, InstanceEvidence::Unavailable(_)));
        let (code, disposition) = refusal_code(resolve_with_evidence(&store, 42, &args, evidence));
        assert_eq!(code, "process_instance_unverified");
        assert_eq!(
            disposition,
            Some(cua_driver_contract::ErrorDisposition::NOT_DELIVERED)
        );

        let evidence = evidence_from_task(&serde_json::json!({}), || async {
            panic!("tokenless reads must not read process identity");
            #[allow(unreachable_code)]
            Ok(Ok(fp(1)))
        })
        .await;
        assert!(matches!(evidence, InstanceEvidence::NotChecked));
    }
}
