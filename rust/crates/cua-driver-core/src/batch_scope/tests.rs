//! Batch scope: the per-call check against fake process facts, and
//! end-to-end runs of run_steps and run_script whose fake tools record what
//! actually reached them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::*;
use crate::{
    protocol::{Content, ToolResult},
    tool::{Tool, ToolDef, ToolRegistry},
};

// ---------------------------------------------------------------- unit ----

#[derive(Default)]
struct Facts {
    processes: HashMap<i64, ProcessFacts>,
    bundles: HashMap<i64, String>,
    windows: Vec<(i64, u64)>,
    lookups: Mutex<Vec<String>>,
}

#[async_trait]
impl ScopeFacts for Facts {
    async fn process(&self, pid: i64) -> Option<ProcessFacts> {
        self.processes.get(&pid).cloned()
    }
    async fn bundle_id(&self, pid: i64) -> Option<String> {
        self.lookups.lock().unwrap().push(format!("bundle {pid}"));
        self.bundles.get(&pid).cloned()
    }
    async fn window_of(&self, pid: i64, window_id: u64) -> bool {
        self.lookups
            .lock()
            .unwrap()
            .push(format!("window {pid}/{window_id}"));
        self.windows.contains(&(pid, window_id))
    }
}

fn notes_and_mail() -> Facts {
    Facts {
        processes: HashMap::from([
            (
                42,
                ProcessFacts {
                    start_time: Some(1),
                    executable: Some("/Applications/Notes.app/Contents/MacOS/Notes".into()),
                },
            ),
            (
                99,
                ProcessFacts {
                    start_time: Some(1),
                    executable: Some("/Applications/Mail.app/Contents/MacOS/Mail".into()),
                },
            ),
        ]),
        bundles: HashMap::from([
            (42, "com.example.notes".into()),
            (99, "com.example.mail".into()),
        ]),
        windows: vec![(42, 7), (99, 9)],
        lookups: Mutex::default(),
    }
}

fn scope(value: Value) -> Arc<BatchScope> {
    BatchScope::parse(Some(&value)).unwrap().unwrap()
}

fn scope_of(value: Value) -> Arc<BatchScope> {
    scope(value)
}

async fn check(scope: &BatchScope, tool: &str, args: Value, facts: &Facts) -> Result<(), Refusal> {
    let read_only = matches!(
        tool,
        "get_window_state" | "zoom" | "verify_state" | "screenshot"
    );
    scope.check(tool, &args, read_only, facts).await
}

