use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;

pub struct ListAppsTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "list_apps".into(),
        description: "List macOS apps, running and installed, with: `running` (pid is 0 when false), `active` \
            (system-frontmost), `launch_path` (the `.app` path; pass it as `name` to `launch_app`), `kind` \
            (`\"desktop\"`) and `last_used` (RFC3339, from the bundle's mtime). Unknown optional fields are \
            omitted, never null. Running entries are regular-policy apps; installed apps (from \
            /Applications, /Applications/Utilities, ~/Applications, /System/Applications and \
            /System/Applications/Utilities) report a live pid even when the process runs as an accessory. \
            For an open app's pid and window_id use `list_windows`; to open an app call `launch_app` \
            directly."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for ListAppsTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        let apps = tokio::task::spawn_blocking(crate::apps::list_all_apps)
            .await
            .unwrap_or_default();
        let text = crate::apps::format_app_list(&apps);
        let structured = serde_json::json!({
            "apps": apps.iter().map(app_record_json).collect::<Vec<_>>()
        });
        ToolResult::text(text).with_structured(structured)
    }
}

/// One `list_apps` record. `pid`, `name`, `running` and `active` are always
/// present; `bundle_id`, `launch_path`, `kind` and `last_used` are omitted
/// when unknown, as the contract allows. Every agent reads this payload, so it
/// carries no null or always-empty field (0.34 sent `windows: []`).
fn app_record_json(a: &crate::apps::AppInfo) -> Value {
    let mut entry = serde_json::json!({
        "pid": a.pid,
        "name": a.name,
        "bundle_id": a.bundle_id,
        "active": a.active,
        "running": a.running,
        "launch_path": a.launch_path,
        "kind": a.kind,
        "last_used": a.last_used,
    });
    if let Some(entry) = entry.as_object_mut() {
        entry.retain(|_, value| !value.is_null());
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::AppInfo;

    fn app(bundle_id: Option<&str>, launch_path: Option<&str>) -> AppInfo {
        AppInfo {
            name: "Example".into(),
            pid: 0,
            bundle_id: bundle_id.map(Into::into),
            running: false,
            active: false,
            launch_path: launch_path.map(Into::into),
            kind: Some("desktop".into()),
            last_used: None,
        }
    }

    #[test]
    fn record_omits_unknown_fields_and_the_empty_windows_list() {
        let record = app_record_json(&app(None, None));
        assert_eq!(
            record,
            serde_json::json!({
                "pid": 0,
                "name": "Example",
                "active": false,
                "running": false,
                "kind": "desktop",
            })
        );
        assert!(record.get("windows").is_none());
    }

    #[test]
    fn record_keeps_known_fields_and_matches_the_contract() {
        let record = app_record_json(&app(
            Some("org.example.App"),
            Some("/Applications/Example.app"),
        ));
        assert_eq!(record["bundle_id"], "org.example.App");
        assert_eq!(record["launch_path"], "/Applications/Example.app");
        assert!(record.get("last_used").is_none());
        let typed: cua_driver_contract::ListAppsOutput =
            serde_json::from_value(serde_json::json!({ "apps": [record] })).unwrap();
        assert_eq!(typed.apps[0].bundle_id.as_deref(), Some("org.example.App"));
        assert_eq!(typed.apps[0].last_used, None);
    }
}
