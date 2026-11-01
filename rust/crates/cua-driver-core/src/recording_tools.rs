//! Platform-independent recording / replay tools.
//!
//! Registered on all platforms via `ToolRegistry::register_recording_tools()`.
//!
//! - `start_recording`     — enable trajectory recording to disk (+ optional video)
//! - `stop_recording`      — disable trajectory recording, finalize video
//! - `get_recording_state` — query current recording state
//! - `replay_trajectory`   — replay a previously recorded trajectory
//!
//! Renamed from the older `set_recording(enabled: bool)` toggle in
//! `02f1f033..` — see `JOURNAL_VIDEO.md` for rationale. The split makes
//! the verbs match the CLI subcommand names (`cua-driver recording start|
//! stop|status`) and removes the "is this a setting write?" ambiguity of
//! the old `set_*` name.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{
    protocol::ToolResult,
    recording::{RecordingSession, RecordingState},
    tool::{Tool, ToolDef, ToolRegistry},
};

pub type ReplayRegistrySlot = Arc<Mutex<Weak<ToolRegistry>>>;

// ── start_recording ──────────────────────────────────────────────────────────

pub struct StartRecordingTool {
    session: Arc<RecordingSession>,
}

impl StartRecordingTool {
    pub fn new(session: Arc<RecordingSession>) -> Self {
        Self { session }
    }
}

static START_REC_DEF: OnceLock<ToolDef> = OnceLock::new();