#[test]
fn scope_parsing_accepts_identities_and_rejects_malformed_scopes() {
    assert!(BatchScope::parse(None).unwrap().is_none());
    assert!(BatchScope::parse(Some(&Value::Null)).unwrap().is_none());
    let parsed = scope(
        json!({"apps": ["com.example.notes", "com.example.notes", "/usr/bin/gedit", r"C:\Program Files\Notes\notes.exe"]}),
    );
    assert_eq!(parsed.delivery(), ScopeDelivery::Background);
    assert_eq!(
        parsed.to_json(),
        json!({"apps": ["com.example.notes", "/usr/bin/gedit", r"C:\Program Files\Notes\notes.exe"], "delivery": "background"})
    );
    assert_eq!(
        scope(json!({"apps": ["a.b"], "delivery": "foreground"})).delivery(),
        ScopeDelivery::Foreground
    );
    for bad in [
        json!("com.example.notes"),
        json!({}),
        json!({"apps": []}),
        json!({"apps": "com.example.notes"}),
        json!({"apps": [7]}),
        json!({"apps": [""]}),
        json!({"apps": [" com.example.notes"]}),
        json!({"apps": ["usr/bin/gedit"]}),
        json!({"apps": ["/usr/bin/../bin/gedit"]}),
        json!({"apps": ["/usr/bin/"]}),
        json!({"apps": ["/"]}),
        json!({"apps": [r"C:\"]}),
        json!({"apps": [r"\\.\pipe\x"]}),
        json!({"apps": ["Notes App"]}),
        json!({"apps": ["a.b"], "delivery": "auto"}),
        json!({"apps": ["a.b"], "windows": [1]}),
        json!({"apps": (0..=MAX_SCOPE_APPS).map(|n| format!("a.b{n}")).collect::<Vec<_>>()}),
    ] {
        assert!(
            BatchScope::parse(Some(&bad)).is_err(),
            "{bad} should be refused"
        );
    }
}

#[tokio::test]
async fn a_call_on_a_scoped_app_window_passes_and_others_are_outside_scope() {
    let facts = notes_and_mail();
    let scope = scope(json!({"apps": ["com.example.notes"]}));
    check(&scope, "click", json!({"pid": 42, "window_id": 7}), &facts)
        .await
        .unwrap();
    // pid alone: the driver resolves that pid's own window.
    check(&scope, "press_key", json!({"pid": 42, "key": "a"}), &facts)
        .await
        .unwrap();
    let refused = check(&scope, "click", json!({"pid": 99, "window_id": 9}), &facts)
        .await
        .unwrap_err();
    assert_eq!(refused.code, OUTSIDE_SCOPE);
    assert_eq!((refused.pid, refused.window_id), (Some(99), Some(9)));
    // A window of another process named with a scoped pid.
    let refused = check(&scope, "click", json!({"pid": 42, "window_id": 9}), &facts)
        .await
        .unwrap_err();
    assert_eq!(refused.code, OUTSIDE_SCOPE);
    assert!(
        refused.message.contains("not a window of pid 42"),
        "{refused:?}"
    );
    // Reads are scoped as well.
    assert_eq!(
        check(
            &scope,
            "get_window_state",
            json!({"pid": 99, "window_id": 9}),
            &facts
        )
        .await
        .unwrap_err()
        .code,
        OUTSIDE_SCOPE
    );
    // Enumeration stays available.
    check(&scope, "list_windows", json!({}), &facts)
        .await
        .unwrap();
    check(&scope, "list_apps", json!({}), &facts).await.unwrap();
    // An unknown process is outside.
    assert_eq!(
        check(&scope, "click", json!({"pid": 5, "window_id": 1}), &facts)
            .await
            .unwrap_err()
            .code,
        OUTSIDE_SCOPE
    );
}

#[tokio::test]
async fn background_scope_refuses_foreground_and_desktop_wide_calls() {
    let facts = notes_and_mail();
    let background = scope(json!({"apps": ["com.example.notes"], "delivery": "background"}));
    for (tool, args) in [
        (
            "click",
            json!({"pid": 42, "window_id": 7, "delivery_mode": "foreground"}),
        ),
        ("bring_to_front", json!({"pid": 42, "window_id": 7})),
        ("activate_app", json!({"pid": 42})),
        (
            "invoke_menu",
            json!({"pid": 42, "window_id": 7, "path": ["File", "Save"]}),
        ),
        ("press_key", json!({"key": "return"})),
        ("hotkey", json!({"keys": ["cmd", "q"]})),
        ("move_cursor", json!({"x": 10, "y": 10})),
        ("click", json!({"x": 10, "y": 10, "scope": "desktop"})),
    ] {
        let refused = check(&background, tool, args.clone(), &facts)
            .await
            .unwrap_err();
        assert_eq!(refused.code, FOREGROUND_NOT_ALLOWED, "{tool} {args}");
    }
    // A screen-wide read is not foreground, but it is outside the apps.
    assert_eq!(
        check(&background, "screenshot", json!({}), &facts)
            .await
            .unwrap_err()
            .code,
        OUTSIDE_SCOPE
    );
    // The foreground check does not wait for an identity lookup.
    assert!(facts.lookups.lock().unwrap().is_empty());

    let foreground = scope(json!({"apps": ["com.example.notes"], "delivery": "foreground"}));
    check(
        &foreground,
        "click",
        json!({"pid": 42, "window_id": 7, "delivery_mode": "foreground"}),
        &facts,
    )
    .await
    .unwrap();
    check(
        &foreground,
        "bring_to_front",
        json!({"pid": 42, "window_id": 7}),
        &facts,
    )
    .await
    .unwrap();
    // Foreground still means the scoped apps' windows only.
    for (tool, args) in [
        ("press_key", json!({"key": "return"})),
        ("bring_to_front", json!({"pid": 99, "window_id": 9})),
    ] {
        assert_eq!(
            check(&foreground, tool, args, &facts)
                .await
                .unwrap_err()
                .code,
            OUTSIDE_SCOPE
        );
    }
}

#[tokio::test]
async fn executable_identities_match_the_attested_executable_only() {
    let mut facts = notes_and_mail();
    // A claimed bundle id equal to the path never stands in for the
    // attested executable.
    facts
        .bundles
        .insert(99, "/Applications/Notes.app/Contents/MacOS/Notes".into());
    let posix = scope(json!({"apps": ["/Applications/Notes.app/Contents/MacOS/Notes"]}));
    check(&posix, "click", json!({"pid": 42, "window_id": 7}), &facts)
        .await
        .unwrap();
    assert!(
        check(&posix, "click", json!({"pid": 99, "window_id": 9}), &facts)
            .await
            .is_err()
    );
    // POSIX paths are case-exact.
    let other_case = scope(json!({"apps": ["/applications/notes.app/Contents/MacOS/Notes"]}));
    assert!(check(
        &other_case,
        "click",
        json!({"pid": 42, "window_id": 7}),
        &facts
    )
    .await
    .is_err());

    let mut windows = notes_and_mail();
    windows.processes.insert(
        42,
        ProcessFacts {
            start_time: Some(1),
            executable: Some(r"\\?\C:\PROGRAM FILES\Notes\NOTES.EXE".into()),
        },
    );
    windows
        .bundles
        .insert(99, r"C:\Program Files\Notes\notes.exe".into());
    let windows_path = scope(json!({"apps": [r"c:/program files/notes/notes.exe"]}));
    check(
        &windows_path,
        "click",
        json!({"pid": 42, "window_id": 7}),
        &windows,
    )
    .await
    .unwrap();
    assert!(check(
        &windows_path,
        "click",
        json!({"pid": 99, "window_id": 9}),
        &windows
    )
    .await
    .is_err());
    // No attested executable: a path identity fails closed.
    windows.processes.remove(&42);
    let fresh = scope(json!({"apps": [r"C:\Program Files\Notes\notes.exe"]}));
    assert!(check(
        &fresh,
        "click",
        json!({"pid": 42, "window_id": 7}),
        &windows
    )
    .await
    .is_err());
}

#[tokio::test]
async fn verdicts_are_cached_per_process_instance() {
    let mut facts = notes_and_mail();
    let scope = scope(json!({"apps": ["com.example.notes"]}));
    for _ in 0..3 {
        check(&scope, "click", json!({"pid": 42, "window_id": 7}), &facts)
            .await
            .unwrap();
    }
    assert_eq!(
        *facts.lookups.lock().unwrap(),
        vec!["bundle 42".to_owned(), "window 42/7".to_owned()]
    );
    // The pid now belongs to another process instance (new start time).
    facts.processes.insert(
        42,
        ProcessFacts {
            start_time: Some(2),
            executable: None,
        },
    );
    facts.bundles.insert(42, "com.example.mail".into());
    assert_eq!(
        check(&scope, "click", json!({"pid": 42, "window_id": 7}), &facts)
            .await
            .unwrap_err()
            .code,
        OUTSIDE_SCOPE
    );
}

#[tokio::test]
async fn a_cached_verdict_never_outlives_the_attested_executable() {
    let mut facts = notes_and_mail();
    let scope = scope(json!({"apps": ["com.example.notes"]}));
    check(&scope, "click", json!({"pid": 42, "window_id": 7}), &facts)
        .await
        .unwrap();
    // Same pid and start time, but the process exec'd another executable.
    facts.processes.insert(
        42,
        ProcessFacts {
            start_time: Some(1),
            executable: Some("/usr/bin/osascript".into()),
        },
    );
    facts.bundles.remove(&42);
    assert_eq!(
        check(&scope, "click", json!({"pid": 42, "window_id": 7}), &facts)
            .await
            .unwrap_err()
            .code,
        OUTSIDE_SCOPE
    );

    // Without a start time nothing is cached: every call looks again.
    let mut facts = notes_and_mail();
    facts.processes.insert(
        42,
        ProcessFacts {
            start_time: None,
            executable: None,
        },
    );
    let scope = scope_of(json!({"apps": ["com.example.notes"]}));
    for _ in 0..2 {
        check(&scope, "click", json!({"pid": 42, "window_id": 7}), &facts)
            .await
            .unwrap();
    }
    assert_eq!(
        *facts.lookups.lock().unwrap(),
        vec!["bundle 42", "window 42/7", "bundle 42", "window 42/7"]
    );
    facts.bundles.insert(42, "com.example.mail".into());
    assert_eq!(
        check(&scope, "click", json!({"pid": 42, "window_id": 7}), &facts)
            .await
            .unwrap_err()
            .code,
        OUTSIDE_SCOPE
    );
}

#[tokio::test]
async fn launch_is_allowed_only_by_a_scoped_bundle_id_alone_on_macos() {
    let facts = notes_and_mail();
    let scope = scope(json!({"apps": ["com.example.notes", "/usr/bin/gedit"]}));
    let allowed = check(
        &scope,
        "launch_app",
        json!({"bundle_id": "com.example.notes", "urls": ["/tmp/a.txt"], "creates_new_application_instance": true}),
        &facts,
    )
    .await;
    if LAUNCH_BY_BUNDLE_ID {
        allowed.unwrap();
    } else {
        // Windows and Linux launch_app can pick another program than the
        // bundle id names, so a scoped batch cannot launch there.
        assert_eq!(allowed.unwrap_err().code, OUTSIDE_SCOPE);
    }
    for args in [
        json!({"bundle_id": "com.example.mail"}),
        json!({"name": "Notes"}),
        json!({"bundle_id": "/usr/bin/gedit"}),
        // A scoped bundle id next to a selector that could start another program.
        json!({"bundle_id": "com.example.notes", "name": "Mail"}),
        json!({"bundle_id": "com.example.notes", "path": r"C:\Windows\notepad.exe"}),
        json!({"bundle_id": "com.example.notes", "launch_path": "/Applications/Mail.app"}),
        json!({"bundle_id": "com.example.notes", "aumid": "Mail_8wekyb3d8bbwe!App"}),
        json!({"bundle_id": "com.example.notes", "webkit_inspector_port": 9222}),
    ] {
        assert_eq!(
            check(&scope, "launch_app", args.clone(), &facts)
                .await
                .unwrap_err()
                .code,
            OUTSIDE_SCOPE,
            "{args}"
        );
    }
}

#[test]
fn a_refusal_is_an_error_result_with_effect_refused_and_a_stable_code() {
    let result = Refusal {
        code: OUTSIDE_SCOPE,
        message: "pid 99 is not one of the scoped apps (com.example.notes)".into(),
        tool: "click".into(),
        pid: Some(99),
        window_id: Some(9),
    }
    .into_result();
    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["effect"], "refused");
    assert_eq!(structured["code"], OUTSIDE_SCOPE);
    assert_eq!(structured["refusal"]["code"], OUTSIDE_SCOPE);
    assert_eq!(structured["error"]["code"], OUTSIDE_SCOPE);
    assert_eq!(
        (structured["pid"].clone(), structured["window_id"].clone()),
        (json!(99), json!(9))
    );
}

// --------------------------------------------------------- end to end ----

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// Fake apps: Notes (pid 42, window 7, button "Save") and Mail (pid 99,
/// window 9, button "Send"). Actions answer `effect: unverifiable`; an
/// action asked to `need_foreground` refuses background delivery the way
/// Electron targets do.
struct Fake {
    def: ToolDef,
    calls: Calls,
}

#[async_trait]
impl Tool for Fake {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let name = self.def.name.clone();
        self.calls
            .lock()
            .unwrap()
            .push((name.clone(), args.clone()));
        let windows = [(42, 7, "Notes", "Draft"), (99, 9, "Mail", "Inbox")];
        match name.as_str() {
            "list_windows" => {
                let pid = args.get("pid").and_then(Value::as_i64);
                let listed: Vec<Value> = windows
                    .iter()
                    .filter(|(owner, ..)| pid.is_none_or(|pid| pid == *owner))
                    .map(|(pid, window_id, app, title)| {
                        json!({
                            "pid": pid, "window_id": window_id, "app_name": app, "title": title,
                            "z_index": 1, "is_on_screen": true,
                            "bounds": {"x": 0.0, "y": 0.0, "width": 800.0, "height": 600.0}
                        })
                    })
                    .collect();
                let mut listing = json!({ "windows": listed, "enumeration_complete": true });
                if pid.is_some() {
                    listing["authoritative_window_ids"] = listed
                        .iter()
                        .map(|window| window["window_id"].clone())
                        .collect();
                }
                ToolResult::text("windows").with_structured(listing)
            }
            "list_apps" => ToolResult::text("apps").with_structured(json!({"apps": [
                {"pid": 42, "name": "Notes", "bundle_id": "com.example.notes", "running": true, "active": false},
                {"pid": 99, "name": "Mail", "bundle_id": "com.example.mail", "running": true, "active": false}
            ]})),
            "get_window_state" => {
                let pid = args["pid"].as_i64().unwrap_or(0);
                let (window_id, label, snapshot) = if pid == 99 {
                    (9, "Send", "s00000002")
                } else {
                    (7, "Save", "s00000001")
                };
                ToolResult::text(format!("- [0] AXWindow\n  - [1] AXButton \"{label}\"")).with_structured(json!({
                    "pid": pid, "window_id": window_id, "snapshot_id": snapshot,
                    "tree_markdown": format!("- [0] AXWindow\n  - [1] AXButton \"{label}\"\n"),
                    "elements": [{"element_index": 1, "element_token": format!("{snapshot}:1"), "role": "AXButton", "label": label, "depth": 1}]
                }))
            }
            "launch_app" => ToolResult::text("launched").with_structured(json!({"pid": 42})),
            _ if args["fail_message"].is_string() => {
                ToolResult::error(args["fail_message"].as_str().unwrap_or_default().to_owned())
            }
            _ if args["need_foreground"] == true
                && args.get("delivery_mode").and_then(Value::as_str) != Some("foreground") =>
            {
                ToolResult::error(format!("{name}: the target ignores background input"))
                    .with_structured(json!({
                        "code": "background_unavailable",
                        "escalation": {"recommended": "foreground", "reason": "delivery_failed"}
                    }))
            }
            _ => ToolResult::text(format!("{name} done")).with_structured(json!({
                "effect": "unverifiable", "path": "ax"
            })),
        }
    }
}

struct Harness {
    registry: Arc<ToolRegistry>,
    calls: Calls,
}

impl Harness {
    fn new() -> Self {
        let calls: Calls = Arc::default();
        let mut registry = ToolRegistry::new();
        for name in crate::batch_tools::BATCHABLE_TOOLS.iter().copied().chain([
            "get_window_state",
            "list_windows",
            "list_apps",
            "launch_app",
            "verify_state",
            "zoom",
            "invoke_menu",
            "bring_to_front",
        ]) {
            let read_only = matches!(
                name,
                "get_window_state" | "list_windows" | "list_apps" | "verify_state" | "zoom"
            );
            let schema = if read_only || name == "launch_app" {
                json!({"type": "object"})
            } else {
                json!({"type": "object", "properties": {
                    "pid": {"type": "integer"},
                    "window_id": {"type": "integer"},
                    "element_token": {"type": "string"},
                    "delivery_mode": {"type": "string", "enum": ["background", "foreground"]}
                }})
            };
            registry.register(Box::new(Fake {
                def: ToolDef {
                    name: name.into(),
                    description: "fake".into(),
                    input_schema: schema,
                    read_only,
                    destructive: false,
                    idempotent: false,
                    open_world: false,
                },
                calls: calls.clone(),
            }));
        }
        registry.register_session_tools();
        #[cfg(feature = "script")]
        registry.register_script_tool();
        let registry = Arc::new(registry);
        registry.init_self_weak();
        Self { registry, calls }
    }

    async fn run(&self, tool: &str, args: Value) -> (ToolResult, Value) {
        let result = self.registry.invoke(tool, args).await;
        let structured = result.structured_content.clone().unwrap_or(Value::Null);
        (result, structured)
    }

    /// Calls of `tool` that reached the fake, with their pid.
    fn sent(&self, tool: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == tool)
            .map(|(_, args)| args.clone())
            .collect()
    }
}

