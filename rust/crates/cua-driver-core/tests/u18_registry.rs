use cua_driver_core::browser::types::ProcessFingerprint;
use cua_driver_core::element_token::{format_snapshot_id, token_for};
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::snapshot_store::{
    InstanceCapture, InstanceEvidence, ScopedWalk, SnapshotPayload, SnapshotStore,
};
use cua_driver_core::tool::{ProtectedResourceOwnership, Tool, ToolDef, ToolRegistry};
use cua_driver_core::window_state_view::{self, ViewContext, ViewKey, ViewOptions};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

struct Payload(Vec<usize>);
impl SnapshotPayload for Payload {
    type Element = usize;
    fn len(&self) -> usize {
        self.0.len()
    }
    fn retain(&self, index: usize) -> Option<usize> {
        self.0.get(index).copied()
    }
    fn structural_path(&self, index: usize) -> Option<Vec<usize>> {
        self.0.get(index).map(|_| vec![index])
    }
}
fn fingerprint() -> ProcessFingerprint {
    ProcessFingerprint {
        pid: 42,
        start_time: Some(1),
        executable: Some("/fake".into()),
    }
}

#[derive(Clone)]
struct FakeRoot {
    element: usize,
    path: Vec<usize>,
    window: u64,
    start_time: Option<u64>,
}

fn revalidate_live(
    live: &Mutex<Option<FakeRoot>>,
    old: &usize,
    path: &[usize],
    window: u64,
    process: &ProcessFingerprint,
) -> Result<usize, cua_driver_core::snapshot_store::RootValidationError> {
    live.lock()
        .unwrap()
        .as_ref()
        .filter(|entry| {
            entry.element == *old
                && entry.path == path
                && entry.window == window
                && entry.start_time == process.start_time
        })
        .map(|entry| entry.element)
        .ok_or_else(|| {
            cua_driver_core::snapshot_store::RootValidationError::new(
                "stale_element_token",
                "fake element is no longer live",
            )
        })
}

struct FakePlatform {
    def: ToolDef,
    store: Arc<SnapshotStore<Payload>>,
    full_walks: Arc<AtomicUsize>,
    root_walks: Arc<AtomicUsize>,
    session: Arc<Mutex<Option<String>>>,
    live_root: Arc<Mutex<Option<FakeRoot>>>,
}

