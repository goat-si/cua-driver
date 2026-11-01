use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cua_driver_core::authorization::PermissionMode;
use cua_driver_core::desktop_lease::test_support::{TestBackend, TestClock};
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling};
use cua_driver_core::tool::{Tool, ToolDef, ToolRegistry};
use serde_json::{json, Value};

struct NativeProbe {
    def: ToolDef,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for NativeProbe {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, _: Value) -> ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("refused admission must not invoke the native leaf")
    }
}

async fn refuse(unavailable: bool) -> (Value, usize, usize) {
    let backend = Arc::new(TestBackend::default());
    if unavailable {
        backend.set_unavailable(true);
    } else {
        backend.hold();
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry =
        ToolRegistry::with_test_lease(backend.clone(), Arc::new(TestClock::default()));
    registry.register(Box::new(NativeProbe {
        calls: calls.clone(),
        def: ToolDef {
            name: "click".into(),
            description: "fake native leaf".into(),
            input_schema: json!({"type": "object"}),
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
    let result = registry
        .invoke_with_context(
            "click",
            json!({"pid":42,"window_id":7,"x":10,"y":20}),
            context,
        )
        .await;
    (
        serde_json::to_value(result).unwrap(),
        calls.load(Ordering::SeqCst),
        backend.attempts(),
    )
}

fn expected(code: &str, message: &str) -> Value {
    json!({
        "content": [{"type":"text", "text":message}],
        "isError": true,
        "structuredContent": {
            "effect":"refused", "code":code, "status":"refused",
            "disposition": {"delivery":"not_delivered", "retry":"safe"},
            "refusal": {"code":code, "message":message}
        }
    })
}

#[tokio::test]
async fn busy_result_is_unchanged_and_native_calls_are_zero() {
    let (result, calls, attempts) = refuse(false).await;
    assert_eq!(result, expected("desktop_busy", "desktop input is busy"));
    assert!(attempts > 0);
    assert_eq!(calls, 0);
}

#[tokio::test]
async fn unavailable_result_is_unchanged_and_native_calls_are_zero() {
    let (result, calls, attempts) = refuse(true).await;
    assert_eq!(
        result,
        expected("desktop_lease_unavailable", "synthetic failure")
    );
    assert!(attempts > 0);
    assert_eq!(calls, 0);
}