fn text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The batch tools this build registers.
const BATCH_TOOLS: &[&str] = &[
    "run_steps",
    #[cfg(feature = "script")]
    "run_script",
];

const NOTES_ONLY: &str = "com.example.notes";

#[tokio::test]
async fn u09_steps_predispatch_refusal_is_not_delivered() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_steps",
            json!({
                "app_scope":{"apps":[NOTES_ONLY]},
                "steps":[{"click":{"pid":99,"window_id":9,"element_token":"s00000002:1"}}]
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(true), "{out}");
    assert_eq!(out["steps"][0]["code"], OUTSIDE_SCOPE);
    assert_eq!(out["steps"][0]["delivery"], "not_delivered");
    assert!(harness.sent("click").is_empty());
    assert_eq!(
        result.disposition,
        Some(cua_driver_contract::ErrorDisposition::NOT_DELIVERED)
    );
    let wire = crate::mcp_result::conforming_tool_result(
        "run_steps",
        serde_json::to_value(result).unwrap(),
    );
    assert_eq!(
        wire["structuredContent"]["disposition"],
        json!({"delivery":"not_delivered","retry":"safe"})
    );
}

#[cfg(feature = "script")]
#[tokio::test]
async fn u09_script_predispatch_refusal_is_not_delivered() {
    let harness = Harness::new();
    let (result, out) = harness.run("run_script", json!({
        "app_scope":{"apps":[NOTES_ONLY]},
        "script":"await cua.call('click', {pid: 99, window_id: 9, element_token: 's00000002:1'});"
    })).await;
    assert_eq!(result.is_error, Some(true), "{out}");
    assert_eq!(out["error"]["code"], OUTSIDE_SCOPE);
    assert_eq!(out["calls"][0]["delivery"], "not_delivered");
    assert_eq!(
        out["calls"][0]["driver_calls"][0]["delivery"],
        "not_delivered"
    );
    assert!(harness.sent("click").is_empty());
    assert_eq!(
        result.disposition,
        Some(cua_driver_contract::ErrorDisposition::NOT_DELIVERED)
    );
    let wire = crate::mcp_result::conforming_tool_result(
        "run_script",
        serde_json::to_value(result).unwrap(),
    );
    assert_eq!(
        wire["structuredContent"]["disposition"],
        json!({"delivery":"not_delivered","retry":"safe"})
    );
}