#[async_trait::async_trait]
impl Tool for FakePlatform {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    async fn protected_resource_ownership(
        &self,
        _adapter: &str,
        _args: &Value,
    ) -> ProtectedResourceOwnership {
        ProtectedResourceOwnership::DriverOwned
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if let Err(result) = window_state_view::validate_root_args(&args) {
            return result;
        }
        let session = args.get("_session_id").and_then(Value::as_str);
        *self.session.lock().unwrap() = session.map(str::to_owned);
        let root_token = args.get("root_element_token").and_then(Value::as_str);
        let (id, scope, expected_parent) = if let Some(root_token) = root_token {
            let selected = match self.store.begin_scoped_capture(
                42,
                7,
                root_token,
                session,
                InstanceEvidence::Live(fingerprint()),
            ) {
                Ok(value) => value,
                Err(error) => return error,
            };
            let selected = match selected.validate_root(|old, path, window, process| {
                revalidate_live(&self.live_root, old, path, window, process)
            }) {
                Ok(value) => value,
                Err(error) => return error,
            };
            let parent = selected.parent_snapshot_id();
            self.root_walks.fetch_add(1, Ordering::SeqCst);
            let walk = if args.get("max_depth").and_then(Value::as_u64) == Some(1) {
                ScopedWalk::Incomplete(vec![cua_driver_contract::IncompleteReason {
                    kind: cua_driver_contract::IncompleteReasonKind::Budget,
                    detail: cua_driver_contract::IncompleteReasonDetail::DepthLimit,
                }])
            } else {
                ScopedWalk::Complete(Payload(vec![11, 12]))
            };
            let id = match self.store.commit_scoped_capture(selected, walk) {
                Ok(id) => id,
                Err(error) => return error,
            };
            let parent = format_snapshot_id(parent);
            (
                id,
                Some(json!({"root_token":root_token,"parent_snapshot_id":&parent})),
                Some(parent),
            )
        } else {
            self.full_walks.fetch_add(1, Ordering::SeqCst);
            let id = self
                .store
                .publish_for_session_with_instance(
                    42,
                    7,
                    Payload(vec![10, 11]),
                    session,
                    None,
                    InstanceCapture::Captured(fingerprint()),
                )
                .unwrap()
                .0;
            (id, None, None)
        };
        let query = args.get("query").and_then(Value::as_str);
        let md = if query.is_some() {
            "- [1] Button Save"
        } else {
            "- [0] Group\n  - [1] Button Save"
        };
        let elements = vec![
            json!({"element_index":0,"element_token":token_for(id,0),"role":"group","depth":0}),
            json!({"element_index":1,"element_token":token_for(id,1),"role":"button","depth":1}),
        ];
        let elements =
            cua_driver_core::element_query::project_elements_for_query(elements, query, md);
        let mut structured = json!({"pid":42,"window_id":7,"snapshot_id":format_snapshot_id(id),
            "tree_markdown":md,"elements":elements,"observation_complete":true,
            "observation_incomplete_reasons":[]});
        let mut budget = cua_driver_core::walk_budget::WalkBudget::nodes_only(10);
        budget.assert_exhaustive();
        let owner = if root_token.is_some() { 11 } else { 10 };
        budget.record_verified_scope_exclusion(
            cua_driver_contract::ScopeExclusionKind::ClosedMenusNotExpanded,
            Some(owner),
        );
        budget.outcome().apply_with_owner_mapping(
            &mut structured,
            &HashMap::from([(owner, if root_token.is_some() { 1 } else { 0 })]),
        );
        if let Some(scope) = scope {
            structured["scope"] = scope;
            structured["drilldown_status"] = json!("complete");
        }
        let opts = match ViewOptions::from_args(&args) {
            Ok(opts) => opts,
            Err(error) => return error,
        };
        let ctx = ViewContext {
            pid: 42,
            window_id: 7,
            key: ViewKey {
                query: query.map(str::to_owned),
                max_elements: 250,
                max_depth: None,
            },
            focus_probe: None,
        };
        let mut result = ToolResult::text(md).with_structured(structured);
        if let Some(root) = root_token {
            window_state_view::apply_scoped(
                root,
                expected_parent.as_deref().unwrap(),
                &opts,
                &ctx,
                &mut result.content,
                result.structured_content.as_mut().unwrap(),
            );
        } else {
            window_state_view::apply(
                &opts,
                &ctx,
                &mut result.content,
                result.structured_content.as_mut().unwrap(),
            );
        }
        result
    }
}

