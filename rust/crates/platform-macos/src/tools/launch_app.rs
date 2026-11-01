use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::path::PathBuf;

pub struct LaunchAppTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "launch_app".into(),
        description:
            "Launch a macOS app in the background; it does not come to the foreground. Give `bundle_id` \
                (preferred, e.g. `com.apple.calculator`) or `name` (\"Calculator\", or the absolute `.app` path \
                from `list_apps` `launch_path`); bundle_id wins. `urls` are opened by the app (a folder path \
                opens a backgrounded Finder window there). `webkit_inspector_port` opens a WebKit inspector \
                server for Tauri/WebKit apps. `creates_new_application_instance:true` forces a new instance \
                (open -n), so concurrent sessions each get their own window. `additional_arguments` go after \
                --args. Browser DevTools setup belongs to `browser_prepare`.\n\
                \n\
                Returns pid, bundle_id, name, a `windows` array (as `list_windows`) and `launch_state` (request \
                sent, process running, window ready). When the launched app was not already frontmost, \
                `self_activation_suppressed` says whether focus stayed with the previous app."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "bundle_id": {
                    "type": "string",
                    "description": "App bundle identifier, e.g. com.apple.calculator. Preferred over name."
                },
                "name": {
                    "type": "string",
                    "description": "App display name, or absolute path to a .app bundle (e.g. launch_path from list_apps). Used only when bundle_id is absent."
                },
                "urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional file paths or URLs to open with the app (e.g. a folder path for Finder)."
                },
                "webkit_inspector_port": {
                    "type": "integer",
                    "description": "Open a WebKit inspector server on this port (sets WEBKIT_INSPECTOR_SERVER env var)."
                },
                "creates_new_application_instance": {
                    "type": "boolean",
                    "description": "When true, force a new app instance even if already running (open -n). Use for concurrent multi-agent/multi-session work so each session gets an isolated instance + window instead of sharing one — on single-instance apps (e.g. Calculator) every caller otherwise gets the same window and the sessions clobber each other."
                },
                "additional_arguments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Extra arguments appended after --args when launching."
                }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: true,
    })
}

