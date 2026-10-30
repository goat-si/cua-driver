use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::*;
use crate::authorization::PermissionMode;
use crate::protocol::ToolResult;
use crate::session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling};
use crate::tool::{Tool, ToolDef, ToolRegistry};

const BROWSER_PID: i64 = 4343;
const OTHER_PID: i64 = 4242;

const MANIFEST: &str = r#"
version: 3
resources:
  browser:
    origins: ["https://app.example"]
    existing_profiles:
      - {pid: 4343, window_id: 9}
  desktop:
    windows:
      - {pid: 4242, window_id: 7}
      - {pid: 4343, window_id: 9}
allow:
  tools: [click, get_window_state, get_accessibility_tree, set_value, type_text, run_steps]
"#;

fn manifest(source: &str) -> Result<crate::session_manifest::SessionManifest, String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manifest.yaml");
    std::fs::write(&path, source).unwrap();
    crate::session_manifest::load_manifest(&path)
}

fn context() -> Arc<crate::session_authorization::EffectiveAuthorizationContext> {
    SessionAuthorizationRegistry::with_ceiling(
        SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Standard],
            false,
            Duration::from_secs(60),
            Duration::from_secs(30),
        )
        .unwrap(),
    )
    .compatibility_context(
        PermissionMode::Standard,
        Some(Arc::new(manifest(MANIFEST).unwrap())),
    )
    .unwrap()
}

struct ScreenSize;