#[tokio::test]
async fn u09_steps_foreground_scope_refusal_is_not_delivered() {
    let harness = Harness::new();
    let (result, out) = harness.run("run_steps", json!({
        "app_scope":{"apps":[NOTES_ONLY]},
        "steps":[{"click":{"pid":42,"window_id":7,"element_token":"s00000001:1","delivery_mode":"foreground"}}]
    })).await;
    assert_eq!(out["steps"][0]["code"], FOREGROUND_NOT_ALLOWED);
    assert!(harness.sent("click").is_empty());
    let wire = crate::mcp_result::conforming_tool_result(
        "run_steps",
        serde_json::to_value(result).unwrap(),
    );
    assert_eq!(
        wire["structuredContent"]["disposition"],
        json!({"delivery":"not_delivered","retry":"safe"})
    );
}

#[cfg(feature = "script")]
#[tokio::test]
async fn u09_script_foreground_scope_refusal_is_not_delivered() {
    let harness = Harness::new();
    let (result, out) = harness.run("run_script", json!({
        "app_scope":{"apps":[NOTES_ONLY]},
        "script":"await cua.call('click', {pid: 42, window_id: 7, element_token: 's00000001:1', delivery_mode: 'foreground'});"
    })).await;
    assert_eq!(out["error"]["code"], FOREGROUND_NOT_ALLOWED);
    assert!(harness.sent("click").is_empty());
    let wire = crate::mcp_result::conforming_tool_result(
        "run_script",
        serde_json::to_value(result).unwrap(),
    );
    assert_eq!(
        wire["structuredContent"]["disposition"],
        json!({"delivery":"not_delivered","retry":"safe"})
    );
}

