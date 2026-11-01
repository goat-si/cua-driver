use cua_driver_core::element_token::ResolvedElement;
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::snapshot_store::{
    InstanceCapture, InstanceEvidence, SnapshotPayload, SnapshotStore,
};

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

fn publish_window_snapshot<S: SnapshotPayload>(
    store: &SnapshotStore<S>,
    pid: i32,
    hwnd: u64,
    payload: S,
    session: Option<&str>,
    capture: InstanceCapture,
) -> Option<(u32, Vec<u32>)> {
    store.publish_for_session_with_instance(pid, hwnd, payload, session, None, capture)
}
fn resolve_with_evidence<S: SnapshotPayload>(
    store: &SnapshotStore<S>,
    pid: i32,
    args: &serde_json::Value,
    evidence: InstanceEvidence,
) -> Result<ResolvedElement<S::Element>, ToolResult> {
    store.resolve_for_instance(pid, args, evidence)
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
}