#[async_trait]
impl Tool for ScreenSize {
    fn def(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(|| ToolDef {
            name: "get_screen_size".into(),
            description: "probe".into(),
            input_schema: json!({"type": "object"}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        ToolResult::text("1440x900")
            .with_structured(json!({"width": 1440, "height": 900, "scale_factor": 2.0}))
    }
}

struct Probe {
    def: ToolDef,
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for Probe {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        self.calls.lock().unwrap().push(self.def.name.clone());
        ToolResult::text("ok")
    }
}

const DESKTOP_TOOLS: [&str; 5] = [
    "click",
    "get_window_state",
    "get_accessibility_tree",
    "set_value",
    "type_text",
];

fn registry() -> (Arc<ToolRegistry>, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::new();
    for name in DESKTOP_TOOLS {
        registry.register(Box::new(Probe {
            def: ToolDef {
                name: name.into(),
                description: "probe".into(),
                input_schema: json!({"type": "object", "additionalProperties": true}),
                read_only: matches!(name, "get_window_state" | "get_accessibility_tree"),
                destructive: false,
                idempotent: false,
                open_world: false,
            },
            calls: calls.clone(),
        }));
    }
    registry.register(Box::new(ScreenSize));
    registry.register_session_tools();
    let registry = Arc::new(registry);
    registry.init_self_weak();
    (registry, calls)
}

fn args(tool: &str, pid: i64, window_id: u64) -> Value {
    let mut args = json!({"pid": pid, "window_id": window_id, "session": "protect"});
    match tool {
        "set_value" => args["value"] = json!("v"),
        "type_text" => args["text"] = json!("t"),
        "click" => {
            args["x"] = json!(10);
            args["y"] = json!(10);
        }
        _ => {}
    }
    args
}

fn code(result: &ToolResult) -> Option<String> {
    result
        .structured_content
        .as_ref()
        .and_then(|value| value.pointer("/refusal/code"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[tokio::test]
async fn desktop_tools_never_reach_a_protected_browser_process() {
    let (registry, calls) = registry();
    for tool in DESKTOP_TOOLS {
        let refused = registry
            .invoke_with_context(tool, args(tool, BROWSER_PID, 9), context())
            .await;
        assert_eq!(
            code(&refused).as_deref(),
            Some("browser_process_protected"),
            "{tool}: {:?}",
            refused.structured_content
        );
        // Every window of that process, not only the listed one.
        let other_window = registry
            .invoke_with_context(tool, args(tool, BROWSER_PID, 10), context())
            .await;
        assert_eq!(
            code(&other_window).as_deref(),
            Some("browser_process_protected"),
            "{tool}"
        );
    }
    assert!(calls.lock().unwrap().is_empty(), "nothing was sent");

    for tool in DESKTOP_TOOLS {
        let allowed = registry
            .invoke_with_context(tool, args(tool, OTHER_PID, 7), context())
            .await;
        assert_ne!(
            allowed.is_error,
            Some(true),
            "{tool}: {:?}",
            allowed.content
        );
    }
    assert_eq!(calls.lock().unwrap().len(), DESKTOP_TOOLS.len());
}

#[tokio::test]
async fn a_batch_step_never_reaches_a_protected_browser_process() {
    let (registry, calls) = registry();
    let result = registry
        .invoke_with_context(
            "run_steps",
            json!({"session": "protect", "steps": [
                {"tool": "type_text", "args": {"pid": BROWSER_PID, "window_id": 9, "text": "secret"}},
            ]}),
            context(),
        )
        .await;
    assert_eq!(result.is_error, Some(true), "{:?}", result.content);
    assert!(
        calls.lock().unwrap().is_empty(),
        "the protected step sent nothing"
    );
    assert!(
        format!("{:?}", result.content).contains("browser_process_protected"),
        "{:?}",
        result.content
    );
}

#[tokio::test]
async fn desktop_scope_input_is_refused_where_its_points_cannot_be_checked() {
    let (registry, calls) = registry();
    let result = registry
        .invoke_with_context(
            "click",
            json!({"x": 10, "y": 10, "scope": "desktop", "session": "protect"}),
            context(),
        )
        .await;
    assert_eq!(code(&result).as_deref(), Some("browser_process_protected"));
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn desktop_points_on_a_protected_window_are_refused() {
    let protected = manifest(MANIFEST).unwrap();
    let owner = |x: f64, _y: f64| {
        if x < 100.0 {
            PointOwner::Process(OTHER_PID)
        } else if x < 200.0 {
            PointOwner::Process(BROWSER_PID)
        } else if x < 300.0 {
            PointOwner::Nothing
        } else {
            PointOwner::Unavailable
        }
    };
    assert!(desktop_point_refusal_for(&protected, "click", &[(10.0, 0.0)], owner).is_none());
    assert!(desktop_point_refusal_for(&protected, "click", &[(250.0, 0.0)], owner).is_none());
    let refused =
        desktop_point_refusal_for(&protected, "drag", &[(10.0, 0.0), (150.0, 0.0)], owner)
            .expect("the drag ends on the browser");
    assert_eq!(refused.code, BrowserRefusalCode::BrowserProcessProtected);
    assert!(desktop_point_refusal_for(&protected, "click", &[(350.0, 0.0)], owner).is_some());

    // Without a protected process nothing is checked.
    let unprotected = manifest("version: 3\nallow:\n  tools: [click]\n").unwrap();
    assert!(desktop_point_refusal_for(&unprotected, "click", &[(150.0, 0.0)], owner).is_none());
}

#[test]
fn origin_scoped_manifests_allow_desktop_tools_but_not_whole_page_or_desktop_reads() {
    manifest(MANIFEST).expect("origins plus desktop tools validate");
    for tool in ["page", "get_desktop_state"] {
        let source = format!(
            "version: 3\nresources:\n  browser:\n    origins: [\"https://app.example\"]\nallow:\n  tools: [{tool}]\n"
        );
        let error = manifest(&source).unwrap_err();
        assert!(error.contains(tool), "{error}");
    }
    // Listing a browser window never grants desktop access to it.
    let listed_only = manifest(
        "version: 3\nresources:\n  browser:\n    origins: [\"https://app.example\"]\n    existing_profiles:\n      - {pid: 4343, window_id: 9}\nallow:\n  tools: [get_window_state]\n",
    )
    .unwrap();
    assert!(listed_only
        .authorize_protected_resource(
            "private_observation",
            &json!({"kind": "window", "pid": 4343, "window_id": 9}),
        )
        .is_err());
}