#[tokio::test]
async fn run_steps_refuses_a_step_outside_the_scope_and_stops_there() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_steps",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "steps": [
                    {"click": {"pid": 42, "window_id": 7, "element_token": "s00000001:1"}},
                    {"click": {"pid": 99, "window_id": 9, "element_token": "s00000002:1"}},
                    {"press_key": {"pid": 42, "window_id": 7, "key": "a"}}
                ]
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(out["failed_step"], 1);
    assert_eq!(out["executed"], 2);
    assert_eq!(
        out["app_scope"],
        json!({"apps": [NOTES_ONLY], "delivery": "background"})
    );
    assert_eq!(out["steps"][0]["effect"], "unverifiable");
    assert_eq!(out["steps"][1]["effect"], "refused");
    assert_eq!(out["steps"][1]["code"], OUTSIDE_SCOPE);
    let clicks = harness.sent("click");
    assert_eq!(clicks.len(), 1, "{clicks:?}");
    assert_eq!(clicks[0]["pid"], 42);
    assert!(harness.sent("press_key").is_empty(), "the batch stopped");
    assert_eq!(
        result.disposition,
        Some(cua_driver_contract::ErrorDisposition::DELIVERED)
    );
}

#[tokio::test]
async fn run_steps_refuses_a_lookup_in_a_window_outside_the_scope() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_steps",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "steps": [{"click": {"app": "Mail", "role": "button", "name": "Send"}}]
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(out["steps"][0]["code"], OUTSIDE_SCOPE, "{out}");
    assert_eq!(out["steps"][0]["effect"], "refused");
    assert!(harness.sent("click").is_empty());
    assert!(
        harness
            .sent("get_window_state")
            .iter()
            .all(|args| args["pid"] != 99),
        "Mail's window was never read"
    );
}

#[tokio::test]
async fn run_steps_background_scope_refuses_foreground_steps_and_the_foreground_fallback() {
    let harness = Harness::new();
    let (_, out) = harness
        .run(
            "run_steps",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "steps": [{"click": {"pid": 42, "window_id": 7, "element_token": "s00000001:1", "delivery_mode": "foreground"}}]
            }),
        )
        .await;
    assert_eq!(out["steps"][0]["code"], FOREGROUND_NOT_ALLOWED, "{out}");
    assert_eq!(out["steps"][0]["effect"], "refused");
    assert!(harness.sent("click").is_empty());

    // The retry foreground_fallback would send is refused too: only the
    // background attempt reached the app.
    let (_, out) = harness
        .run(
            "run_steps",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "foreground_fallback": true,
                "steps": [{"click": {"pid": 42, "window_id": 7, "element_token": "s00000001:1", "need_foreground": true}}]
            }),
        )
        .await;
    assert_eq!(out["steps"][0]["code"], FOREGROUND_NOT_ALLOWED, "{out}");
    let clicks = harness.sent("click");
    assert_eq!(clicks.len(), 1, "{clicks:?}");
    assert!(clicks[0]
        .get("delivery_mode")
        .is_none_or(|mode| mode == "background"));

    // A desktop-wide key press names no window.
    let (_, out) = harness
        .run(
            "run_steps",
            json!({"app_scope": {"apps": [NOTES_ONLY]}, "steps": [{"press_key": {"key": "return"}}]}),
        )
        .await;
    assert_eq!(out["steps"][0]["code"], FOREGROUND_NOT_ALLOWED, "{out}");
    assert!(harness.sent("press_key").is_empty());
}

#[tokio::test]
async fn run_steps_without_scope_is_unchanged_and_a_bad_scope_runs_nothing() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_steps",
            json!({"steps": [{"click": {"pid": 99, "window_id": 9, "element_token": "s00000002:1"}}]}),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert!(out.get("app_scope").is_none());
    assert_eq!(harness.sent("click").len(), 1);

    let (result, out) = harness
        .run(
            "run_steps",
            json!({"app_scope": {"apps": []}, "steps": [{"click": {"pid": 42, "window_id": 7}}]}),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(out["code"], "invalid_batch");
    assert_eq!(harness.sent("click").len(), 1, "nothing ran");
}