#[async_trait]
impl Tool for StartRecordingTool {
    fn def(&self) -> &ToolDef {
        START_REC_DEF.get_or_init(|| ToolDef {
            name: "start_recording".into(),
            description: "Start trajectory recording for the calling session (without `session`: every call on this \
                connection, named sessions included). Each action call (click, right_click, scroll, type_text, \
                press_key, hotkey, set_value) writes a turn folder `turn-00001/`, `turn-00002/`, ... under \
                `output_dir`, numbered from 1 on each start: `before_state.json` / `after_state.json` \
                (AX/UIA/AT-SPI state), `before.png` / `after.png` (window screenshots; `screenshot.png` aliases \
                `after.png`), `app_state.json`, `action.json` (tool, arguments, result summary, error flag, pid, \
                click point, ISO-8601 time), `click.png` (click-family only: `before.png` with the click point \
                marked) and `evidence.json` (capture status, and why an expected artifact is missing). Session \
                lifecycle calls and other connections are not recorded; `cua-driver recording start` is \
                daemon-wide.\n\
                \n\
                Each state walk is bounded by `state_timeout_ms` (default 1000) and records the PARTIAL tree \
                when cut (`truncated`, `truncation_reason`, `nodes_visited`, `nodes_pending`, `timeout_ms` in \
                evidence.json); an unresponsive provider is classified `state_capture_timeout`, and actions \
                refused before dispatch `not_applicable` / `action_refused_before_dispatch`. \
                `include_accessibility_tree:false` records no state.\n\
                \n\
                Video is off by default. `record_video:true` also records the main display to \
                `<output_dir>/recording.mp4` (H.264, 30 fps, no audio) until the client disconnects: on macOS \
                15+ with ScreenCaptureKit under the daemon's Screen Recording grant (macOS 26 may ask once for \
                direct screen access; `cua-driver permissions grant` answers it ahead), on Windows and Linux \
                with ffmpeg on PATH (when it is missing or fails, per-turn capture continues and the session's \
                `last_error` says why). State lasts for the daemon's lifetime; `stop_recording` ends it and \
                finalizes the video."
                .into(),
            input_schema: json!({
                "type": "object",
                "required": ["output_dir"],
                "properties": {
                    "output_dir": {
                        "type": "string",
                        "description": "Absolute or ~-rooted directory where turn folders \
                            and (when enabled) the video file are written."
                    },
                    "record_video": {
                        "type": "boolean",
                        "description": "Capture the main display to <output_dir>/recording.mp4. \
                            Default: false. Set to true to also capture the main \
                            display to recording.mp4 (otherwise only the per-turn \
                            screenshots + JSON are recorded). On macOS this uses native \
                            ScreenCaptureKit (macOS 15.0+); macOS 26 can show a one-time \
                            direct screen-capture consent on first use (see \
                            `cua-driver permissions grant`). On Windows + Linux it \
                            requires ffmpeg on PATH."
                    },
                    "state_timeout_ms": {
                        "type": "integer",
                        "minimum": crate::tool_schema::TIMEOUT_MS_MIN,
                        "maximum": crate::tool_schema::TIMEOUT_MS_MAX,
                        "default": crate::recording::TURN_STATE_TIMEOUT_MS_DEFAULT,
                        "description": "Wall-clock budget in milliseconds for EACH per-turn \
                            before/after accessibility walk (same default and bounds as \
                            get_window_state's timeout_ms). A walk that runs out records \
                            the partial tree and marks it truncated in evidence.json."
                    },
                    "include_accessibility_tree": {
                        "type": "boolean",
                        "default": true,
                        "description": "Default true. Set false to skip the per-turn \
                            before/after accessibility walks entirely; screenshots, \
                            click markers and action.json are still recorded and state \
                            is classified `state_capture_disabled`."
                    },
                    "session": {
                        "type": "string",
                        "description": "For multi-call work, prefer a short public session label \
                            and repeat it on every call that accepts it. Omit it to use the \
                            authenticated transport's implicit lifecycle session."
                    }
                },
                "additionalProperties": false
            }),
            read_only: false,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use crate::tool_args::ArgsExt;
        let output_dir = args.opt_str("output_dir");
        if output_dir.as_deref().map(str::is_empty).unwrap_or(true) {
            return ToolResult::error("`output_dir` is required.");
        }
        let record_video = args.bool_or("record_video", false);
        // Daemon-injected ownership key (absent for one-shot CLI / anonymous
        // sessions). Stamps the recording so a session-scoped teardown
        // (session_end) only stops the recording its own session started.
        let owner = args.opt_str("_session_id");

        let state_budget = args.bool_or("include_accessibility_tree", true).then(|| {
            crate::recording::StateCaptureBudget {
                timeout_ms: crate::tool_schema::resolve_timeout_ms(args.get("state_timeout_ms")),
            }
        });

        match self.session.start_with_state_budget(
            output_dir.as_deref().unwrap(),
            record_video,
            owner.as_deref(),
            state_budget,
        ) {
            Ok(()) => {
                let state = self.session.current_state();
                // When the caller asked for video and it failed (e.g. macOS
                // ffmpeg TCC prompt deadlock), surface the actual error
                // prominently — the per-turn capture still runs, but the
                // caller deserves to know the mp4 won't materialize.
                let video_failed = record_video && !state.video_active;
                let video_note = if record_video && state.video_active {
                    " (video → recording.mp4)".to_string()
                } else if video_failed {
                    let err = state.last_error.clone().unwrap_or_else(|| "unknown".into());
                    let hint = if crate::video_ffmpeg::find_ffmpeg().is_none() {
                        "\n\nffmpeg was not found. Call install_ffmpeg (then again with \
                         confirm=true) to install it, then restart recording."
                    } else {
                        ""
                    };
                    format!("\n\n⚠️ Video capture failed (per-turn JSON+screenshot still running):\n{err}{hint}")
                } else {
                    String::new()
                };
                let msg = format!(
                    "✅ Recording started -> {}{}",
                    state.output_dir.as_deref().unwrap_or("?"),
                    video_note
                );
                ToolResult::text(msg).with_structured(recording_state_json(&state))
            }
            Err(e) => ToolResult::error(format!("Failed to start recording: {e}")),
        }
    }
}

// ── stop_recording ───────────────────────────────────────────────────────────

pub struct StopRecordingTool {
    session: Arc<RecordingSession>,
}

impl StopRecordingTool {
    pub fn new(session: Arc<RecordingSession>) -> Self {
        Self { session }
    }
}

static STOP_REC_DEF: OnceLock<ToolDef> = OnceLock::new();

#[async_trait]
impl Tool for StopRecordingTool {
    fn def(&self) -> &ToolDef {
        STOP_REC_DEF.get_or_init(|| ToolDef {
            name: "stop_recording".into(),
            description: "Stop trajectory recording: per-turn capture stops and an enabled video is finalized (playable); \
                `last_video_path` names it. Stopping an already-stopped recording is a no-op. This stops \
                whatever recording is active, whichever session started it; a disconnecting client ends only the \
                recording it started."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            read_only: false,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        // Manual stop is unconditional — `None` requester tears down whatever
        // recording is active. Session-scoped teardown is driven by the
        // registry-owned session-end hook, which calls `stop_owner(sid)`.
        match self.session.stop_owner(None) {
            Ok(()) => {
                let state = self.session.current_state();
                let video_note = state
                    .last_video_path
                    .as_deref()
                    .map(|p| format!(" (video → {p})"))
                    .unwrap_or_default();
                ToolResult::text(format!("✅ Recording stopped.{video_note}"))
                    .with_structured(recording_state_json(&state))
            }
            Err(e) => ToolResult::error(format!("Failed to stop recording: {e}")),
        }
    }
}

// ── get_recording_state ───────────────────────────────────────────────────────

pub struct GetRecordingStateTool {
    session: Arc<RecordingSession>,
}

impl GetRecordingStateTool {
    pub fn new(session: Arc<RecordingSession>) -> Self {
        Self { session }
    }
}

static GET_REC_DEF: OnceLock<ToolDef> = OnceLock::new();

#[async_trait]
impl Tool for GetRecordingStateTool {
    fn def(&self) -> &ToolDef {
        GET_REC_DEF.get_or_init(|| ToolDef {
            name: "get_recording_state".into(),
            // Description ported from Swift `GetRecordingStateTool.swift`.
            description: "Report the current trajectory recorder state: whether recording \
                is enabled, the output directory (when enabled), and the 1-based counter \
                for the next turn folder that will be written. Counter increments on every \
                recorded action tool call and resets to 1 each time recording is \
                (re-)enabled.\n\n\
                Pure read-only.".into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        let state = self.session.current_state();
        // Match Swift text format 1:1:
        //   "✅ recording: enabled output_dir=<path> next_turn=<N>"
        //   "✅ recording: disabled"
        let summary = if state.enabled {
            format!(
                "recording: enabled output_dir={} next_turn={}",
                state.output_dir.as_deref().unwrap_or("?"),
                state.next_turn
            )
        } else {
            "recording: disabled".to_owned()
        };
        ToolResult::text(format!("✅ {summary}")).with_structured(recording_state_json(&state))
    }
}

// ── replay_trajectory ─────────────────────────────────────────────────────────

pub struct ReplayTrajectoryTool {
    registry: ReplayRegistrySlot,
}

impl ReplayTrajectoryTool {
    pub fn new(registry: ReplayRegistrySlot) -> Self {
        Self { registry }
    }
}

static REPLAY_DEF: OnceLock<ToolDef> = OnceLock::new();

#[async_trait]
impl Tool for ReplayTrajectoryTool {
    fn def(&self) -> &ToolDef {
        REPLAY_DEF.get_or_init(|| ToolDef {
            name: "replay_trajectory".into(),
            // Description ported from Swift `ReplayTrajectoryTool.swift`
            // with its caveats about element-indexed actions and recording-
            // during-replay semantics.
            description: "Replay a recorded trajectory: `dir` is a `start_recording` directory, and each \
                `turn-NNNNN/action.json` tool call is re-invoked in lexical order through the normal dispatch \
                path. Element-token actions fail (tokens do not survive sessions); pixel clicks and keyboard \
                tools replay. Failures are reported and stop the replay only with `stop_on_error`. Read-only \
                tools such as `get_window_state` are not recorded, so no element cache is rebuilt. If recording \
                is on, the replay is itself recorded, so two builds' trajectories can be diffed.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["dir"],
                "properties": {
                    "dir":           { "type": "string",  "description": "Trajectory directory previously written by `start_recording`. Absolute or ~-rooted." },
                    "delay_ms":      { "type": "integer", "minimum": 0, "maximum": 10000, "description": "Milliseconds to sleep between turns, for human-observable pacing. Default 500." },
                    "stop_on_error": { "type": "boolean", "description": "Stop replay on the first tool-call error. Default true — set false to best-effort through the full trajectory." }
                },
                "additionalProperties": false
            }),
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        // Swift error wording 1:1.
        let dir_str = match args.get("dir").and_then(|v| v.as_str()) {
            Some(v) if !v.is_empty() => v.to_owned(),
            _ => return ToolResult::error("Missing required string field `dir`."),
        };
        use crate::tool_args::ArgsExt;
        let delay_ms = args.u64_or("delay_ms", 500).min(10_000);
        let stop_on_error = args.bool_or("stop_on_error", true);

        // Expand ~/
        let dir = {
            let p = std::path::PathBuf::from(&dir_str);
            if let Some(relative) = dir_str.strip_prefix("~/") {
                if let Ok(home) = std::env::var("HOME") {
                    std::path::PathBuf::from(home).join(relative)
                } else {
                    p
                }
            } else {
                p
            }
        };

        if !dir.exists() {
            return ToolResult::error(format!(
                "Trajectory directory does not exist: {}",
                dir.display()
            ));
        }

        // Collect and sort turn-NNNNN directories.
        let mut turn_dirs: Vec<_> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_dir()
                            && p.file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n.starts_with("turn-"))
                                .unwrap_or(false)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        turn_dirs.sort();

        if turn_dirs.is_empty() {
            return ToolResult::error(format!(
                "No turn-NNNNN folders found under {}",
                dir.display()
            ));
        }

        let registry = match self.registry.lock().unwrap().upgrade() {
            Some(r) => r,
            None => {
                return ToolResult::error("Replay not available: registry not initialised yet.")
            }
        };

        let mut attempted = 0u32;
        let mut succeeded = 0u32;
        let mut failed = 0u32;
        let mut turns_json = Vec::new();
        let mut first_failure: Option<(String, String, String)> = None;

        for turn_dir in &turn_dirs {
            let turn_name = turn_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_owned();

            let action_path = turn_dir.join("action.json");
            let (tool_name, tool_args, withheld_args) = match parse_action_json(&action_path) {
                Ok(v) => v,
                Err(e) => {
                    failed += 1;
                    if first_failure.is_none() {
                        first_failure =
                            Some((turn_name.clone(), "action.json".into(), e.to_string()));
                    }
                    turns_json.push(json!({
                        "turn": turn_name,
                        "ok": false,
                        "parse_error": e.to_string()
                    }));
                    if stop_on_error {
                        break;
                    }
                    continue;
                }
            };

            attempted += 1;
            if !withheld_args.is_empty() {
                failed += 1;
                if first_failure.is_none() {
                    first_failure = Some((
                        turn_name.clone(),
                        tool_name.clone(),
                        "input_withheld".into(),
                    ));
                }
                turns_json.push(json!({
                    "turn": turn_name, "tool": tool_name, "ok": false,
                    "code": "input_withheld", "withheld_args": withheld_args,
                }));
                if stop_on_error {
                    break;
                }
                continue;
            }
            let result = registry.invoke(&tool_name, tool_args).await;
            let is_err = result.is_error.unwrap_or(false);
            let summary = result
                .content
                .iter()
                .find_map(|c| {
                    if let crate::protocol::Content::Text { text, .. } = c {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .unwrap_or("")
                .to_owned();

            turns_json.push(json!({
                "turn": turn_name,
                "tool": &tool_name,
                "ok": !is_err,
                "result_summary": &summary,
            }));

            if is_err {
                failed += 1;
                if first_failure.is_none() {
                    first_failure = Some((turn_name.clone(), tool_name.clone(), summary));
                }
                if stop_on_error {
                    break;
                }
            } else {
                succeeded += 1;
            }

            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }

        let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let mut summary_text = format!(
            "replay {dir_name}: attempted={attempted} succeeded={succeeded} failed={failed}"
        );
        if let Some((ref turn, ref tool, _)) = first_failure {
            summary_text.push_str(&format!(" first_failure={turn}:{tool}"));
        }

        let mut structured = json!({
            "directory": dir.to_string_lossy(),
            "attempted": attempted,
            "succeeded": succeeded,
            "failed": failed,
            "stop_on_error": stop_on_error,
            "turns": turns_json,
        });
        if let Some((turn, tool, error)) = first_failure {
            structured["first_failure"] = json!({ "turn": turn, "tool": tool, "error": error });
        }

        ToolResult::text(summary_text).with_structured(structured)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn recording_state_json(state: &RecordingState) -> Value {
    json!({
        // "recording" mirrors the Swift field name for parity.
        "recording": state.enabled,
        "enabled": state.enabled,
        "output_dir": state.output_dir,
        "next_turn": state.next_turn,
        "last_error": state.last_error,
        "video_active": state.video_active,
        "last_video_path": state.last_video_path,
        // Session that owns the live recording (the daemon-injected
        // `_session_id`), or null when started anonymously. Informational; the
        // proxy-exit teardown drives ownership via the daemon `session_end`
        // signal rather than reading this back.
        "owner": state.owner,
    })
}

fn parse_action_json(path: &std::path::Path) -> anyhow::Result<(String, Value, Vec<String>)> {
    if !path.exists() {
        anyhow::bail!("Missing action.json");
    }
    let text = std::fs::read_to_string(path)?;
    let obj: Value = serde_json::from_str(&text)?;
    let tool = obj
        .get("tool")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("action.json missing 'tool' string field"))?
        .to_owned();
    let tool_args = obj
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let mut withheld_args: Vec<String> = match obj.get("withheld_args") {
        Some(value) => serde_json::from_value(value.clone())?,
        None => Vec::new(),
    };
    if withheld_args.is_empty() {
        withheld_args = legacy_placeholder_args(&tool, &tool_args);
    }
    Ok((tool, tool_args, withheld_args))
}

/// Recordings made before input was withheld replaced these arguments with
/// the literal "[redacted]". Replaying them would type or paste that
/// placeholder, so they are treated as withheld.
fn legacy_placeholder_args(tool: &str, args: &Value) -> Vec<String> {
    let fields: &[&str] = match tool {
        "browser_dialog" => &["prompt_text"],
        "clipboard_write" => &["text", "image_path", "file_path"],
        _ => &[],
    };
    fields
        .iter()
        .filter(|field| args.get(**field).and_then(Value::as_str) == Some("[redacted]"))
        .map(|field| (*field).to_owned())
        .collect()
}

// ── install_ffmpeg ────────────────────────────────────────────────────────────
//
// Confirmation-gated installer for the ffmpeg binary that the Linux/Windows
// video backend shells out to. Called without `confirm` it only REPORTS the
// command it would run (read-only preview); `confirm: true` runs it. Marked
// destructive + open_world so conforming MCP clients also gate it behind a
// human approval. ffmpeg is invoked as a separate process, never linked.

pub struct InstallFfmpegTool;
static INSTALL_FFMPEG_DEF: OnceLock<ToolDef> = OnceLock::new();

#[async_trait]
impl Tool for InstallFfmpegTool {
    fn def(&self) -> &ToolDef {
        INSTALL_FFMPEG_DEF.get_or_init(|| ToolDef {
            name: "install_ffmpeg".into(),
            description: "Install the ffmpeg binary used by start_recording's video \
                capture (Linux/Windows; macOS records natively and needs no ffmpeg). \
                Two-step and confirmed: called without `confirm` it only REPORTS the \
                exact install command for this platform's package manager; pass \
                `confirm: true` to actually run it. No-op if ffmpeg is already on PATH. \
                ffmpeg is run as a separate process, never linked into the driver."
                .into(),
            input_schema: json!({"type":"object","properties":{
                "confirm":{"type":"boolean","description":"Run the install command. Without it, only the planned command is reported."}
            },"additionalProperties":false}),
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use crate::tool_args::ArgsExt;

        if let Some(path) = crate::video_ffmpeg::find_ffmpeg() {
            return ToolResult::text(format!(
                "✅ ffmpeg already available ({}). Nothing to install.",
                path.display()
            ))
            .with_structured(json!({
                "installed": true, "ran": false, "path": path.display().to_string()
            }));
        }

        let Some(plan) = crate::ffmpeg_install::install_plan() else {
            return ToolResult::error(
                "ffmpeg is not installed and no supported package manager was found to \
                 install it automatically. Install ffmpeg manually and put it on PATH \
                 (Linux: apt/dnf/pacman/zypper/apk/snap; macOS: `brew install ffmpeg`; \
                 Windows: `winget install Gyan.FFmpeg`).",
            );
        };

        if !args.bool_or("confirm", false) {
            return ToolResult::text(format!(
                "ffmpeg is not installed. To install it via {}, re-call install_ffmpeg \
                 with confirm=true.\n\nCommand that will run:\n  {}",
                plan.manager,
                plan.display()
            ))
            .with_structured(json!({
                "installed": false, "ran": false,
                "manager": plan.manager, "command": plan.display()
            }));
        }

        let display = plan.display();
        let result =
            tokio::task::spawn_blocking(move || crate::ffmpeg_install::run_install(&plan)).await;
        match result {
            Ok(Ok((cmd_ok, output))) => match crate::video_ffmpeg::find_ffmpeg() {
                Some(path) => ToolResult::text(format!("✅ ffmpeg installed via `{display}`."))
                    .with_structured(json!({
                        "installed": true, "ran": true,
                        "command": display, "path": path.display().to_string()
                    })),
                None => ToolResult::error(format!(
                    "Ran the install command but ffmpeg is still not found.\n\
                     Command: {display}\ncommand_succeeded={cmd_ok}\nOutput tail:\n{output}"
                )),
            },
            Ok(Err(e)) => {
                ToolResult::error(format!("ffmpeg install failed: {e}\nCommand: {display}"))
            }
            Err(e) => ToolResult::error(format!("install task error: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe {
        def: ToolDef,
        calls: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait]
    impl Tool for Probe {
        fn def(&self) -> &ToolDef {
            &self.def
        }
        async fn invoke(&self, mut args: Value) -> ToolResult {
            if let Some(map) = args.as_object_mut() {
                map.retain(|key, _| !key.starts_with('_'));
            }
            self.calls.lock().unwrap().push(args);
            ToolResult::text("probe ran")
        }
    }

    async fn replay_probe(actions: &[Value], stop: bool) -> (Value, Vec<Value>) {
        let root = tempfile::tempdir().unwrap();
        for (i, action) in actions.iter().enumerate() {
            let dir = root.path().join(format!("turn-{:05}", i + 1));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("action.json"), action.to_string()).unwrap();
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        for name in ["type_text", "browser_dialog"] {
            registry.register(Box::new(Probe {
                calls: calls.clone(),
                def: ToolDef {
                    name: name.into(),
                    description: "probe".into(),
                    input_schema: json!({"type": "object"}),
                    read_only: false,
                    destructive: false,
                    idempotent: false,
                    open_world: false,
                },
            }));
        }
        let registry = Arc::new(registry);
        let replay = ReplayTrajectoryTool::new(Arc::new(Mutex::new(Arc::downgrade(&registry))));
        let result = replay
            .invoke(
                json!({"dir": root.path().to_str().unwrap(), "delay_ms": 0, "stop_on_error": stop}),
            )
            .await;
        let seen = calls.lock().unwrap().clone();
        (result.structured_content.unwrap(), seen)
    }

    #[tokio::test]
    async fn replay_refuses_a_withheld_turn_without_dispatch() {
        let (result, calls) = replay_probe(
            &[
                json!({"tool":"type_text", "arguments":{}, "withheld_args":["text"]}),
                json!({"tool":"type_text", "arguments":{"text":"next"}}),
            ],
            true,
        )
        .await;
        assert!(calls.is_empty());
        assert_eq!(result["attempted"], 1);
        assert_eq!(result["failed"], 1);
        assert_eq!(result["turns"][0]["code"], "input_withheld");
        assert_eq!(result["turns"][0]["withheld_args"], json!(["text"]));
        assert_eq!(result["first_failure"]["tool"], "type_text");
    }

    #[tokio::test]
    async fn replay_continues_after_a_withheld_turn_when_stop_on_error_is_false() {
        let (result, calls) = replay_probe(
            &[
                json!({"tool":"type_text", "arguments":{}, "withheld_args":["text"]}),
                json!({"tool":"type_text", "arguments":{"text":"next"}}),
            ],
            false,
        )
        .await;
        assert_eq!(calls, vec![json!({"text":"next"})]);
        assert_eq!(result["attempted"], 2);
        assert_eq!(result["failed"], 1);
        assert_eq!(result["succeeded"], 1);
    }

    #[tokio::test]
    async fn legacy_literal_redacted_text_replays_unchanged() {
        let (result, calls) = replay_probe(
            &[json!({"tool":"type_text", "arguments":{"text":"[redacted]"}})],
            true,
        )
        .await;
        assert_eq!(calls, vec![json!({"text":"[redacted]"})]);
        assert_eq!(result["succeeded"], 1);
    }

    #[tokio::test]
    async fn browser_dialog_prompt_text_is_withheld_and_not_replayed() {
        let (result, calls) = replay_probe(&[json!({"tool":"browser_dialog", "arguments":{"action":"accept"}, "withheld_args":["prompt_text"]})], true).await;
        assert!(calls.is_empty());
        assert_eq!(result["turns"][0]["code"], "input_withheld");
    }

    #[tokio::test]
    async fn legacy_redacted_placeholders_are_never_replayed() {
        for turn in [
            json!({"tool":"browser_dialog", "arguments":{"action":"accept", "prompt_text":"[redacted]"}}),
            json!({"tool":"clipboard_write", "arguments":{"text":"[redacted]"}}),
        ] {
            let (result, calls) = replay_probe(std::slice::from_ref(&turn), true).await;
            assert!(calls.is_empty(), "{turn}");
            assert_eq!(result["turns"][0]["code"], "input_withheld", "{turn}");
        }
    }

    #[tokio::test]
    async fn malformed_withheld_metadata_refuses_dispatch() {
        let (result, calls) = replay_probe(
            &[json!({"tool":"type_text", "arguments":{"text":"secret"}, "withheld_args":"text"})],
            true,
        )
        .await;
        assert!(calls.is_empty());
        assert_eq!(result["failed"], 1);
        assert!(result["turns"][0].get("parse_error").is_some());
    }

    #[tokio::test]
    async fn start_recording_resolves_the_per_turn_state_budget() {
        let session = Arc::new(RecordingSession::new());
        let tool = StartRecordingTool::new(session.clone());
        let properties = &tool.def().input_schema["properties"];
        assert_eq!(properties["state_timeout_ms"]["default"], 1000);
        assert_eq!(properties["include_accessibility_tree"]["default"], true);

        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        for (args, expected) in [
            (json!({"output_dir": dir}), Some(1000)),
            (
                json!({"output_dir": dir, "state_timeout_ms": 250}),
                Some(250),
            ),
            // Clamped to the shared timeout_ms bounds.
            (json!({"output_dir": dir, "state_timeout_ms": 1}), Some(100)),
            (
                json!({"output_dir": dir, "include_accessibility_tree": false}),
                None,
            ),
        ] {
            let result = tool.invoke(args.clone()).await;
            assert_ne!(result.is_error, Some(true), "{args}");
            assert_eq!(
                session.state_budget().map(|budget| budget.timeout_ms),
                expected,
                "{args}"
            );
            session.stop_owner(None).unwrap();
        }
    }
}
