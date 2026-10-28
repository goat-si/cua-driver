//! Per-call app and delivery scope for `run_steps` and `run_script`.
//!
//! A host that limits a turn to some apps cannot see inside a batch or a
//! script before it runs. With `scope`, the driver enforces that limit
//! itself: every call the batch or script dispatches, including the reads
//! the locator makes to find a target, the menu-bar reroute and a
//! `foreground_fallback` retry, is checked here before any authorization,
//! consent or platform code runs. A call outside the scope is refused with
//! `effect: "refused"` and a stable code, and nothing is sent.
//!
//! The scope travels as a task-local installed by the batch tools, never as
//! a call argument, so a step or a script cannot widen or drop it. App
//! identity follows the capability manifest's rules (see
//! [`crate::session_manifest`]): a bundle or package id is compared exactly
//! with the identity `list_apps` reports for the pid; a POSIX executable
//! path is compared case-exactly with the executable the OS reports for the
//! pid; a Windows executable path is compared after Windows normalization
//! with that same OS-read executable.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::protocol::ToolResult;

/// Refusal code: the call targets no window of an app in `scope.apps`.
pub const OUTSIDE_SCOPE: &str = "outside_scope";
/// Refusal code: the call needs foreground or desktop-wide delivery and the
/// scope allows background delivery only.
pub const FOREGROUND_NOT_ALLOWED: &str = "foreground_not_allowed";
/// Most app identities one scope may name.
pub const MAX_SCOPE_APPS: usize = 32;

/// Reads that enumerate apps or windows and target none. They stay
/// available in a scope so a batch can find its app's window by name.
const ENUMERATION_TOOLS: &[&str] = &["list_apps", "list_windows"];
/// Tools that always bring a window forward.
const FOREGROUND_TOOLS: &[&str] = &["bring_to_front", "activate_app"];

/// Delivery a scope allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeDelivery {
    /// Background delivery to the scoped apps' windows only.
    Background,
    /// Foreground delivery to the scoped apps' windows is also allowed.
    Foreground,
}

impl ScopeDelivery {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeDelivery::Background => "background",
            ScopeDelivery::Foreground => "foreground",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AppIdentity {
    /// macOS bundle id, Windows package family name or AUMID, Linux app id:
    /// compared exactly with the identity `list_apps` reports for the pid.
    Bundle(String),
    /// Absolute POSIX executable path, compared case-exactly with the
    /// executable the OS reports for the pid.
    PosixExecutable(String),
    /// Windows executable path (drive or UNC), compared after Windows
    /// normalization with the executable the OS reports for the pid.
    WindowsExecutable(String),
}

/// What the registry can attest about a running process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessFacts {
    /// Process start time from the OS, when the platform can read it. Keys
    /// the per-call cache so a reused pid is checked again.
    pub start_time: Option<u64>,
    /// Executable path the OS reports for the pid.
    pub executable: Option<String>,
}

/// Lookups the scope check needs. The registry implements it over its own
/// platform tools; tests fake it.
#[async_trait]
pub trait ScopeFacts: Send + Sync {
    /// OS-attested facts for `pid`; `None` when the process is gone or the
    /// platform cannot read them.
    async fn process(&self, pid: i64) -> Option<ProcessFacts>;
    /// The bundle/package id `list_apps` reports for `pid`.
    async fn bundle_id(&self, pid: i64) -> Option<String>;
    /// Whether `window_id` is a window of `pid`.
    async fn window_of(&self, pid: i64, window_id: u64) -> bool;
}

/// A parsed `scope` argument plus the verdicts it cached during one call.
#[derive(Debug)]
pub struct BatchScope {
    apps: Vec<AppIdentity>,
    raw_apps: Vec<String>,
    delivery: ScopeDelivery,
    /// (pid, start time) -> whether that process is one of the scoped apps.
    processes: Mutex<HashMap<(i64, Option<u64>), bool>>,
    /// (pid, start time, window id) confirmed to be a window of that process.
    windows: Mutex<HashSet<(i64, Option<u64>, u64)>>,
}