#[cfg(feature = "script")]
#[tokio::test]
async fn run_script_refuses_calls_outside_the_scope_and_journals_every_effect() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_script",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "script": "const notes = await cua.getApp({pid: 42, windowId: 7});\nawait notes.click({role: 'button', name: 'Save'});\nconst mail = await cua.getApp('Mail');\nawait mail.click([10, 10]);\nreturn 1;"
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(out["error"]["code"], OUTSIDE_SCOPE, "{out}");
    assert_eq!(out["app_scope"]["apps"], json!([NOTES_ONLY]));
    let calls = out["calls"].as_array().unwrap();
    let save = calls
        .iter()
        .find(|call| call["op"] == "click" && call["ok"] == true)
        .unwrap();
    assert_eq!(save["tool"], "click");
    assert_eq!(save["effect"], "unverifiable");
    assert_eq!(
        (save["pid"].clone(), save["window_id"].clone()),
        (json!(42), json!(7))
    );
    let refused = calls.last().unwrap();
    assert_eq!(refused["op"], "click");
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["effect"], "refused");
    assert_eq!(refused["code"], OUTSIDE_SCOPE);
    assert_eq!(refused["pid"], 99);
    let clicks = harness.sent("click");
    assert_eq!(clicks.len(), 1, "{clicks:?}");
    assert_eq!(clicks[0]["pid"], 42);
    assert_eq!(
        result.disposition,
        Some(cua_driver_contract::ErrorDisposition::DELIVERED)
    );
    assert!(
        text(&result).contains("click ERROR, refused (outside_scope)"),
        "{}",
        text(&result)
    );
}

#[cfg(feature = "script")]
#[tokio::test]
async fn a_script_stops_at_its_first_scope_refusal_even_when_it_catches_it() {
    for (first, code) in [
        (
            "cua.call('press_key', {key: 'return'})",
            FOREGROUND_NOT_ALLOWED,
        ),
        (
            "cua.call('launch_app', {bundle_id: 'com.example.mail'})",
            OUTSIDE_SCOPE,
        ),
        (
            "(await cua.getApp('Mail')).query({role: 'button'})",
            OUTSIDE_SCOPE,
        ),
    ] {
        let harness = Harness::new();
        let script = format!(
            "let caught = null;\ntry {{ await {first}; }} catch (error) {{ caught = error.code; }}\nawait cua.call('click', {{pid: 42, window_id: 7, element_token: 's00000001:1'}});\nreturn caught;"
        );
        let (result, out) = harness
            .run(
                "run_script",
                json!({"app_scope": {"apps": [NOTES_ONLY]}, "script": script}),
            )
            .await;
        assert_eq!(result.is_error, Some(true), "{first}: {}", text(&result));
        assert_eq!(out["ok"], false);
        assert_eq!(out["error"]["code"], code, "{first}: {out}");
        assert_eq!(out["error"]["line"], 2, "{first}: {out}");
        assert!(
            harness.sent("click").is_empty(),
            "{first}: no call after the refusal"
        );
        assert!(harness.sent("press_key").is_empty());
        assert!(harness.sent("launch_app").is_empty());
        assert!(harness
            .sent("get_window_state")
            .iter()
            .all(|args| args["pid"] != 99));
        let refused = out["calls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|call| call["ok"] == false)
            .cloned()
            .unwrap();
        assert_eq!(refused["effect"], "refused", "{first}: {refused}");
        assert_eq!(refused["code"], code);
    }
}

#[cfg(feature = "script")]
#[tokio::test]
async fn run_script_records_effects_without_a_scope_and_rejects_a_bad_one() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_script",
            json!({"script": "const app = await cua.getApp({pid: 99, windowId: 9});\nawait app.click([5, 5]);\nreturn 1;"}),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let click = out["calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|call| call["op"] == "click")
        .cloned()
        .unwrap();
    assert_eq!(click["effect"], "unverifiable");
    assert_eq!(click["tool"], "click");
    assert!(out.get("app_scope").is_none());

    let (result, out) = harness
        .run(
            "run_script",
            json!({"script": "return 1;", "app_scope": {"apps": ["com.example.notes"], "delivery": "sometimes"}}),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(out["code"], "invalid_script");
}

#[test]
fn run_steps_and_run_script_advertise_scope_in_their_input_schemas() {
    let harness = Harness::new();
    for tool in BATCH_TOOLS {
        let def = harness.registry.get_def(tool).unwrap();
        assert_eq!(
            def.input_schema["properties"]["app_scope"],
            input_schema(),
            "{tool}"
        );
        assert!(
            !def.input_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("app_scope")),
            "{tool}: app_scope is optional"
        );
    }
    let schema = input_schema();
    assert_eq!(schema["required"], json!(["apps"]));
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["delivery"]["enum"],
        json!(["background", "foreground"])
    );
}

#[tokio::test]
async fn a_gone_check_on_a_refused_window_fails_the_step_instead_of_passing() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_steps",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "steps": [
                    {"expect": [{"app": "Mail", "text": "Inbox", "gone": true, "timeout_ms": 0}]},
                    {"press_key": {"pid": 42, "window_id": 7, "key": "a"}}
                ]
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(out["failed_step"], 0, "{out}");
    assert_eq!(out["steps"][0]["code"], OUTSIDE_SCOPE);
    assert_eq!(out["steps"][0]["effect"], "refused");
    assert!(harness.sent("press_key").is_empty(), "the batch stopped");

    #[cfg(feature = "script")]
    {
        let (result, out) = harness
            .run(
                "run_script",
                json!({
                    "app_scope": {"apps": [NOTES_ONLY]},
                    "script": "const mail = await cua.getApp('Mail');\nawait mail.waitFor({text: 'Inbox', gone: true, timeout_ms: 0});\nawait cua.call('press_key', {pid: 42, window_id: 7, key: 'a'});\nreturn 1;"
                }),
            )
            .await;
        assert_eq!(result.is_error, Some(true), "{}", text(&result));
        assert_eq!(out["error"]["code"], OUTSIDE_SCOPE, "{out}");
        assert!(harness.sent("press_key").is_empty());
    }
}

