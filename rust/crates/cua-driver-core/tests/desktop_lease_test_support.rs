use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use cua_driver_core::{
    authorization::PermissionMode,
    desktop_lease::{
        release_lease, retain_lease,
        test_support::{is_retained, TestBackend, TestClock},
    },
    protocol::ToolResult,
    session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling},
    tool::{Tool, ToolDef, ToolRegistry},
};
use serde_json::{json, Value};

struct Probe {
    def: ToolDef,
    hits: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for Probe {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        self.hits.fetch_add(1, Ordering::SeqCst);
        retain_lease(args["_session_id"].as_str().unwrap()).unwrap();
        ToolResult::text("done")
    }
}

#[tokio::test]
async fn platform_can_test_busy_and_retained_ownership() {
    let backend = Arc::new(TestBackend::default());
    let mut registry = ToolRegistry::with_test_lease(backend, Arc::new(TestClock::default()));
    let hits = Arc::new(AtomicUsize::new(0));
    registry.register(Box::new(Probe {
        hits: hits.clone(),
        def: ToolDef {
            name: "invoke_menu".into(),
            description: "probe".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: false,
        },
    }));
    let ceiling = SessionModeCeiling::for_trusted_sessions(
        [PermissionMode::Unrestricted],
        true,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
    .unwrap();
    let context = SessionAuthorizationRegistry::with_ceiling(ceiling)
        .compatibility_context(PermissionMode::Unrestricted, None)
        .unwrap();
    let owner = context.runtime_session_key("u19-owner");
    registry
        .invoke_with_context(
            "invoke_menu",
            json!({"pid":42,"session":"u19-owner"}),
            context.clone(),
        )
        .await;
    assert!(is_retained(&owner));
    let result = registry
        .invoke_with_context(
            "invoke_menu",
            json!({"pid":42,"session":"u19-other"}),
            context,
        )
        .await;
    assert_eq!(result.structured_content.unwrap()["code"], "desktop_busy");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    release_lease(&owner);
    assert!(!is_retained(&owner));
}