impl BatchScope {
    /// Parse a `scope` argument. `None` and `null` mean no scope.
    pub fn parse(value: Option<&Value>) -> Result<Option<Arc<Self>>, String> {
        let object = match value {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::Object(object)) => object,
            Some(_) => return Err("`scope` must be an object {apps, delivery?}".into()),
        };
        if let Some(key) = object
            .keys()
            .find(|key| !matches!(key.as_str(), "apps" | "delivery"))
        {
            return Err(format!("`scope` has unknown field `{key}`"));
        }
        let raw = object
            .get("apps")
            .and_then(Value::as_array)
            .ok_or("`scope.apps` must be a non-empty array of app identities")?;
        if raw.is_empty() || raw.len() > MAX_SCOPE_APPS {
            return Err(format!(
                "`scope.apps` must hold 1 to {MAX_SCOPE_APPS} app identities, got {}",
                raw.len()
            ));
        }
        let mut apps = Vec::with_capacity(raw.len());
        let mut raw_apps = Vec::with_capacity(raw.len());
        for entry in raw {
            let text = entry
                .as_str()
                .ok_or("each `scope.apps` entry must be a string")?;
            let identity = parse_identity(text)?;
            if !apps.contains(&identity) {
                apps.push(identity);
                raw_apps.push(text.to_owned());
            }
        }
        let delivery = match object.get("delivery") {
            None | Some(Value::Null) => ScopeDelivery::Background,
            Some(Value::String(mode)) if mode == "background" => ScopeDelivery::Background,
            Some(Value::String(mode)) if mode == "foreground" => ScopeDelivery::Foreground,
            Some(_) => {
                return Err("`scope.delivery` must be \"background\" or \"foreground\"".into())
            }
        };
        Ok(Some(Arc::new(Self {
            apps,
            raw_apps,
            delivery,
            processes: Mutex::default(),
            windows: Mutex::default(),
        })))
    }

    pub fn delivery(&self) -> ScopeDelivery {
        self.delivery
    }

    /// The scope as the caller wrote it, normalized (duplicates dropped,
    /// delivery filled in).
    pub fn to_json(&self) -> Value {
        json!({"apps": self.raw_apps, "delivery": self.delivery.as_str()})
    }

    /// Check one call. `read_only` is the tool's own declaration. `args`
    /// are the call's arguments after the registry normalized them.
    pub async fn check(
        &self,
        tool: &str,
        args: &Value,
        read_only: bool,
        facts: &dyn ScopeFacts,
    ) -> Result<(), Refusal> {
        if ENUMERATION_TOOLS.contains(&tool) {
            return Ok(());
        }
        if tool == "launch_app" {
            return self.check_launch(args);
        }
        let pid = args
            .get("pid")
            .and_then(Value::as_i64)
            .filter(|pid| *pid > 0);
        let window_id = args.get("window_id").and_then(Value::as_u64);
        let refusal = |code: &'static str, message: String| Refusal {
            code,
            message,
            tool: tool.to_owned(),
            pid,
            window_id,
        };
        if self.delivery == ScopeDelivery::Background {
            if FOREGROUND_TOOLS.contains(&tool) {
                return Err(refusal(
                    FOREGROUND_NOT_ALLOWED,
                    format!("{tool} brings a window to the front, and this scope allows background delivery only"),
                ));
            }
            if args.get("delivery_mode").and_then(Value::as_str) == Some("foreground") {
                return Err(refusal(
                    FOREGROUND_NOT_ALLOWED,
                    format!("{tool} asked for delivery_mode \"foreground\", and this scope allows background delivery only"),
                ));
            }
        }
        let Some(pid) = pid else {
            // No process named: a desktop-wide action (global keys, a screen
            // point, the desktop pointer) or a screen-wide read.
            let code = if !read_only && self.delivery == ScopeDelivery::Background {
                FOREGROUND_NOT_ALLOWED
            } else {
                OUTSIDE_SCOPE
            };
            return Err(refusal(
                code,
                format!("{tool} names no window, so it would act on or read the whole desktop; in a scoped batch pass the pid and window_id of a window of {}", self.apps_text()),
            ));
        };
        let process = facts.process(pid).await;
        let start_time = process.as_ref().and_then(|facts| facts.start_time);
        let cached = self
            .processes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(pid, start_time))
            .copied();
        let in_scope = match cached {
            Some(verdict) => verdict,
            None => {
                let executable = process.as_ref().and_then(|facts| facts.executable.clone());
                let needs_bundle = self
                    .apps
                    .iter()
                    .any(|app| matches!(app, AppIdentity::Bundle(_)));
                let bundle = if needs_bundle {
                    facts.bundle_id(pid).await
                } else {
                    None
                };
                let verdict = self.matches(bundle.as_deref(), executable.as_deref());
                self.processes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert((pid, start_time), verdict);
                verdict
            }
        };
        if !in_scope {
            return Err(refusal(
                OUTSIDE_SCOPE,
                format!(
                    "pid {pid} is not one of the scoped apps ({})",
                    self.apps_text()
                ),
            ));
        }
        if let Some(window_id) = window_id {
            let key = (pid, start_time, window_id);
            let known = self
                .windows
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&key);
            if !known {
                if !facts.window_of(pid, window_id).await {
                    return Err(refusal(
                        OUTSIDE_SCOPE,
                        format!("window {window_id} is not a window of pid {pid}"),
                    ));
                }
                self.windows
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(key);
            }
        }
        Ok(())
    }

    fn check_launch(&self, args: &Value) -> Result<(), Refusal> {
        let bundle = args.get("bundle_id").and_then(Value::as_str);
        if bundle.is_some_and(|bundle| {
            self.apps
                .iter()
                .any(|app| matches!(app, AppIdentity::Bundle(id) if id == bundle))
        }) {
            return Ok(());
        }
        Err(Refusal {
            code: OUTSIDE_SCOPE,
            message: format!(
                "a scoped batch launches only a scoped app, by its bundle_id ({})",
                self.apps_text()
            ),
            tool: "launch_app".to_owned(),
            pid: None,
            window_id: None,
        })
    }

    fn matches(&self, bundle: Option<&str>, executable: Option<&str>) -> bool {
        self.apps.iter().any(|app| match app {
            AppIdentity::Bundle(id) => {
                crate::session_manifest::bundle_grant_matches(id, bundle, executable)
            }
            AppIdentity::WindowsExecutable(path) => {
                crate::session_manifest::bundle_grant_matches(path, None, executable)
            }
            AppIdentity::PosixExecutable(path) => executable == Some(path.as_str()),
        })
    }

    fn apps_text(&self) -> String {
        self.raw_apps.join(", ")
    }
}

