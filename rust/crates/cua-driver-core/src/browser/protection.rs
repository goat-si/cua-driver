//! Browser processes that hold an origin-scoped attachment are reachable only
//! through the browser tools.
//!
//! Under a capability manifest, a person's attached browser (listed in
//! `existing_profiles`, or holding a live existing-profile grant) and a
//! driver-owned isolated browser are protected processes: desktop tools that
//! target any of their windows, or desktop-scope pointer input whose point
//! lands on one of their windows, are refused with
//! `browser_process_protected` before anything is sent. The browser tools,
//! which enforce the manifest's origins and the call's `origin_scope`, are
//! the only route to those pages. Calls without a capability manifest keep
//! their behaviour.

use std::sync::{Mutex, OnceLock, Weak};

use serde_json::json;

use super::engine::BrowserEngine;
use super::refusal::{BrowserRefusal, BrowserRefusalCode};
use crate::protocol::ToolResult;
use crate::session_manifest::SessionManifest;

static ENGINES: Mutex<Vec<Weak<BrowserEngine>>> = Mutex::new(Vec::new());

pub(crate) fn register_engine(engine: Weak<BrowserEngine>) {
    let mut engines = ENGINES.lock().unwrap();
    engines.retain(|engine| engine.strong_count() > 0);
    engines.push(engine);
}

fn engine_holds(predicate: impl Fn(&BrowserEngine) -> bool) -> bool {
    ENGINES
        .lock()
        .unwrap()
        .iter()
        .filter_map(Weak::upgrade)
        .any(|engine| predicate(&engine))
}

/// Whether `pid` is a protected browser process under `manifest`.
pub fn is_protected_pid(manifest: &SessionManifest, pid: i64) -> bool {
    manifest.lists_existing_profile_pid(pid)
        || engine_holds(|engine| engine.holds_browser_attachment(pid))
}

/// Whether any protected browser process exists under `manifest`.
pub fn any_protected(manifest: &SessionManifest) -> bool {
    manifest.lists_any_existing_profile()
        || engine_holds(BrowserEngine::holds_any_browser_attachment)
}

/// What a platform's hit test found at a desktop point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointOwner {
    /// The window list could not be read.
    Unavailable,
    /// No ordinary window is at the point.
    Nothing,
    /// The topmost ordinary window at the point belongs to this process.
    Process(i64),
}

static DESKTOP_POINT_GUARD: OnceLock<()> = OnceLock::new();

/// A platform declares that its desktop-scope pointer tools call
/// [`desktop_point_refusal`] with every point they deliver to. Without this
/// declaration, desktop-scope input is refused while a protected browser
/// process exists, because its points cannot be checked.
pub fn declare_desktop_point_guard() {
    let _ = DESKTOP_POINT_GUARD.set(());
}

pub(crate) fn platform_guards_desktop_points() -> bool {
    DESKTOP_POINT_GUARD.get().is_some()
}

/// The refusal for desktop-scope pointer input at `points` (screen points),
/// or `None` when it may proceed. `owner_at` is the platform's hit test.
/// Applies only to calls dispatched under a capability manifest.
pub fn desktop_point_refusal(
    tool: &str,
    points: &[(f64, f64)],
    owner_at: impl Fn(f64, f64) -> PointOwner,
) -> Option<ToolResult> {
    let context = crate::tool::current_dispatch_authorization_context()?;
    let manifest = context.capability_manifest()?;
    desktop_point_refusal_for(manifest, tool, points, owner_at)
        .map(|refusal| refusal.to_tool_result())
}

fn desktop_point_refusal_for(
    manifest: &SessionManifest,
    tool: &str,
    points: &[(f64, f64)],
    owner_at: impl Fn(f64, f64) -> PointOwner,
) -> Option<BrowserRefusal> {
    if !any_protected(manifest) {
        return None;
    }
    points.iter().find_map(|&(x, y)| match owner_at(x, y) {
        PointOwner::Process(pid) if is_protected_pid(manifest, pid) => Some(refusal(
            tool,
            format!("({x:.0}, {y:.0}) is on a window of browser process {pid}"),
            Some(pid),
        )),
        PointOwner::Unavailable => Some(refusal(
            tool,
            "the window at this desktop point could not be identified while a protected \
             browser is open"
                .to_owned(),
            None,
        )),
        PointOwner::Process(_) | PointOwner::Nothing => None,
    })
}

/// The refusal for a desktop tool aimed at `pid` (all of its windows).
pub(crate) fn process_refusal(
    manifest: &SessionManifest,
    tool: &str,
    pid: i64,
) -> Option<BrowserRefusal> {
    is_protected_pid(manifest, pid).then(|| {
        refusal(
            tool,
            format!("browser process {pid} holds an origin-scoped browser attachment"),
            Some(pid),
        )
    })
}

/// The refusal for desktop-scope keyboard input, which goes to the
/// frontmost application (`None` when it cannot be identified). Applies only
/// to calls dispatched under a capability manifest.
pub fn frontmost_app_refusal(tool: &str, frontmost_pid: Option<i64>) -> Option<ToolResult> {
    let context = crate::tool::current_dispatch_authorization_context()?;
    let manifest = context.capability_manifest()?;
    if !any_protected(manifest) {
        return None;
    }
    match frontmost_pid {
        Some(pid) => process_refusal(manifest, tool, pid),
        None => Some(refusal(
            tool,
            "the frontmost application could not be identified while a protected browser \
             is open"
                .to_owned(),
            None,
        )),
    }
    .map(|refusal| refusal.to_tool_result())
}

/// The refusal for desktop-scope input on a platform that cannot check
/// where its points land, while a protected browser process exists.
pub(crate) fn unguarded_desktop_refusal(
    manifest: &SessionManifest,
    tool: &str,
) -> Option<BrowserRefusal> {
    (!platform_guards_desktop_points() && any_protected(manifest)).then(|| {
        refusal(
            tool,
            "this platform cannot tell which window desktop-scope input lands on while a \
             protected browser is open"
                .to_owned(),
            None,
        )
    })
}

/// The refusal for a whole-desktop observation while a protected browser
/// process exists: its windows would be part of what is observed.
pub(crate) fn display_observation_refusal(
    manifest: &SessionManifest,
    tool: &str,
) -> Option<BrowserRefusal> {
    any_protected(manifest).then(|| {
        refusal(
            tool,
            "a whole-desktop observation would include a protected browser's windows; \
             observe another application by pid and window_id instead"
                .to_owned(),
            None,
        )
    })
}

fn refusal(tool: &str, reason: String, pid: Option<i64>) -> BrowserRefusal {
    BrowserRefusal::new(
        BrowserRefusalCode::BrowserProcessProtected,
        format!(
            "{tool} was refused: {reason}. That browser is reachable only through the browser \
             tools (get_browser_state, browser_click, browser_type, browser_pointer, \
             browser_navigate) with origin_scope. Nothing was sent."
        ),
    )
    .with_detail(json!({
        "pid": pid,
        "route": ["get_browser_state", "browser_click", "browser_type", "browser_pointer", "browser_navigate"],
    }))
}

#[cfg(test)]
#[path = "protection_tests.rs"]
mod tests;
