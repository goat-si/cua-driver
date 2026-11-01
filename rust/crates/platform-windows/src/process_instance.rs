use cua_driver_core::element_token::ResolvedElement;
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::snapshot_store::{
    InstanceCapture, InstanceEvidence, SnapshotPayload, SnapshotStore,
};

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