#[cfg(feature = "script")]
#[tokio::test]
async fn the_script_journal_lists_every_driver_call_of_each_op() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_script",
            json!({"script": "const app = await cua.getApp('Notes');\nawait app.click({role: 'button', name: 'Save'});\nreturn 1;"}),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let calls = out["calls"].as_array().unwrap();
    let get_app = calls.iter().find(|call| call["op"] == "get_app").unwrap();
    assert_eq!(get_app["tool"], "list_windows", "{get_app}");
    let click = calls.iter().find(|call| call["op"] == "click").unwrap();
    let tools: Vec<&str> = click["driver_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["tool"].as_str().unwrap())
        .collect();
    assert_eq!(tools.last(), Some(&"click"), "{click}");
    assert!(tools.contains(&"get_window_state"), "{click}");
    let last = click["driver_calls"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["effect"], "unverifiable");
    assert_eq!(
        (last["pid"].clone(), last["window_id"].clone()),
        (json!(42), json!(7))
    );
}

#[test]
fn the_batch_scope_argument_keeps_to_the_shared_schema_canon() {
    // `scope` is the shared window/desktop action argument, so the batch
    // argument has its own name.
    let harness = Harness::new();
    for tool in BATCH_TOOLS {
        let def = harness.registry.get_def(tool).unwrap();
        assert!(
            def.input_schema["properties"].get("scope").is_none(),
            "{tool}"
        );
        assert_eq!(
            crate::tool_schema::shared_schema_violations(tool, &def.input_schema),
            Vec::<String>::new(),
            "{tool}"
        );
    }
}

#[tokio::test]
async fn a_refused_precondition_never_lets_the_same_step_act() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_steps",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "steps": [{
                    "wait_for": {"pid": 99, "window_id": 9, "text": "Inbox", "gone": true, "timeout_ms": 0},
                    "click": {"pid": 42, "window_id": 7, "element_token": "s00000001:1"}
                }]
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(out["steps"][0]["code"], OUTSIDE_SCOPE, "{out}");
    assert_eq!(out["steps"][0]["effect"], "refused");
    assert!(harness.sent("click").is_empty(), "the action was not sent");
    assert_eq!(
        result.disposition,
        Some(cua_driver_contract::ErrorDisposition::NOT_DELIVERED)
    );
    let wire = crate::mcp_result::conforming_tool_result(
        "run_steps",
        serde_json::to_value(&result).unwrap(),
    );
    assert_eq!(
        wire["structuredContent"]["disposition"],
        json!({"delivery":"not_delivered", "retry":"safe"})
    );
}

#[tokio::test]
async fn a_refused_final_read_fails_the_batch() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_steps",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "steps": [{"click": {"pid": 42, "window_id": 7, "element_token": "s00000001:1"}}],
                "observe": {"pid": 99, "window_id": 9}
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(out["ok"], false);
    assert_eq!(out["code"], OUTSIDE_SCOPE);
    assert_eq!(out["observation"]["code"], OUTSIDE_SCOPE, "{out}");
    assert_eq!(out["observation"]["effect"], "refused");
    assert_eq!(
        result.disposition,
        Some(cua_driver_contract::ErrorDisposition::DELIVERED)
    );
    assert_eq!(
        out["steps"][0]["effect"], "unverifiable",
        "the step itself ran"
    );
    assert!(harness
        .sent("get_window_state")
        .iter()
        .all(|args| args["pid"] != 99));
}

#[cfg(feature = "script")]
#[tokio::test]
async fn the_journal_keeps_a_typed_window_target() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_script",
            json!({"script": "await cua.call('click', {target: {kind: 'window', pid: 42, window_id: 7}, element_token: 's00000001:1'});\nreturn 1;"}),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let click = out["calls"][0].clone();
    assert_eq!(
        (click["pid"].clone(), click["window_id"].clone()),
        (json!(42), json!(7)),
        "{click}"
    );
    let inner = click["driver_calls"][0].clone();
    assert_eq!(
        (inner["pid"].clone(), inner["window_id"].clone()),
        (json!(42), json!(7)),
        "{inner}"
    );
}

#[tokio::test]
async fn a_background_scope_never_reroutes_a_click_through_the_menu_bar() {
    // A menu-bar item token refused as outside the window would normally run
    // as invoke_menu, which can focus or raise the window.
    crate::window_state_view::remember_for_test(
        "s0000b0c1",
        42,
        7,
        "- [0] AXWindow \"Draft\"\n- [27] AXMenuBar\n  - [700] AXMenuBarItem \"File\"\n    - [701] AXMenu\n      - [702] AXMenuItem \"Save\"\n",
    );
    let refused = "Background input refused (element_outside_target_window): the addressed element could not be proven to belong to window 7";
    let step = json!({"click": {"pid": 42, "window_id": 7, "element_token": "s0000b0c1:702", "fail_message": refused}});

    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_steps",
            json!({"app_scope": {"apps": [NOTES_ONLY]}, "steps": [step.clone()]}),
        )
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert!(harness.sent("invoke_menu").is_empty(), "{out}");
    assert!(out["steps"][0].get("refound").is_none(), "{out}");

    // With foreground allowed, the reroute still runs on the scoped window.
    let harness = Harness::new();
    let (_, out) = harness
        .run(
            "run_steps",
            json!({"app_scope": {"apps": [NOTES_ONLY], "delivery": "foreground"}, "steps": [step]}),
        )
        .await;
    let menus = harness.sent("invoke_menu");
    assert_eq!(menus.len(), 1, "{out}");
    assert_eq!(menus[0]["path"], json!(["File", "Save"]));
}