#[async_trait]
impl Tool for LaunchAppTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        crate::window_change_detector::end_tail();
        // Let a background key hand-back finish before the app is fronted.
        if !tokio::task::spawn_blocking(crate::input::skylight::await_key_restores)
            .await
            .unwrap_or(false)
        {
            return super::key_restore_pending_refusal("launch_app", None, None);
        }
        crate::window_change_detector::note_intentional_activation();
        let bundle_id = args.opt_str("bundle_id");
        let name = args.opt_str("name");
        let mut response_bundle_id = bundle_id.clone();
        let response_requested_name = name.clone();
        let urls: Vec<String> = args
            .str_array("urls")
            .into_iter()
            .map(normalize_launch_url)
            .collect();
        if args.get("cdp_debugging_port").is_some() {
            return ToolResult::error(
                "cdp_debugging_port moved to browser_prepare so DevTools is never enabled on an unproven user profile",
            );
        }
        let webkit_inspector_port = args.opt_u64("webkit_inspector_port").map(|v| v as u16);
        let creates_new_instance = args.bool_or("creates_new_application_instance", false);
        let additional_arguments: Vec<String> = args.str_array("additional_arguments");
        if additional_arguments
            .iter()
            .any(|argument| argument == super::check_permissions::PERMISSIONS_HOST_REQUEST_ARG)
        {
            return protected_host_launch_refusal();
        }
        if additional_arguments
            .iter()
            .any(|argument| cua_driver_core::launch_guard::contains_remote_debugging_flag(argument))
        {
            return ToolResult::error(
                cua_driver_core::launch_guard::REMOTE_DEBUGGING_LAUNCH_REFUSAL,
            );
        }

        if bundle_id.is_none() && name.is_none() {
            return ToolResult::error(
                "Provide either bundle_id or name to identify the app to launch.",
            );
        }
        if bundle_id.as_deref().is_some_and(is_cua_driver_bundle_id) {
            return protected_host_launch_refusal();
        }
        if let Some(ref bid) = bundle_id {
            if crate::apps::resolve_bundle_id_to_locator(bid).is_none() {
                return structured_launch_error(
                    "APP_NOT_INSTALLED",
                    format!("No installed macOS app found for bundle_id '{bid}'."),
                    serde_json::json!({ "bundle_id": bid }),
                );
            }
        } else if let Some(ref n) = name {
            let Some(locator) = crate::apps::locate_by_name(n) else {
                return structured_launch_error(
                    "APP_NOT_INSTALLED",
                    format!("No installed macOS app found for name '{n}'."),
                    serde_json::json!({ "name": n }),
                );
            };
            let (_, resolved_bundle_id) = locator.app_ref_and_bundle_id();
            response_bundle_id = resolved_bundle_id.clone();
            if resolved_bundle_id
                .as_deref()
                .is_some_and(is_cua_driver_bundle_id)
            {
                return protected_host_launch_refusal();
            }
        }
        if let Some(err) = preflight_file_urls(&urls) {
            return err;
        }

        // Build env dict for webkit inspector.
        let mut env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if let Some(port) = webkit_inspector_port {
            env.insert(
                "WEBKIT_INSPECTOR_SERVER".to_string(),
                format!("127.0.0.1:{port}"),
            );
            env.insert("TAURI_WEBVIEW_AUTOMATION".to_string(), "1".to_string());
        }

        let port_summary = {
            let mut s = String::new();
            if let Some(port) = webkit_inspector_port {
                s.push_str(&format!("\nWebKit inspector available on port {port}."));
            }
            s
        };

        // ── Layer-3 focus-steal suppression (3-phase wrap) ───────────────
        //
        // Captures the prior frontmost pid, arms a wildcard suppression
        // BEFORE the launch (covers self-activations the target fires
        // synchronously during `open()`), then upgrades to a targeted
        // suppression keyed to the actual launched pid. Briefly holds
        // BOTH leases so a self-activation arriving in the wildcard→
        // targeted gap is still caught — that race is what hoang17's
        // Swift PR #1521 explicitly fixes; we do not regress it here.
        //
        // After 500ms (enough for `applicationDidFinishLaunching` +
        // any reflex `NSApp.activate(...)` to fire and get suppressed)
        // both leases are dropped. The belt-and-braces step at the end
        // re-activates the prior frontmost if the target is still
        // frontmost — handles the intra-`open()` synchronous activation
        // that fired before we could arm with the real pid.
        let prior_frontmost = crate::apps::frontmost_pid();
        let wildcard_lease = prior_frontmost.map(|prior| {
            crate::focus_steal::FocusStealPreventer::begin_suppression(
                None,
                prior,
                "LaunchAppTool.pre",
            )
        });

        // Predicate captured BEFORE moving inputs into spawn_blocking.
        // Same condition that selects the `openURLs:withApplicationAtURL:`
        // chain over the simpler `openApplicationAtURL:` path. Used after
        // the spawn returns to size the suppression window — the slow
        // path triggers a SECOND activation when the file-open delivers,
        // which lands AFTER the bundle-only-launch activation window.
        let slow_launch_path = !urls.is_empty()
            || !additional_arguments.is_empty()
            || !env.is_empty()
            || creates_new_instance;

        // Move the launch closure inputs into spawn_blocking. The
        // blocking task returns (pid, app_info, windows). Suppression
        // upgrade happens AFTER the blocking call returns (back on the
        // async runtime), then we sleep holding the targeted lease.
        let launch_result = tokio::task::spawn_blocking(move || {
            // Finder's windows before a folder launch: only a window the
            // launch adds can show the requested folder. Folder launches run
            // one at a time from snapshot to match, so two launches never
            // claim the same new window.
            let finder_folders =
                !urls.is_empty() && is_finder_request(bundle_id.as_deref(), name.as_deref());
            let _finder_turn = finder_folders.then(|| {
                FINDER_FOLDER_LAUNCH
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
            });
            let finder_before: std::collections::HashSet<u32> = if !finder_folders {
                Default::default()
            } else {
                crate::apps::list_running_apps()
                    .into_iter()
                    .find(|app| app.bundle_id.as_deref() == Some("com.apple.finder"))
                    .map(|finder| {
                        finder_windows(finder.pid)
                            .into_iter()
                            .map(|w| w.window_id)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let pid = if let Some(ref bid) = bundle_id {
                if urls.is_empty()
                    && additional_arguments.is_empty()
                    && env.is_empty()
                    && !creates_new_instance
                {
                    crate::apps::launch_app(bid)?
                } else {
                    crate::apps::launch_with_urls_by_bundle(
                        bid,
                        &urls,
                        &additional_arguments,
                        &env,
                        creates_new_instance,
                    )?
                }
            } else {
                let n = name.as_deref().unwrap();
                if urls.is_empty()
                    && additional_arguments.is_empty()
                    && env.is_empty()
                    && !creates_new_instance
                {
                    crate::apps::launch_app_by_name(n)?
                } else {
                    crate::apps::launch_with_urls_by_name(
                        n,
                        &urls,
                        &additional_arguments,
                        &env,
                        creates_new_instance,
                    )?
                }
            };

            let app_info: Option<crate::apps::AppInfo> = {
                let apps = crate::apps::list_running_apps();
                apps.into_iter().find(|a| a.pid == pid)
            };

            // Finder is always running, so its existing windows say nothing
            // about the folders asked for: wait for each folder's own window.
            // Otherwise poll up to 5x100ms: LaunchServices returns before
            // WindowServer has registered the new windows.
            let folders = finder_folder_targets(
                app_info.as_ref().and_then(|app| app.bundle_id.as_deref()),
                &urls,
            );
            let (windows, folders_ready) = if folders.is_empty() {
                (resolve_windows_for_pid(pid), None)
            } else {
                let (found, ready) = await_folder_windows(pid, &folders, &finder_before);
                (found, Some(ready))
            };

            Ok::<_, anyhow::Error>((pid, app_info, windows, folders_ready))
        })
        .await;

        // Upgrade to targeted suppression now that we know the real pid.
        // Keep the wildcard lease alive until immediately AFTER we've
        // armed the targeted one — that's the PR #1521 overlap window.
        //
        // `self_activation_suppressed` is the outcome of the belt-and-
        // braces demotion check: `None` when the check didn't run
        // (no prior frontmost / launch failed / pid == prior), `Some(true)`
        // when the target was NOT frontmost after the suppression window
        // (or we successfully re-demoted it), `Some(false)` when the
        // re-demote failed and the target is still stealing focus.
        // Surfaced in the structured response so callers can observe
        // whether focus-steal prevention actually held.
        let mut self_activation_suppressed: Option<bool> = None;
        if let Ok(Ok((pid, _, _, _))) = &launch_result {
            if let Some(prior) = prior_frontmost {
                if *pid != prior {
                    let targeted_lease = crate::focus_steal::FocusStealPreventer::begin_suppression(
                        Some(*pid),
                        prior,
                        "LaunchAppTool.post",
                    );
                    // Now safe to drop the wildcard — targeted is armed.
                    drop(wildcard_lease);
                    // Hold the targeted lease long enough to cover the
                    // ENTIRE post-launch activation window.
                    //
                    // - Fast path (bundle-only launch, no urls/args/env):
                    //   500ms covers `applicationDidFinishLaunching` plus
                    //   any reflex `NSApp.activate(...)`. Matches Swift
                    //   LaunchAppTool.swift exactly.
                    //
                    // - Slow path (urls / additional_arguments / env /
                    //   creates_new_instance): 2500ms. The slow-path
                    //   `openURLs:withApplicationAtURL:` chain triggers a
                    //   second activation when the file-open delivers to
                    //   the just-launched app — Electron apps (VSCode,
                    //   Cursor, Slack) re-`app.focus()` from inside their
                    //   `open-file` JS handler, AFTER our 500ms window
                    //   would have already closed. Empirically VSCode's
                    //   late activation can land anywhere from ~700ms to
                    //   ~2000ms after the openURLs return. The observer-
                    //   based lease catches any activation that lands
                    //   WHILE held, so widening the window converts the
                    //   late activation from a contract violation into
                    //   another auto-demote.
                    let window_ms: u64 = if slow_launch_path { 2500 } else { 500 };
                    tokio::time::sleep(std::time::Duration::from_millis(window_ms)).await;
                    drop(targeted_lease);

                    // Belt-and-braces LOOP: if the target ever pops back
                    // to the foreground after the lease drops (rare —
                    // observer already covered the suppression window —
                    // but happens when the activation fires literally on
                    // the same tokio tick the lease dropped), demote it.
                    // Loop 5x200ms = 1s of post-window coverage. Each
                    // iteration is cheap (one frontmost_pid + maybe one
                    // activate_pid call) so this stays well under the
                    // RPC budget even when the demote keeps working.
                    let mut demotion_succeeded = true;
                    for _ in 0..5 {
                        let frontmost_now = crate::apps::frontmost_pid();
                        if frontmost_now != Some(*pid) {
                            // Not frontmost — nothing to do this tick.
                            continue;
                        }
                        let activated = crate::apps::activate_pid(prior);
                        let still_frontmost = crate::apps::frontmost_pid() == Some(*pid);
                        if still_frontmost {
                            tracing::warn!(
                                target: "platform_macos::tools::launch_app",
                                launched_pid = *pid,
                                prior_pid = prior,
                                activate_pid_returned = activated,
                                "belt-and-braces demotion iteration failed: \
                                 launched app remained frontmost after \
                                 re-activating prior — will retry"
                            );
                            demotion_succeeded = false;
                        } else {
                            demotion_succeeded = true;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    // Final in-call state determines the structured response.
                    let final_frontmost = crate::apps::frontmost_pid();
                    self_activation_suppressed =
                        Some(final_frontmost != Some(*pid) && demotion_succeeded);

                    // Detached late-activation watchdog (slow path only).
                    //
                    // Why: Electron apps with no workspace open (cold-
                    // launched VSCode / Cursor / Slack with a file URL)
                    // re-activate AGAIN when their Welcome window
                    // finishes loading — empirically 4-8 seconds after
                    // the `openURLs:withApplicationAtURL:` call returns.
                    // That's well past the in-call suppression window
                    // and any reasonable extension of it that an agent
                    // workflow would tolerate as caller latency.
                    //
                    // Solution: hold a fresh observer-backed lease in
                    // the background for ~8s, demoting if the launched
                    // pid pops back. The caller doesn't wait — the tool
                    // already returned its honest `self_activation_
                    // suppressed` for the in-call window. The detached
                    // task just keeps the no-foreground-steal contract
                    // honored past the RPC boundary.
                    //
                    // Note on process lifecycle: this watchdog only runs
                    // when the tokio runtime stays alive — i.e. in the
                    // long-running `cua-driver mcp` / `cua-driver serve`
                    // daemon modes. The one-shot `cua-driver call` mode
                    // exits as soon as the tool returns, taking the
                    // detached task with it. Acceptable because the
                    // contract is "no foreground steal during a session
                    // the agent is driving" — `cua-driver call` doesn't
                    // have a session that outlives the call.
                    //
                    // Tradeoffs:
                    // - Caller latency unchanged (~2.5s for slow path).
                    // - Total observer coverage: ~10.5s post-launch.
                    // - CPU: the observer fires per activation event,
                    //   not per poll; the 250ms tick is just for the
                    //   manual belt-and-braces demote. Cheap.
                    // - If a legitimate user click activates Code while
                    //   the watchdog is alive, we'll demote them. Worst
                    //   case ~10s of "I clicked Code and it didn't come
                    //   forward" — acceptable trade for an automation
                    //   scenario where the agent just launched it.
                    if slow_launch_path {
                        let launched_pid = *pid;
                        let prior_pid = prior;
                        tokio::spawn(async move {
                            let _lease = crate::focus_steal::FocusStealPreventer::begin_suppression(
                                Some(launched_pid),
                                prior_pid,
                                "LaunchAppTool.watchdog",
                            );
                            let mut late_activations = 0u32;
                            for _ in 0..32 {
                                // 32 × 250ms = 8s
                                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                                if crate::apps::frontmost_pid() == Some(launched_pid) {
                                    late_activations += 1;
                                    let _ = crate::apps::activate_pid(prior_pid);
                                }
                            }
                            if late_activations > 0 {
                                tracing::warn!(
                                    target: "platform_macos::tools::launch_app",
                                    launched_pid,
                                    prior_pid,
                                    late_activations,
                                    "watchdog demoted post-RPC late activations \
                                     — slow-path window may need tuning"
                                );
                            }
                        });
                    }
                } else {
                    // pid == prior frontmost (re-launch of an already-
                    // frontmost app). Just drop the wildcard.
                    drop(wildcard_lease);
                }
            }
        } else {
            // Launch failed; just drop the lease.
            drop(wildcard_lease);
        }

        match launch_result {
            Ok(Ok((pid, app_info, windows, folders_ready))) => {
                let (app_name, bid) = response_identity(
                    app_info.as_ref(),
                    response_bundle_id.as_deref(),
                    response_requested_name.as_deref(),
                );

                let mut summary =
                    format!("Launched {app_name} (pid {pid}) in background.{port_summary}");

                if !windows.is_empty() {
                    summary.push_str("\n\nWindows:");
                    for w in &windows {
                        let title = if w.title.is_empty() {
                            "(no title)".to_owned()
                        } else {
                            format!("\"{}\"", w.title)
                        };
                        summary.push_str(&format!("\n- {title} [window_id: {}]", w.window_id));
                    }
                    summary.push_str(&format!(
                        "\n→ Call get_window_state(pid: {pid}, window_id) to inspect."
                    ));
                }

                if folders_ready == Some(false) {
                    summary.push_str(
                        "\n\nFinder took the request, but not every folder got a new window \
                         within 3 s. A folder that was already open keeps its existing window, \
                         and the root folder's window carries its volume's name; Finder does not \
                         identify a window by folder, so call list_windows and check the window \
                         before acting on it.",
                    );
                }

                let windows_json: Vec<Value> = windows
                    .iter()
                    .map(super::list_windows::window_record_json)
                    .collect();

                let mut structured = serde_json::json!({
                    "pid": pid,
                    "bundle_id": bid,
                    "name": app_name,
                    "windows": windows_json,
                    "launch_state": launch_state(
                        true,
                        true,
                        folders_ready.unwrap_or(!windows.is_empty()),
                    ),
                });
                // Only emit `self_activation_suppressed` when the
                // belt-and-braces demotion check actually ran. `None`
                // means the launch didn't enter the focus-steal path
                // (no prior frontmost, or pid == prior) — surfacing
                // a stale `false` would be misleading.
                if let Some(suppressed) = self_activation_suppressed {
                    structured["self_activation_suppressed"] = serde_json::Value::Bool(suppressed);
                }
                ToolResult::text(summary).with_structured(structured)
            }
            Ok(Err(e)) => structured_launch_failure(&e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

fn is_cua_driver_bundle_id(bundle_id: &str) -> bool {
    matches!(bundle_id, "com.trycua.driver" | "com.trycua.driver.local")
}

fn protected_host_launch_refusal() -> ToolResult {
    structured_launch_error(
        "PROTECTED_HOST_ENTRYPOINT",
        "launch_app cannot launch Cua Driver's protected host; operating-system permission UI must originate outside the agent tool stream".to_owned(),
        serde_json::json!({}),
    )
}

// ── Blocking helpers ──────────────────────────────────────────────────────────

fn resolve_windows_for_pid(pid: i32) -> Vec<crate::windows::WindowInfo> {
    for attempt in 0..5 {
        let mut found = crate::windows::pid_windows_with_space_snapshot(pid, false).windows;
        found.retain(|w| w.bounds.width > 1.0 && w.bounds.height > 1.0);
        crate::windows::prefer_layer0(&mut found);
        if !found.is_empty() {
            rank_launch_windows(&mut found);
            return found;
        }
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    vec![]
}

/// A folder a Finder launch asked for, by the title Finder gives its window;
/// `None` when that title cannot be known from the path (the root folder
/// shows its volume's name), so no window can be confirmed as its own.
struct FolderTarget {
    title: Option<String>,
}

/// The folders in `urls`, when every one is a local folder opened with
/// Finder; empty otherwise.
fn finder_folder_targets(bundle_id: Option<&str>, urls: &[String]) -> Vec<FolderTarget> {
    if bundle_id != Some("com.apple.finder") || urls.is_empty() {
        return Vec::new();
    }
    let mut targets = Vec::new();
    for url in urls {
        let path = std::path::Path::new(url);
        if !path.is_dir() {
            return Vec::new();
        }
        targets.push(FolderTarget {
            title: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
        });
    }
    targets
}

/// The windows that show the requested folders, best first, and whether
/// every folder has one. Finder reports no folder for its windows (its
/// `AXDocument` is empty), so a window counts only when the launch opened it:
/// absent from `before` and titled with the folder's name. Each window stands
/// for one folder. A folder that was already open in a window is reported as
/// not ready rather than guessed from a window of the same name.
///
/// `unresolved` names folders of earlier launches that timed out before
/// their window appeared: such a window may still arrive, so a new window of
/// that name proves nothing for this launch.
fn match_folder_windows(
    targets: &[FolderTarget],
    windows: Vec<crate::windows::WindowInfo>,
    before: &std::collections::HashSet<u32>,
    unresolved: &[String],
) -> (Vec<crate::windows::WindowInfo>, bool) {
    let mut fresh: Vec<_> = windows
        .into_iter()
        .filter(|w| !before.contains(&w.window_id))
        .collect();
    let mut found = Vec::new();
    let mut ready = true;
    for target in targets {
        let title = target
            .title
            .as_deref()
            .filter(|title| !unresolved.iter().any(|u| u == title));
        match fresh.iter().position(|w| title == Some(w.title.as_str())) {
            Some(at) => found.push(fresh.remove(at)),
            None => ready = false,
        }
    }
    rank_launch_windows(&mut found);
    (found, ready)
}

/// Held by a Finder launch with `urls` from Finder's window snapshot until
/// its folder windows are matched.
static FINDER_FOLDER_LAUNCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Folder names whose launch timed out before a window appeared, and when.
static UNRESOLVED_FOLDERS: std::sync::Mutex<Vec<(String, std::time::Instant)>> =
    std::sync::Mutex::new(Vec::new());

/// How long a timed-out folder name keeps later matches unconfirmed.
const UNRESOLVED_FOLDER_TTL: std::time::Duration = std::time::Duration::from_secs(30);

fn unresolved_folders() -> Vec<String> {
    let mut held = UNRESOLVED_FOLDERS.lock().unwrap_or_else(|e| e.into_inner());
    held.retain(|(_, at)| at.elapsed() < UNRESOLVED_FOLDER_TTL);
    held.iter().map(|(title, _)| title.clone()).collect()
}

/// Most timed-out folder names remembered at once; the oldest go first.
const UNRESOLVED_FOLDER_CAP: usize = 64;

/// The titles of `targets` that no window in `found` stands for, each found
/// window standing for one target.
fn missing_folder_titles(
    targets: &[FolderTarget],
    found: &[crate::windows::WindowInfo],
) -> Vec<String> {
    let mut left: Vec<&str> = found.iter().map(|w| w.title.as_str()).collect();
    let mut missing = Vec::new();
    for title in targets.iter().filter_map(|t| t.title.as_deref()) {
        match left.iter().position(|w| *w == title) {
            Some(at) => {
                left.remove(at);
            }
            None => missing.push(title.to_owned()),
        }
    }
    missing
}

/// Add `titles` to `held`, once per name with the newest time, keeping at
/// most [`UNRESOLVED_FOLDER_CAP`] names.
fn remember_unresolved(
    held: &mut Vec<(String, std::time::Instant)>,
    titles: &[String],
    now: std::time::Instant,
) {
    for title in titles {
        held.retain(|(held_title, _)| held_title != title);
        held.push((title.clone(), now));
    }
    let excess = held.len().saturating_sub(UNRESOLVED_FOLDER_CAP);
    held.drain(..excess);
}

fn note_unresolved_folders(targets: &[FolderTarget], found: &[crate::windows::WindowInfo]) {
    let missing = missing_folder_titles(targets, found);
    let mut held = UNRESOLVED_FOLDERS.lock().unwrap_or_else(|e| e.into_inner());
    remember_unresolved(&mut held, &missing, std::time::Instant::now());
}

/// Whether the launch asks for Finder: by bundle id, or by a name that
/// resolves to Finder (its display name, app path or bundle id).
fn is_finder_request(bundle_id: Option<&str>, name: Option<&str>) -> bool {
    const FINDER: &str = "com.apple.finder";
    match (bundle_id, name) {
        (Some(bundle_id), _) => bundle_id == FINDER,
        (None, Some(name)) => {
            name.eq_ignore_ascii_case("finder")
                || name == FINDER
                || crate::apps::locate_by_name(name)
                    .and_then(|app| app.app_ref_and_bundle_id().1)
                    .as_deref()
                    == Some(FINDER)
        }
        (None, None) => false,
    }
}

/// How long the Finder folder windows get to appear.
const FOLDER_WINDOW_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Finder's on-screen document windows, by id.
fn finder_windows(pid: i32) -> Vec<crate::windows::WindowInfo> {
    crate::windows::pid_windows_with_space_snapshot(pid, false)
        .windows
        .into_iter()
        .filter(|w| w.layer == 0 && w.bounds.width > 1.0 && w.bounds.height > 1.0)
        .collect()
}

/// Wait for each folder's new window; returns the windows found and whether
/// every folder got one.
fn await_folder_windows(
    pid: i32,
    targets: &[FolderTarget],
    before: &std::collections::HashSet<u32>,
) -> (Vec<crate::windows::WindowInfo>, bool) {
    let deadline = std::time::Instant::now() + FOLDER_WINDOW_WAIT;
    let unresolved = unresolved_folders();
    loop {
        let (found, ready) =
            match_folder_windows(targets, finder_windows(pid), before, &unresolved);
        if ready {
            return (found, ready);
        }
        if std::time::Instant::now() >= deadline {
            note_unresolved_folders(targets, &found);
            return (found, ready);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn rank_launch_windows(windows: &mut [crate::windows::WindowInfo]) {
    windows.sort_by_key(|window| std::cmp::Reverse(!window.title.trim().is_empty()));
}

fn structured_launch_error(code: &str, message: String, details: serde_json::Value) -> ToolResult {
    let mut payload = serde_json::json!({
        "error": code,
    });

    match details {
        serde_json::Value::Object(details) => {
            if let serde_json::Value::Object(payload) = &mut payload {
                payload.extend(details);
            }
        }
        details => {
            if let serde_json::Value::Object(payload) = &mut payload {
                payload.insert("details".to_string(), details);
            }
        }
    }

    ToolResult::error(message).with_structured(payload)
}

fn launch_state(requested: bool, process_running: bool, window_ready: bool) -> serde_json::Value {
    serde_json::json!({
        "requested": requested,
        "process_running": process_running,
        "window_ready": window_ready,
    })
}

fn response_identity(
    app_info: Option<&crate::apps::AppInfo>,
    requested_bundle_id: Option<&str>,
    requested_name: Option<&str>,
) -> (String, String) {
    let bundle_id = app_info
        .and_then(|app| app.bundle_id.as_deref())
        .filter(|value| !value.is_empty())
        .or(requested_bundle_id)
        .unwrap_or("?")
        .to_owned();

    let name = app_info
        .map(|app| app.name.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| requested_app_name(requested_name, requested_bundle_id));

    (name, bundle_id)
}

fn requested_app_name(requested_name: Option<&str>, requested_bundle_id: Option<&str>) -> String {
    if let Some(name) = requested_name.filter(|name| Some(*name) != requested_bundle_id) {
        let file_name = std::path::Path::new(name)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(name);
        return file_name
            .strip_suffix(".app")
            .unwrap_or(file_name)
            .to_owned();
    }

    requested_bundle_id
        .and_then(|bundle_id| bundle_id.rsplit('.').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("?")
        .to_owned()
}

fn structured_launch_failure(error: &anyhow::Error) -> ToolResult {
    use crate::apps::nsworkspace::LaunchError;

    let (code, requested) = if let Some(launch_error) = error.downcast_ref::<LaunchError>() {
        match launch_error {
            LaunchError::Cocoa(_) => ("NSWORKSPACE_LAUNCH_FAILED", true),
            LaunchError::NoApp => ("LAUNCH_RESULT_MISSING", true),
            LaunchError::Timeout => ("LAUNCH_CALLBACK_TIMEOUT", true),
            LaunchError::BadUrl(_) => ("APP_URL_INVALID", false),
        }
    } else {
        ("LAUNCH_FAILED", false)
    };

    structured_launch_error(
        code,
        format!("Launch failed: {error:#}"),
        serde_json::json!({
            "launch_state": launch_state(requested, false, false),
        }),
    )
}

fn preflight_file_urls(urls: &[String]) -> Option<ToolResult> {
    for raw in urls {
        let Some(path) = local_file_target(raw) else {
            continue;
        };
        if !path.exists() {
            return Some(structured_launch_error(
                "FILE_NOT_FOUND",
                format!(
                    "Local launch_app url target does not exist: {}",
                    path.display()
                ),
                serde_json::json!({
                    "url": raw,
                    "path": path.display().to_string(),
                }),
            ));
        }
    }
    None
}

fn normalize_launch_url(raw: String) -> String {
    local_file_target(&raw)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(raw)
}

fn local_file_target(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() {
        return Some(PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("file://") {
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        let decoded = percent_decode_path(path);
        return Some(expand_tilde(&decoded));
    }
    let looks_like_url = raw.contains(':') && !raw.starts_with('/') && !raw.starts_with('~');
    if looks_like_url {
        return None;
    }
    Some(expand_tilde(raw))
}

fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn percent_decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                decoded.push((high << 4) | low);
                i += 3;
                continue;
            }
        }

        decoded.push(bytes[i]);
        i += 1;
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        finder_folder_targets, is_cua_driver_bundle_id, is_finder_request, local_file_target,
        match_folder_windows, missing_folder_titles, normalize_launch_url, preflight_file_urls,
        rank_launch_windows, remember_unresolved, response_identity, structured_launch_failure,
        LaunchAppTool, UNRESOLVED_FOLDER_CAP,
    };
    use cua_driver_core::tool::Tool;
    use serde_json::json;
    use std::path::PathBuf;

    fn window(window_id: u32, title: &str, width: f64, height: f64) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id,
            pid: 42,
            app_name: "Calculator".to_owned(),
            title: title.to_owned(),
            bounds: crate::windows::WindowBounds {
                x: 0.0,
                y: 0.0,
                width,
                height,
            },
            layer: 0,
            z_index: window_id as usize,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    #[test]
    fn a_finder_folder_launch_is_judged_by_the_folders_own_window() {
        let root = std::env::temp_dir().join("cua-launch-folder-test");
        let (a, b) = (root.join("A/reports"), root.join("B/reports"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (a, b) = (a.canonicalize().unwrap(), b.canonicalize().unwrap());
        let url = |p: &std::path::Path| p.to_string_lossy().into_owned();

        let targets = finder_folder_targets(Some("com.apple.finder"), &[url(&b)]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].title.as_deref(), Some("reports"));
        // Finder asked for by name is the same request; other apps never wait
        // on Finder's launch turn.
        assert!(is_finder_request(Some("com.apple.finder"), None));
        assert!(is_finder_request(None, Some("Finder")));
        // Finder by its app path or its bundle id given as the name.
        assert!(is_finder_request(
            None,
            Some("/System/Library/CoreServices/Finder.app")
        ));
        assert!(is_finder_request(None, Some("com.apple.finder")));
        assert!(!is_finder_request(Some("com.apple.Safari"), None));
        assert!(!is_finder_request(None, Some("TextEdit")));
        // Not Finder, or not a folder: the app's windows are reported as before.
        assert!(finder_folder_targets(Some("com.apple.TextEdit"), &[url(&b)]).is_empty());
        assert!(finder_folder_targets(
            Some("com.apple.finder"),
            &["https://example.com".to_owned()]
        )
        .is_empty());

        let ids = |found: &[crate::windows::WindowInfo]| {
            found.iter().map(|w| w.window_id).collect::<Vec<_>>()
        };
        // Finder reports no folder for its windows, so only a window the
        // launch opened (absent before it) proves the folder is shown. The
        // person's "Recents", or an "A/reports" window that was already open,
        // does not.
        let before: std::collections::HashSet<u32> = [1, 2].into();
        let (found, ready) = match_folder_windows(
            &targets,
            vec![
                window(1, "Recents", 900.0, 500.0),
                window(2, "reports", 900.0, 500.0),
            ],
            &before,
            &[],
        );
        assert!(ids(&found).is_empty());
        assert!(!ready);
        let (found, ready) = match_folder_windows(
            &targets,
            vec![
                window(2, "reports", 900.0, 500.0),
                window(3, "reports", 900.0, 500.0),
            ],
            &before,
            &[],
        );
        assert_eq!(ids(&found), vec![3]);
        assert!(ready);
        // An earlier launch of another "reports" timed out: its window may
        // still appear, so a new "reports" window proves nothing now.
        let (found, ready) = match_folder_windows(
            &targets,
            vec![window(3, "reports", 900.0, 500.0)],
            &before,
            &["reports".to_owned()],
        );
        assert!(ids(&found).is_empty());
        assert!(!ready);

        // The root folder has no name to find its window by: a Finder
        // request all the same, never ready from some other window.
        let root = finder_folder_targets(Some("com.apple.finder"), &["/".to_owned()]);
        assert_eq!(root.len(), 1);
        let (found, ready) = match_folder_windows(
            &root,
            vec![window(5, "Macintosh HD", 900.0, 500.0)],
            &before,
            &[],
        );
        assert!(ids(&found).is_empty());
        assert!(!ready);

        // Several folders: one new window per folder, and ready only when
        // every folder has one. One window never stands for two folders.
        let both = finder_folder_targets(Some("com.apple.finder"), &[url(&a), url(&b)]);
        let (found, ready) = match_folder_windows(
            &both,
            vec![window(3, "reports", 900.0, 500.0)],
            &before,
            &[],
        );
        assert_eq!(ids(&found), vec![3]);
        assert!(!ready);
        let (found, ready) = match_folder_windows(
            &both,
            vec![
                window(3, "reports", 900.0, 500.0),
                window(4, "reports", 900.0, 500.0),
            ],
            &before,
            &[],
        );
        assert_eq!(ids(&found), vec![3, 4]);
        assert!(ready);
    }

    #[test]
    fn a_timed_out_folder_is_remembered_once_per_missing_window() {
        let dir = std::env::temp_dir().join("cua-launch-unresolved-test");
        let (a, b) = (dir.join("A/reports"), dir.join("B/reports"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let url = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let both = finder_folder_targets(Some("com.apple.finder"), &[url(&a), url(&b)]);
        // One "reports" window appeared for two "reports" folders: the
        // other is still on its way.
        assert_eq!(
            missing_folder_titles(&both, &[window(3, "reports", 900.0, 500.0)]),
            vec!["reports".to_owned()]
        );
        assert!(missing_folder_titles(
            &both,
            &[
                window(3, "reports", 900.0, 500.0),
                window(4, "reports", 900.0, 500.0)
            ]
        )
        .is_empty());

        // Remembered once per name, newest time kept, bounded.
        let now = std::time::Instant::now();
        let mut held = Vec::new();
        remember_unresolved(&mut held, &["reports".to_owned()], now);
        remember_unresolved(&mut held, &["reports".to_owned()], now);
        assert_eq!(held.len(), 1);
        let many: Vec<String> = (0..200).map(|i| format!("f{i}")).collect();
        remember_unresolved(&mut held, &many, now);
        assert_eq!(held.len(), UNRESOLVED_FOLDER_CAP);
        assert!(held.iter().any(|(title, _)| title == "f199"));
    }

    #[test]
    fn launch_windows_prefer_titled_document_windows_over_chrome_strips() {
        let mut windows = vec![
            window(10174, "", 1470.0, 33.0),
            window(10169, "", 1920.0, 30.0),
            window(10166, "Calculator", 674.0, 408.0),
        ];

        rank_launch_windows(&mut windows);

        assert_eq!(
            windows
                .iter()
                .map(|window| window.window_id)
                .collect::<Vec<_>>(),
            vec![10166, 10174, 10169]
        );
    }

    #[test]
    fn launch_window_ranking_preserves_window_server_order_within_groups() {
        let mut windows = vec![
            window(3, "New small document", 300.0, 200.0),
            window(2, "Old large document", 1200.0, 900.0),
            window(1, "", 1920.0, 30.0),
        ];

        rank_launch_windows(&mut windows);

        assert_eq!(
            windows
                .iter()
                .map(|window| window.window_id)
                .collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
    }

    #[test]
    fn local_file_target_treats_plain_paths_as_files() {
        assert_eq!(
            local_file_target("/tmp/does-not-exist.md"),
            Some(PathBuf::from("/tmp/does-not-exist.md"))
        );
        assert_eq!(
            local_file_target("relative/path.md"),
            Some(PathBuf::from("relative/path.md"))
        );
    }

    #[test]
    fn launch_url_normalization_expands_home_relative_paths() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME must be set for macOS"));

        assert_eq!(
            PathBuf::from(normalize_launch_url("~/Desktop/BenchInbox".to_owned())),
            home.join("Desktop/BenchInbox")
        );
        assert_eq!(
            normalize_launch_url("https://example.com".to_owned()),
            "https://example.com"
        );
    }

    #[test]
    fn local_file_target_skips_remote_and_custom_schemes() {
        assert_eq!(local_file_target("https://example.com"), None);
        assert_eq!(local_file_target("about:blank"), None);
        assert_eq!(local_file_target("myapp://open/item"), None);
    }

    #[test]
    fn preflight_file_urls_returns_structured_file_not_found() {
        let missing = "/tmp/cua-driver-definitely-missing-file-for-test.md".to_string();
        let result = preflight_file_urls(&[missing]).expect("missing file should error");
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structured error");
        assert_eq!(structured["error"], "FILE_NOT_FOUND");
        assert_eq!(
            structured["path"],
            "/tmp/cua-driver-definitely-missing-file-for-test.md"
        );
        assert!(structured.get("details").is_none());
    }

    #[test]
    fn local_file_target_percent_decodes_file_urls_before_path_checks() {
        assert_eq!(
            local_file_target("file:///tmp/My%20Doc.txt"),
            Some(PathBuf::from("/tmp/My Doc.txt"))
        );
        assert_eq!(
            local_file_target("file://localhost/tmp/%E2%9C%93.txt"),
            Some(PathBuf::from("/tmp/✓.txt"))
        );
    }

    #[test]
    fn recognizes_release_and_local_protected_host_bundle_ids() {
        assert!(is_cua_driver_bundle_id("com.trycua.driver"));
        assert!(is_cua_driver_bundle_id("com.trycua.driver.local"));
        assert!(!is_cua_driver_bundle_id("com.trycua.harness.tauri"));
    }

    #[test]
    fn launch_timeout_reports_requested_without_process_or_window() {
        let error = anyhow::Error::new(crate::apps::nsworkspace::LaunchError::Timeout)
            .context("Failed to launch com.example.App");
        let result = structured_launch_failure(&error);
        let structured = result.structured_content.expect("structured error");

        assert_eq!(result.is_error, Some(true));
        assert_eq!(structured["error"], "LAUNCH_CALLBACK_TIMEOUT");
        assert_eq!(structured["launch_state"]["requested"], true);
        assert_eq!(structured["launch_state"]["process_running"], false);
        assert_eq!(structured["launch_state"]["window_ready"], false);
    }

    #[test]
    fn invalid_url_reports_request_was_not_sent() {
        let error = anyhow::Error::new(crate::apps::nsworkspace::LaunchError::BadUrl(
            "bad url".to_owned(),
        ))
        .context("Failed to launch com.example.App");
        let result = structured_launch_failure(&error);
        let structured = result.structured_content.expect("structured error");

        assert_eq!(structured["error"], "APP_URL_INVALID");
        assert_eq!(structured["launch_state"]["requested"], false);
        assert_eq!(structured["launch_state"]["process_running"], false);
        assert_eq!(structured["launch_state"]["window_ready"], false);
    }

    #[test]
    fn process_only_response_falls_back_to_requested_identity() {
        assert_eq!(
            response_identity(None, Some("com.apple.Safari"), None),
            ("Safari".to_owned(), "com.apple.Safari".to_owned())
        );
        assert_eq!(
            response_identity(
                None,
                Some("com.example.Editor"),
                Some("/Applications/Example Editor.app"),
            ),
            ("Example Editor".to_owned(), "com.example.Editor".to_owned())
        );
    }

    #[tokio::test]
    async fn launch_app_cannot_reach_private_permission_host_entrypoint() {
        let result = LaunchAppTool
            .invoke(json!({
                "bundle_id": "com.example.not-installed",
                "additional_arguments": [
                    "__permissions-host-request",
                    "--result-file",
                    "/tmp/cua-driver-permissions-forged.json"
                ]
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"],
            "PROTECTED_HOST_ENTRYPOINT"
        );

        let result = LaunchAppTool
            .invoke(json!({ "bundle_id": "com.trycua.driver" }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"],
            "PROTECTED_HOST_ENTRYPOINT"
        );
    }
}