struct FakeAction {
    def: ToolDef,
    store: Arc<SnapshotStore<Payload>>,
    live: Arc<Mutex<Option<FakeRoot>>>,
    sends: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for FakeAction {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    async fn protected_resource_ownership(
        &self,
        _adapter: &str,
        _args: &Value,
    ) -> ProtectedResourceOwnership {
        ProtectedResourceOwnership::DriverOwned
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(token) = args.get("element_token").and_then(Value::as_str) else {
            return ToolResult::error("missing element token");
        };
        let session = args.get("_session_id").and_then(Value::as_str);
        let selected = match self.store.begin_scoped_capture(
            42,
            7,
            token,
            session,
            InstanceEvidence::Live(fingerprint()),
        ) {
            Ok(value) => value,
            Err(error) => return error,
        };
        if let Err(error) = selected.validate_root(|old, path, window, process| {
            revalidate_live(&self.live, old, path, window, process)
        }) {
            return error;
        }
        self.sends.fetch_add(1, Ordering::SeqCst);
        use cua_driver_core::action_record::{
            ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
        };
        let record = ActionExecutionRecord::builder(
            ActionEffect::Unverifiable,
            ActionTransport::WindowsUiaInvoke,
            RequestedDelivery::Background,
        )
        .actual_delivery(ActualDelivery::Background)
        .build()
        .unwrap();
        ToolResult::text("fake action sent").with_action_record(record)
    }
}

#[tokio::test]
async fn registry_fake_platform_drills_without_replacing_the_full_snapshot() {
    let store = Arc::new(SnapshotStore::new());
    let full_walks = Arc::new(AtomicUsize::new(0));
    let root_walks = Arc::new(AtomicUsize::new(0));
    let session = Arc::new(Mutex::new(None));
    let live_root = Arc::new(Mutex::new(Some(FakeRoot {
        element: 11,
        path: vec![1],
        window: 7,
        start_time: Some(1),
    })));
    let live_action = Arc::new(Mutex::new(Some(FakeRoot {
        element: 12,
        path: vec![1, 1],
        window: 7,
        start_time: Some(1),
    })));
    let sends = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FakePlatform { store: store.clone(), full_walks: full_walks.clone(), root_walks: root_walks.clone(), session: session.clone(), live_root:live_root.clone(),
        def: ToolDef { name:"get_window_state".into(), description:"fake".into(), input_schema:window_state_view::extend_input_schema(json!({"type":"object","additionalProperties":false,
            "properties":{"pid":{"type":"integer"},"window_id":{"type":"integer"},"query":{"type":"string"},"max_depth":{"type":"integer"},"include_accessibility_tree":{"type":"boolean"}}})),
            read_only:true, destructive:false, idempotent:false, open_world:false } }));
    registry.register(Box::new(FakeAction { store:store.clone(), live:live_action.clone(), sends:sends.clone(),
        def:ToolDef { name:"click".into(), description:"fake action".into(),
            input_schema:json!({"type":"object","additionalProperties":false,"properties":{
                "pid":{"type":"integer"},"window_id":{"type":"integer"},"element_token":{"type":"string"}}}),
            read_only:false, destructive:false, idempotent:false, open_world:false } }));
    let full = registry
        .invoke("get_window_state", json!({"pid":42,"window_id":7}))
        .await;
    assert_ne!(full.is_error, Some(true), "{full:?}");
    assert_eq!(
        full.structured_content.as_ref().unwrap()["observation_scope_exclusions"],
        json!([{"kind":"closed_menus_not_expanded","element_indices":[0]}])
    );
    let parent = full.structured_content.unwrap()["snapshot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let root = format!("{parent}:1");
    let mut action_token = None;
    for format in ["markdown", "elements", "both"] {
        let result = registry.invoke("get_window_state", json!({"pid":42,"window_id":7,"root_element_token":root,"tree_format":format,"query":"Save"})).await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let output = result.structured_content.unwrap();
        assert_eq!(output["drilldown_status"], "complete");
        assert_eq!(output["scope"]["parent_snapshot_id"], parent);
        assert_eq!(
            output["observation_scope_exclusions"],
            json!([{"kind":"closed_menus_not_expanded","element_indices":[1]}])
        );
        if format == "markdown" {
            assert!(output.get("elements").is_none());
        }
        if format == "elements" {
            assert!(output.get("tree_markdown").is_none());
        }
        if format != "elements" {
            assert!(output["tree_markdown"].as_str().unwrap().contains("[1]"));
        }
        if format != "markdown" {
            assert_eq!(output["elements"].as_array().unwrap().len(), 1);
            let token = output["elements"][0]["element_token"].as_str().unwrap();
            let action = registry
                .invoke(
                    "click",
                    json!({"pid":42,"window_id":7,"element_token":token}),
                )
                .await;
            assert_ne!(action.is_error, Some(true), "{action:?}");
            assert_eq!(
                action.structured_content.as_ref().unwrap()["effect"],
                "unverifiable"
            );
            action_token = Some(token.to_owned());
            let args = json!({"element_token":token,"_session_id":session.lock().unwrap().clone()});
            assert!(store
                .resolve_for_instance(42, &args, InstanceEvidence::Live(fingerprint()))
                .is_ok());
            let owner = session.lock().unwrap().clone();
            let root = store
                .begin_scoped_capture(
                    42,
                    7,
                    token,
                    owner.as_deref(),
                    InstanceEvidence::Live(fingerprint()),
                )
                .unwrap();
            let live = root
                .validate_root(|element, path, window, _| {
                    assert_eq!((*element, path, window), (12, &[1, 1][..], 7));
                    Ok(12)
                })
                .unwrap();
            assert_eq!(*live.root_element(), 12);
        }
        assert_eq!(
            store.with_latest(42, 7, |id, payload| (
                format_snapshot_id(id),
                payload.0.clone()
            )),
            Some((parent.clone(), vec![10, 11]))
        );
    }
    *live_action.lock().unwrap() = None;
    let refused_action = registry
        .invoke(
            "click",
            json!({"pid":42,"window_id":7,"element_token":action_token.unwrap()}),
        )
        .await;
    assert_eq!(refused_action.is_error, Some(true));
    assert_eq!(
        refused_action.structured_content.unwrap()["refusal"]["code"],
        "stale_element_token"
    );
    assert_eq!(sends.load(Ordering::SeqCst), 2);
    let full_output = registry
        .invoke(
            "get_window_state",
            json!({"pid":42,"window_id":7,
        "root_element_token":root,"full_output":true,"query":"Save"}),
        )
        .await;
    assert_ne!(full_output.is_error, Some(true), "{full_output:?}");
    let output = full_output.structured_content.unwrap();
    assert_eq!(output["drilldown_status"], "complete");
    assert_eq!(
        output["observation_scope_exclusions"],
        json!([{"kind":"closed_menus_not_expanded","element_indices":[1]}])
    );
    assert_eq!(output["elements"].as_array().unwrap().len(), 1);
    assert!(output.get("tree_markdown").is_some());
    assert_eq!(
        store
            .with_latest(42, 7, |id, _| format_snapshot_id(id))
            .as_deref(),
        Some(parent.as_str())
    );
    let incomplete = registry
        .invoke(
            "get_window_state",
            json!({"pid":42,"window_id":7,"root_element_token":root,"max_depth":1}),
        )
        .await;
    let output = incomplete.structured_content.unwrap();
    assert_eq!(output["drilldown_status"], "incomplete");
    assert!(output.get("snapshot_id").is_none());
    assert!(output.get("elements").is_none());
    for (extra, code) in [
        (json!({"since":"latest"}), "root_since_unsupported"),
        (
            json!({"display_only":true}),
            "root_display_only_unsupported",
        ),
        (
            json!({"include_accessibility_tree":false}),
            "root_screenshot_only_unsupported",
        ),
    ] {
        let mut args = json!({"pid":42,"window_id":7,"root_element_token":root});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let refused = registry.invoke("get_window_state", args).await;
        assert_eq!(refused.is_error, Some(true));
        assert_eq!(refused.structured_content.unwrap()["refusal"]["code"], code);
    }
    let malformed = registry
        .invoke(
            "get_window_state",
            json!({"pid":42,"window_id":7,"root_element_token":42}),
        )
        .await;
    assert_eq!(malformed.is_error, Some(true));
    assert_eq!(
        malformed.structured_content.unwrap()["refusal"]["code"],
        "invalid_element_token"
    );
    for live in [
        Some(FakeRoot {
            element: 11,
            path: vec![2],
            window: 7,
            start_time: Some(1),
        }),
        None,
        Some(FakeRoot {
            element: 11,
            path: vec![1],
            window: 8,
            start_time: Some(1),
        }),
        Some(FakeRoot {
            element: 11,
            path: vec![1],
            window: 7,
            start_time: Some(2),
        }),
        Some(FakeRoot {
            element: 99,
            path: vec![1],
            window: 7,
            start_time: Some(1),
        }),
    ] {
        *live_root.lock().unwrap() = live;
        let refused = registry
            .invoke(
                "get_window_state",
                json!({"pid":42,"window_id":7,"root_element_token":root}),
            )
            .await;
        assert_eq!(refused.is_error, Some(true), "{refused:?}");
        assert_eq!(
            refused.structured_content.unwrap()["refusal"]["code"],
            "stale_element_token"
        );
    }
    assert_eq!(full_walks.load(Ordering::SeqCst), 1);
    assert_eq!(root_walks.load(Ordering::SeqCst), 5);
}