#[tokio::test]
async fn nothing_is_read_after_a_step_the_scope_refused() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_steps",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "steps": [
                    {"press_key": {"pid": 42, "window_id": 7, "key": "a", "delivery_mode": "foreground"}},
                    {"zoom": {"pid": 42, "window_id": 7, "x1": 0, "y1": 0, "x2": 10, "y2": 10}}
                ],
                "observe": {"pid": 42, "window_id": 7}
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(out["steps"][0]["code"], FOREGROUND_NOT_ALLOWED, "{out}");
    assert!(out.get("observation").is_none(), "{out}");
    assert!(out.get("reads").is_none(), "{out}");
    assert!(
        harness.sent("get_window_state").is_empty(),
        "no read after the refusal"
    );
    assert!(harness.sent("zoom").is_empty());
    assert!(harness.sent("press_key").is_empty());
}

#[cfg(feature = "script")]
#[tokio::test]
async fn a_desktop_click_is_refused_by_the_scope_before_the_delivery_guard() {
    // The delivery guard would answer background_unavailable, which a script
    // can catch and carry on from; the scope must answer first.
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_script",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "script": "let caught = null;\ntry { await cua.call('click', {target: {kind: 'desktop', display_id: 'primary'}, x: 10, y: 10, delivery_mode: 'background'}); } catch (error) { caught = error.code; }\nawait cua.call('click', {pid: 42, window_id: 7, element_token: 's00000001:1'});\nreturn caught;"
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(out["error"]["code"], FOREGROUND_NOT_ALLOWED, "{out}");
    assert!(
        harness.sent("click").is_empty(),
        "nothing after the refusal"
    );
}

#[tokio::test]
async fn the_locator_makes_no_second_attempt_after_a_refused_read() {
    use crate::batch_tools::locate::{ElementSpec, Locator, WindowSpec};
    let harness = Harness::new();
    let scope = scope(json!({"apps": [NOTES_ONLY]}));
    let registry = harness.registry.clone();
    let journal = enter(Some(scope), async {
        // With an observe baseline, the first read of a window is a baseline
        // read whose own failure is not reported.
        let locator = Locator::new(
            &registry,
            None,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .with_observe_baseline(250);
        let found = locator
            .find(
                &WindowSpec {
                    app: Some("Mail".into()),
                    ..WindowSpec::default()
                },
                &ElementSpec {
                    role: Some("button".into()),
                    name: Some("Send".into()),
                    ..ElementSpec::default()
                },
                std::time::Duration::ZERO,
            )
            .await;
        assert!(found.is_err());
        assert_eq!(locator.scope_refusal(), Some(OUTSIDE_SCOPE));
        locator.take_journal()
    })
    .await;
    let reads: Vec<&Value> = journal
        .iter()
        .filter(|entry| entry["tool"] == "get_window_state")
        .collect();
    assert_eq!(
        reads.len(),
        1,
        "one refused read, no second attempt: {journal:?}"
    );
    assert_eq!(reads[0]["code"], OUTSIDE_SCOPE);
}

#[cfg(feature = "script")]
#[tokio::test]
async fn a_call_outside_the_scope_is_refused_even_with_invalid_arguments() {
    // Argument normalization errors (a scroll's invalid dx/dy, a legacy
    // desktop scope on a window where the backend refuses it) must not
    // answer before the scope: a script could catch them and carry on.
    for first in [
        "cua.call('scroll', {pid: 99, window_id: 9, dx: 1, dy: 1})",
        "cua.call('click', {pid: 99, window_id: 9, x: 1, y: 1, scope: 'desktop'})",
        "cua.call('click', {target: {kind: 'window', pid: 99, window_id: 9}, x: 1, y: 1, scope: 'desktop'})",
    ] {
        let harness = Harness::new();
        let script = format!(
            "let caught = null;\ntry {{ await {first}; }} catch (error) {{ caught = error.code; }}\nawait cua.call('click', {{pid: 42, window_id: 7, element_token: 's00000001:1'}});\nreturn caught;"
        );
        let (result, out) = harness
            .run(
                "run_script",
                json!({"app_scope": {"apps": [NOTES_ONLY]}, "script": script}),
            )
            .await;
        assert_eq!(result.is_error, Some(true), "{first}: {}", text(&result));
        assert_eq!(out["error"]["code"], OUTSIDE_SCOPE, "{first}: {out}");
        assert!(harness.sent("click").is_empty(), "{first}: nothing after the refusal");
        assert!(harness.sent("scroll").is_empty());
    }
}

#[cfg(feature = "script")]
#[tokio::test]
async fn a_call_outside_the_scope_is_refused_before_post_action_wait_is_parsed() {
    // macOS parses post_action_wait before anything else in dispatch; an
    // invalid value must not answer before the scope.
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_script",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "script": "let caught = null;\ntry { await cua.call('click', {pid: 99, window_id: 9, x: 1, y: 1, post_action_wait: 'off'}); } catch (error) { caught = error.code; }\nawait cua.call('click', {pid: 42, window_id: 7, element_token: 's00000001:1'});\nreturn caught;"
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(out["error"]["code"], OUTSIDE_SCOPE, "{out}");
    assert!(
        harness.sent("click").is_empty(),
        "nothing after the refusal"
    );
}

#[cfg(feature = "script")]
#[tokio::test]
async fn a_scoped_typed_window_target_is_accepted_on_its_exact_window() {
    let harness = Harness::new();
    let (result, out) = harness
        .run(
            "run_script",
            json!({
                "app_scope": {"apps": [NOTES_ONLY]},
                "script": "await cua.call('click', {target: {kind: 'window', pid: 42, window_id: 7}, element_token: 's00000001:1'});\nreturn 1;"
            }),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let clicks = harness.sent("click");
    assert_eq!(clicks.len(), 1, "{out}");
    assert_eq!(
        (clicks[0]["pid"].clone(), clicks[0]["window_id"].clone()),
        (json!(42), json!(7))
    );
    assert_eq!(out["calls"][0]["effect"], "unverifiable");
}