fn parse_identity(text: &str) -> Result<AppIdentity, String> {
    if text.trim() != text || text.is_empty() {
        return Err(format!(
            "`scope.apps` entry {text:?} must be a bundle id or an executable path without surrounding spaces"
        ));
    }
    if crate::session_manifest::is_windows_path_identity(text) {
        return crate::session_manifest::normalize_windows_executable_path(text)
            .map(|_| AppIdentity::WindowsExecutable(text.to_owned()))
            .ok_or_else(|| {
                format!(
                    "`scope.apps` entry {text:?} is not a usable absolute Windows executable path"
                )
            });
    }
    if let Some(rest) = text.strip_prefix('/') {
        let canonical = !rest.is_empty()
            && !rest.ends_with('/')
            && rest
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..");
        return if canonical {
            Ok(AppIdentity::PosixExecutable(text.to_owned()))
        } else {
            Err(format!(
                "`scope.apps` entry {text:?} must be a canonical absolute executable path"
            ))
        };
    }
    if text.contains(['/', '\\']) || text.chars().any(char::is_whitespace) {
        return Err(format!(
            "`scope.apps` entry {text:?} is neither a bundle id nor an absolute executable path"
        ));
    }
    Ok(AppIdentity::Bundle(text.to_owned()))
}

/// A call the scope refused. Nothing was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
    pub tool: String,
    pub pid: Option<i64>,
    pub window_id: Option<u64>,
}

impl Refusal {
    pub fn into_result(self) -> ToolResult {
        let hint = if self.code == FOREGROUND_NOT_ALLOWED {
            "This batch may act only in the background on its scoped apps' windows; drop the foreground step or ask the user for foreground control."
        } else {
            "This batch may act only on windows of its scoped apps; target one of them by pid and window_id."
        };
        let message = format!(
            "{}: refused by the batch scope: {}",
            self.tool, self.message
        );
        ToolResult::error(message.clone()).with_structured(json!({
            "effect": "refused",
            "code": self.code,
            "status": "refused",
            "refusal": {"code": self.code, "message": message},
            "error": {"code": self.code, "hint": hint},
            "hint": hint,
            "tool": self.tool,
            "pid": self.pid,
            "window_id": self.window_id,
        }))
    }
}

/// JSON schema of the `scope` argument, shared by `run_steps` and
/// `run_script`.
pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "description": "Optional limit the driver enforces on every call the batch makes, including the reads it makes to find a target. A call whose pid is not one of `apps`, whose window_id is not a window of that pid, or that names no window is refused with effect \"refused\" and code \"outside_scope\"; with delivery \"background\" (the default), a call that needs the foreground or the whole desktop (bring_to_front, delivery_mode \"foreground\", keys or pointer without a pid) is refused with code \"foreground_not_allowed\". Nothing is sent for a refused call, and the batch stops there like at any other failure. list_apps and list_windows stay available; launch_app is allowed only for a scoped bundle_id.",
        "required": ["apps"],
        "properties": {
            "apps": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_SCOPE_APPS,
                "items": {"type": "string", "minLength": 1},
                "description": "App identities the batch may touch, as in a capability manifest: a bundle or package id (macOS bundle id such as com.apple.calculator, Windows package family name or AUMID), compared exactly with what list_apps reports; or an absolute executable path, compared with the executable the OS reports for the process (POSIX paths case-exactly, Windows paths after Windows normalization)."
            },
            "delivery": {
                "type": "string",
                "enum": ["background", "foreground"],
                "description": "\"background\" (default): only background delivery to the scoped apps' windows. \"foreground\": foreground delivery to those windows is also allowed."
            }
        },
        "additionalProperties": false
    })
}

tokio::task_local! {
    static CURRENT: Arc<BatchScope>;
}

/// Run `future` with `scope` checked on every registry call it dispatches.
/// With `None`, `future` runs unscoped.
pub async fn enter<F: Future>(scope: Option<Arc<BatchScope>>, future: F) -> F::Output {
    match scope {
        Some(scope) => CURRENT.scope(scope, future).await,
        None => future.await,
    }
}

/// The scope of the batch whose call is being dispatched, if any.
pub fn current() -> Option<Arc<BatchScope>> {
    CURRENT.try_with(Arc::clone).ok()
}

#[cfg(test)]
mod tests;
