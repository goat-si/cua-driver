//! Stdio MCP adapters for direct SDK-owned and service-owned runtimes.
//!
//! The client side always sees a normal stdio server. Depending on platform
//! and explicit launch options, this adapter either owns the SDK runtime
//! directly or forwards to a service that owns it.
//!
//! On macOS the CLI can ensure a daemon is running under `LaunchServices`
//! (which gives it the right TCC attribution). Embedded hosts may also start a
//! private service explicitly. The MCP client never sees that ownership
//! boundary — it receives the same JSON-RPC envelope.
//!
//! Why this lives in `cua-driver` and not `mcp-server`:
//!   `cua_driver_core::server` defines the shared JSON-RPC protocol. The
//!   proxy speaks that protocol on the client side, while the server side is the daemon's
//!   line-delimited JSON UDS protocol, owned by `crate::serve`.
//!   Putting the proxy here avoids `mcp-server → cua-driver` reverse
//!   coupling.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use cua_driver_core::mcp_wire::{self, ProtocolSession};
use cua_driver_core::policy::{authorize_tool_call, validate_configured_policy};
use cua_driver_core::protocol::{initialize_result, Request, Response};
use cua_driver_core::server::{
    observe_proxy_session_started, observe_proxy_tool_completed, tool_observation_timer,
    StdioExecutionPath,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::{AbortHandle, JoinSet};
use tracing::{debug, error, warn};

use crate::serve::{
    is_daemon_listening, send_request, DaemonRequest, DaemonResponse, ToolObservationOrigin,
};

/// Run stdio MCP directly over an SDK-owned runtime.
///
/// Windows and Linux use this when no explicit service endpoint was selected.
/// The runtime lives exactly as long as stdin: EOF ends every observed public
/// session, drains admitted work through `shutdown`, and releases process
/// ownership before returning.
pub async fn run_direct(driver: Arc<cua_driver_sdk::CuaDriver>) -> anyhow::Result<()> {
    // Direct stdio is an action endpoint just like `serve`; enforce the same
    // admin lock, bounded-manifest approval/expiry, and legacy-approval
    // consistency before the first request can be read.
    cua_driver_core::authorization::validate_startup_authorization()?;
    validate_configured_policy()?;
    let sdk = crate::sdk_adapter::SdkAdapter::load(driver.clone()).await?;
    if crate::mcp_envelope::configured()? {
        let result = crate::mcp_envelope::run(
            sdk.clone(),
            BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
        )
        .await;
        sdk.shutdown().await.map_err(anyhow::Error::msg)?;
        return result;
    }
    let transport_session = format!("mcp-{}", uuid::Uuid::new_v4());
    struct DirectTransportCleanup {
        sdk: Arc<crate::sdk_adapter::SdkAdapter>,
        transport_session: String,
    }
    impl Drop for DirectTransportCleanup {
        fn drop(&mut self) {
            self.sdk.end_transport_sessions(&self.transport_session);
        }
    }
    let _cleanup = DirectTransportCleanup {
        sdk: sdk.clone(),
        transport_session: transport_session.clone(),
    };
    let deadline = tool_call_deadline()?;
    let direct_sdk = sdk.clone();
    let direct_handler_sdk = sdk.clone();
    let direct_session = transport_session.clone();
    let result = run_concurrent_mcp_io(
        BufReader::new(tokio::io::stdin()),
        tokio::io::BufWriter::new(tokio::io::stdout()),
        deadline,
        move |request: &Request| protocol_admission(request, &direct_sdk.tools_list()),
        move |mut request: Request, id: serde_json::Value| {
            apply_direct_session_identity(&mut request, &direct_session);
            let session_context = request.tool_call().ok().and_then(|call| {
                direct_handler_sdk.begin_tool_call(
                    &call.name,
                    &call.args,
                    cua_driver_core::session::SessionTransport::McpStdio,
                    cua_driver_core::session::SessionClientKind::Mcp,
                )
            });
            let timer = tool_observation_timer(
                &request,
                |name| direct_handler_sdk.is_known_tool(name),
                StdioExecutionPath::DirectDaemon,
            );
            let mut telemetry = ToolCallTelemetry {
                timer,
                session_context,
                id: id.clone(),
            };
            let sdk = direct_handler_sdk.clone();
            let session = direct_session.clone();
            async move {
                let response = cua_driver_core::server::handle_request_with_transport_session(
                    request,
                    id,
                    sdk.as_ref(),
                    &session,
                )
                .await;
                telemetry.finish(&response);
                response
            }
        },
    )
    .await;
    drop(_cleanup);
    let shutdown = sdk.shutdown().await.map_err(anyhow::Error::msg);
    result?;
    shutdown
}

const TOOL_CALL_DEADLINE_ENV: &str = "CUA_DRIVER_TOOL_CALL_TIMEOUT_SECS";
const DEFAULT_TOOL_CALL_DEADLINE: Duration = Duration::from_secs(110);
const MAX_IN_FLIGHT_REQUESTS: usize = 16;
static DAEMON_FORWARD_PERMITS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_IN_FLIGHT_REQUESTS);

fn tool_call_deadline() -> anyhow::Result<Duration> {
    let Some(raw) = std::env::var_os(TOOL_CALL_DEADLINE_ENV) else {
        return Ok(DEFAULT_TOOL_CALL_DEADLINE);
    };
    let secs = raw.to_string_lossy().trim().parse::<u64>().map_err(|_| {
        anyhow::anyhow!("{TOOL_CALL_DEADLINE_ENV} must be an integer from 1 to 110")
    })?;
    anyhow::ensure!(
        (1..=110).contains(&secs),
        "{TOOL_CALL_DEADLINE_ENV} must be an integer from 1 to 110"
    );
    Ok(Duration::from_secs(secs))
}

fn protocol_admission(request: &Request, inventory: &serde_json::Value) -> Result<(), Response> {
    let era = mcp_wire::classify_request(request)?;
    if request.method == "tools/call" {
        mcp_wire::validate_tool_call(
            request,
            request.id.clone().unwrap_or_default(),
            era,
            inventory,
        )?;
    }
    Ok(())
}

struct PendingRequest {
    token: u64,
    id: serde_json::Value,
    tool_name: Option<String>,
    abort: AbortHandle,
}

struct RequestCompletion {
    key: String,
    token: u64,
    response: Response,
}

struct ToolCallTelemetry {
    timer: Option<cua_driver_core::server::ToolObservationTimer>,
    session_context: Option<cua_driver_core::session::SessionToolContext>,
    id: serde_json::Value,
}

impl ToolCallTelemetry {
    fn finish(&mut self, response: &Response) {
        if let Some(timer) = self.timer.take() {
            let outcome = timer.finish(response);
            if let Some(context) = self.session_context.take() {
                context.complete(&outcome);
            }
            observe_proxy_tool_completed(outcome);
        }
    }
}

impl Drop for ToolCallTelemetry {
    fn drop(&mut self) {
        if self.timer.is_some() {
            let result = cua_driver_core::protocol::ToolResult::error(
                "tool call was cancelled or abandoned; its effect is unknown. \
                 Inspect target state before another action.",
            )
            .with_structured(serde_json::json!({
                "code": "tool_call_abandoned",
                "effect": "unknown",
            }));
            let response = Response::ok(
                self.id.clone(),
                serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
            );
            self.finish(&response);
        }
    }
}

fn cancelled_response(pending: &PendingRequest) -> Response {
    if let Some(tool) = pending.tool_name.as_deref() {
        let mut result = cua_driver_core::protocol::ToolResult::error(format!(
            "{tool} was cancelled; its effect is unknown. Inspect target state before another action."
        ))
        .with_structured(serde_json::json!({
            "code": "tool_call_cancelled",
            "effect": "unknown",
            "tool": tool,
        }));
        if matches!(tool, "press_key" | "hotkey") {
            if let Some(serde_json::Value::Object(fields)) = result.structured_content.as_mut() {
                fields.insert("key_release".into(), serde_json::json!("pending"));
            }
        }
        return Response::ok(
            pending.id.clone(),
            serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
        );
    }
    Response::error(pending.id.clone(), -32800, "request cancelled")
}

fn deadline_response(id: serde_json::Value, tool: Option<&str>, deadline: Duration) -> Response {
    if let Some(tool) = tool {
        let result = cua_driver_core::protocol::ToolResult::error(format!(
            "{tool} did not finish within {}s and was abandoned; its effect is unknown. \
             Inspect the target state before another action.",
            deadline.as_secs()
        ))
        .with_structured(serde_json::json!({
            "code": "tool_call_timeout",
            "effect": "unknown",
            "tool": tool,
            "deadline_secs": deadline.as_secs(),
        }));
        return Response::ok(
            id,
            serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
        );
    }
    Response::error(
        id,
        -32603,
        format!(
            "request exceeded the {}s deadline; effect is unknown",
            deadline.as_secs()
        ),
    )
}

async fn write_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: Response,
) -> anyhow::Result<()> {
    let serialized = serde_json::to_string(&response).unwrap_or_else(|error| {
        format!(
            r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize error: {error}"}}}}"#
        )
    });
    writer.write_all(serialized.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn run_concurrent_mcp_io<R, W, A, F, Fut>(
    mut reader: R,
    mut writer: W,
    deadline: Duration,
    mut admit: A,
    handler: F,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
    A: FnMut(&Request) -> Result<(), Response>,
    F: Fn(Request, serde_json::Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    let handler = Arc::new(handler);
    let mut tasks = JoinSet::<RequestCompletion>::new();
    let mut pending = HashMap::<String, PendingRequest>::new();
    let mut line = Vec::new();
    let mut next_token = 0u64;
    let mut session_observed = false;
    let mut protocol_session = ProtocolSession::default();

    loop {
        let read = if line.contains(&b'\n') {
            line.len()
        } else {
            tokio::select! {
                biased;
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    match joined {
                        Some(Ok(completion)) => {
                            let should_write = pending
                                .get(&completion.key)
                                .is_some_and(|entry| entry.token == completion.token);
                            if should_write {
                                pending.remove(&completion.key);
                                write_response(&mut writer, completion.response).await?;
                            }
                        }
                        Some(Err(join_error)) => {
                            let failed_key = pending.iter().find_map(|(key, request)| {
                                (request.abort.id() == join_error.id()).then(|| key.clone())
                            });
                            if let Some(failed_key) = failed_key {
                                let failed = pending
                                    .remove(&failed_key)
                                    .expect("pending request found by task id");
                                warn!(
                                    request_id = %failed.id,
                                    panicked = join_error.is_panic(),
                                    "MCP request handler task failed"
                                );
                                write_response(
                                    &mut writer,
                                    Response::error(
                                        failed.id,
                                        -32603,
                                        "request handler failed; effect is unknown",
                                    ),
                                )
                                .await?;
                            }
                        }
                        None => {}
                    }
                    continue;
                }
                result = reader.read_until(b'\n', &mut line) => result?,
            }
        };
        if read == 0 {
            break;
        }
        let trimmed = String::from_utf8_lossy(&line).trim().to_owned();
        line.clear();
        if trimmed.is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<Request>(&trimmed) {
            Ok(request) => request,
            Err(error) => {
                error!("JSON parse error: {error}");
                write_response(&mut writer, Response::parse_error()).await?;
                continue;
            }
        };
        debug!(method = %request.method, id = ?request.id, "MCP request");

        if request.method == "notifications/cancelled" && request.is_notification() {
            let request_id = request
                .params
                .as_ref()
                .and_then(|params| params.get("requestId"))
                .cloned();
            if let Some(request_id) = request_id {
                let key = request_id.to_string();
                if let Some(pending_request) = pending.remove(&key) {
                    pending_request.abort.abort();
                    write_response(&mut writer, cancelled_response(&pending_request)).await?;
                }
            }
            continue;
        }
        if request.is_notification() {
            continue;
        }

        let id = request.id.clone().unwrap_or(serde_json::Value::Null);
        let key = id.to_string();
        if pending.contains_key(&key) {
            write_response(
                &mut writer,
                Response::error(id, -32600, "duplicate active MCP request ID"),
            )
            .await?;
            continue;
        }
        if tasks.len() >= MAX_IN_FLIGHT_REQUESTS {
            write_response(&mut writer, Response::error(id, -32029, "in_flight_limit")).await?;
            continue;
        }
        let admission = protocol_session.validate(&request).and_then(|era| {
            admit(&request)?;
            Ok(era)
        });
        if let Err(response) = admission {
            write_response(&mut writer, response).await?;
            continue;
        }
        if !session_observed {
            if let Some(metadata) = request.initialize_metadata() {
                observe_proxy_session_started(metadata);
                session_observed = true;
            }
        }

        // Keep initialize ordering deterministic: the first action cannot run
        // before the initialize response has been written.
        let metadata_request = mcp_wire::handle_metadata_request(&request, id.clone()).is_some();
        if request.method == "initialize" || metadata_request {
            let tool_name = request.tool_call().ok().map(|call| call.name);
            let response = match tokio::time::timeout(deadline, handler(request, id.clone())).await
            {
                Ok(response) => response,
                Err(_) => deadline_response(id, tool_name.as_deref(), deadline),
            };
            write_response(&mut writer, response).await?;
            continue;
        }

        next_token = next_token.wrapping_add(1);
        let token = next_token;
        let tool_name = request.tool_call().ok().map(|call| call.name);
        let handler = handler.clone();
        let task_id = id.clone();
        let task_key = key.clone();
        let task_tool_name = tool_name.clone();
        let future = handler(request, task_id.clone());
        let abort = tasks.spawn(async move {
            let response = match tokio::time::timeout(deadline, future).await {
                Ok(response) => response,
                Err(_) => deadline_response(task_id, task_tool_name.as_deref(), deadline),
            };
            RequestCompletion {
                key: task_key,
                token,
                response,
            }
        });
        pending.insert(
            key,
            PendingRequest {
                token,
                id,
                tool_name,
                abort,
            },
        );
    }

    for request in pending.values() {
        request.abort.abort();
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

pub(crate) fn apply_direct_session_identity(request: &mut Request, transport_session: &str) {
    let Some(arguments) = request
        .params
        .as_mut()
        .and_then(|params| params.get_mut("arguments"))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    let effective = arguments
        .get("session")
        .and_then(serde_json::Value::as_str)
        .filter(|session| !session.is_empty())
        .unwrap_or(transport_session)
        .to_owned();
    arguments.insert("_session_id".into(), serde_json::Value::String(effective));
    arguments.insert(
        "_transport_session_id".into(),
        serde_json::Value::String(transport_session.to_owned()),
    );
}

/// Run the MCP stdio proxy. Reads JSON-RPC lines from stdin, forwards
/// the body of each `tools/list` / `tools/call` to the daemon at
/// `socket_path`, and writes the daemon's response back as a proper
/// JSON-RPC envelope.
///
/// Implements the core protocol's EOF, parse-error, notification, and
/// response rules while forwarding method dispatch to the daemon.
///
/// Fails fast if the daemon isn't reachable, so MCP clients see a
/// clear startup error instead of a "successful" handshake that
/// advertises zero tools and then errors on every call. Matches
/// Swift `makeProxy`'s `fetchProxyToolList` pre-check.
pub async fn run_proxy(socket_path: String, on_ready: impl FnOnce()) -> anyhow::Result<()> {
    validate_configured_policy()?;
    let probe_path = socket_path.clone();
    if !startup_step("listening probe", move || {
        Ok(is_daemon_listening(&probe_path))
    })
    .await?
    {
        anyhow::bail!(
            "cua-driver-rs daemon not reachable on {socket_path}. Start it \
             with `cua-driver permissions grant` and retry."
        );
    }
    // A selected service may outlive the CLI package that launched this
    // proxy. Refuse an incompatible contract before creating the control
    // binding or forwarding any action.
    let compatibility_path = socket_path.clone();
    startup_step("compatibility handshake", move || {
        // The client is dropped on this blocking thread: its Drop sends a
        // synchronous `session_end` that must stay inside the deadline.
        let client = cua_driver_sdk::CuaDriver::connect(Some(compatibility_path))?;
        tokio::runtime::Handle::current().block_on(client.metadata())?;
        Ok(())
    })
    .await?;
    if crate::mcp_envelope::configured()? {
        return crate::mcp_envelope::proxy(&socket_path, on_ready).await;
    }

    // Mint this MCP session's identity once at proxy startup. One proxy process
    // == one MCP session; the daemon outlives it. We stamp this id on every
    // forwarded request so the daemon can OWN and CLEAN UP this session's
    // state (recording, config overrides) and tear it down on disconnect via
    // a `session_end` signal. Dep-free `pid + start-nanos` is sufficient for
    // daemon-local uniqueness over this proxy's lifetime (no `uuid` crate dep
    // for one mint).
    let session_id = mint_session_id();
    debug!(session_id = %session_id, "proxy session minted");

    // Open ONE long-lived "control" connection to the daemon and hold it open
    // for this proxy's entire lifetime (separate from the per-call connections
    // that `send_request` opens and closes per tool call). It sends a single
    // `session_begin` line and then parks reading — it never writes again and
    // never closes until this process dies.
    //
    // This is the reaper: when the proxy exits (graceful stdin EOF) OR is
    // SIGKILLed/crashes, the kernel closes this socket; the daemon's
    // per-connection reader hits EOF and fires `session_end` for `session_id`,
    // tearing down every piece of state this session owns (overlay cursor,
    // config overrides, recording). Liveness is connection-based, so an
    // alive-but-idle session — one issuing zero tool calls — is never reaped:
    // its control connection stays parked open.
    //
    // The daemon must acknowledge `session_begin` before the proxy accepts tool
    // calls. Besides lifecycle cleanup, that registered control channel is the
    // trust boundary used by destructive `browser_prepare` calls.
    let (control_ready_tx, control_ready_rx) = tokio::sync::oneshot::channel();
    let control = run_control_connection(socket_path.clone(), session_id.clone(), control_ready_tx);
    tokio::pin!(control);
    supervise_control_connection(&mut control, async {
        tokio::time::timeout(std::time::Duration::from_secs(4), control_ready_rx)
            .await
            .map_err(|_| anyhow::anyhow!("daemon did not acknowledge the MCP control session"))?
            .map_err(|_| anyhow::anyhow!("daemon control session closed before acknowledgement"))
    })
    .await?;

    // Cache the tool list once at startup. The daemon's registry is
    // static for the lifetime of the daemon, so polling on every
    // `tools/list` would waste a round-trip per call. Swift does the
    // same caching in `fetchProxyToolList`.
    let (list_path, list_session) = (socket_path.clone(), session_id.clone());
    let (cached_tools_list, daemon_observes_tool_calls) = supervise_control_connection(
        &mut control,
        startup_step("tool list request", move || {
            fetch_tools_list_from_daemon(&list_path, &list_session)
        }),
    )
    .await?;
    let cached_tools_list = Arc::new(cached_tools_list);
    on_ready();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    supervise_control_connection(
        &mut control,
        run_proxy_io(
            BufReader::new(stdin),
            tokio::io::BufWriter::new(stdout),
            &socket_path,
            &cached_tools_list,
            &session_id,
            daemon_observes_tool_calls,
        ),
    )
    .await
}

/// Longest the MCP proxy waits for one daemon exchange before it can answer
/// `initialize`. The daemon client's reply deadline is sized for tool calls
/// (minutes); a daemon that stops answering during startup must instead fail
/// the MCP process promptly.
pub(crate) const DAEMON_STARTUP_DEADLINE: Duration = Duration::from_secs(5);

/// Run one blocking startup exchange off the async threads, bounded by
/// [`DAEMON_STARTUP_DEADLINE`]. A timed-out exchange is abandoned; the
/// runtime is shut down in the background when the proxy fails.
async fn startup_step<T: Send + 'static>(
    exchange: &'static str,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    match tokio::time::timeout(DAEMON_STARTUP_DEADLINE, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(anyhow::anyhow!("daemon {exchange} failed: {error}")),
        Err(_) => Err(anyhow::anyhow!(
            "daemon did not answer the {exchange} within {DAEMON_STARTUP_DEADLINE:?}"
        )),
    }
}

/// Keep the MCP transport alive only while its daemon-owned identity is live.
/// Dropping the scoped control future also closes its socket on stdin EOF,
/// startup failure, or cancellation; no detached task can retain the session.
async fn supervise_control_connection<T>(
    control: impl std::future::Future<Output = ()>,
    work: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::select! {
        biased;
        _ = control => anyhow::bail!("daemon control session closed; reconnect the MCP client"),
        result = work => result,
    }
}

/// Run the service-owned stdio loop over caller-provided I/O.
///
/// A clean reader EOF must return `Ok(())` promptly. The caller then drops the
/// persistent control connection, allowing the daemon to reap the MCP session
/// and its recording, preview, and overlay state (issue #2002).
async fn run_proxy_io<R, W>(
    reader: R,
    writer: W,
    socket_path: &str,
    cached_tools_list: &Arc<serde_json::Value>,
    session_id: &str,
    daemon_observes_tool_calls: bool,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let deadline = tool_call_deadline()?;
    let inventory = cached_tools_list.clone();
    let socket = socket_path.to_owned();
    let session = session_id.to_owned();
    let handler_inventory = inventory.clone();
    run_concurrent_mcp_io(
        reader,
        writer,
        deadline,
        move |request| {
            let era = mcp_wire::classify_request(request)?;
            if request.method == "tools/call" {
                mcp_wire::validate_tool_call(
                    request,
                    request.id.clone().unwrap_or_default(),
                    era,
                    &inventory,
                )?;
            }
            Ok(())
        },
        move |request, id| {
            let socket = socket.clone();
            let session = session.clone();
            let inventory = handler_inventory.clone();
            let session_context = (!daemon_observes_tool_calls)
                .then(|| {
                    request.tool_call().ok().and_then(|call| {
                        let known_tool = proxy_knows_tool(&inventory, &call.name);
                        cua_driver_core::session::begin_tool_call(
                            &call.name,
                            &call.args,
                            known_tool,
                            cua_driver_core::session::SessionTransport::McpStdio,
                            cua_driver_core::session::SessionClientKind::Mcp,
                        )
                    })
                })
                .flatten();
            let timer = (!daemon_observes_tool_calls)
                .then(|| {
                    tool_observation_timer(
                        &request,
                        |name| proxy_knows_tool(&inventory, name),
                        StdioExecutionPath::DaemonProxy,
                    )
                })
                .flatten();
            let mut telemetry = ToolCallTelemetry {
                timer,
                session_context,
                id: id.clone(),
            };
            async move {
                let response = handle_proxy_request(
                    request,
                    id,
                    &socket,
                    &inventory,
                    &session,
                    daemon_observes_tool_calls,
                )
                .await;
                telemetry.finish(&response);
                response
            }
        },
    )
    .await
}

fn proxy_knows_tool(cached_tools_list: &serde_json::Value, name: &str) -> bool {
    if name == "type_text_chars" || cua_driver_core::tool::renamed_tool(name).is_some() {
        return true;
    }
    cached_tools_list
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("name").and_then(serde_json::Value::as_str) == Some(name))
        })
}

/// Own the proxy's single long-lived control connection. Connects directly to
/// the daemon socket (its OWN async open — `send_request` is sync, blocking,
/// and one-shot, so it cannot be reused here), sends one `session_begin` line
/// carrying `session_id`, then parks in a read loop until the connection
/// closes. It never writes again. The daemon records `session_id` from
/// `session_begin` and fires `session_end` when this connection EOFs — which
/// the kernel triggers on proxy exit AND on kill -9.
///
/// On EOF/error (daemon-side close, broken pipe), this future completes and
/// its supervisor closes the MCP proxy. Per-call connections must never keep
/// serving the ended identity. Startup connection failures follow the same path.
async fn run_control_connection(
    socket_path: String,
    session_id: String,
    control_ready: tokio::sync::oneshot::Sender<()>,
) {
    let begin = DaemonRequest {
        method: "session_begin".into(),
        name: None,
        args: None,
        session_id: Some(session_id.clone()),
        observation_origin: None,
        client_kind: None,
    };
    let line = match serde_json::to_string(&begin) {
        Ok(s) => s + "\n",
        Err(e) => {
            warn!("control connection: serialize session_begin failed: {e}");
            return;
        }
    };

    #[cfg(unix)]
    {
        use tokio::net::UnixStream;
        // Retry the connect briefly — the daemon may still be spinning up
        // (mirrors the windows pipe-open retry below). The is_daemon_listening
        // precheck makes the window tiny, but keep both paths symmetric.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut stream = loop {
            match UnixStream::connect(&socket_path).await {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => {
                    debug!(session_id = %session_id, "control connect failed (daemon starting?): {e}");
                    return;
                }
            }
        };
        if let Err(e) = stream.write_all(line.as_bytes()).await {
            debug!("control connection: write session_begin failed: {e}");
            return;
        }
        let _ = stream.flush().await;
        debug!(session_id = %session_id, "control connection established (session_begin sent)");

        // Park: read until the daemon closes (it ACKs session_begin then keeps
        // the conn open; we drain anything and only return on EOF/error). The
        // proxy never writes here again — the connection lives until process
        // death, when the kernel closes it and the daemon reaps the session.
        let mut reader = BufReader::new(stream);
        let mut buf = String::new();
        match reader.read_line(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if let Err(error) = validate_session_begin_ack(&buf) {
                    warn!("control connection: {error}");
                    return;
                }
                let _ = control_ready.send(());
            }
        }
        loop {
            buf.clear();
            match reader.read_line(&mut buf).await {
                Ok(0) | Err(_) => break, // daemon closed or error — task done.
                Ok(_) => continue,       // ACK / stray line — ignore, keep parked.
            }
        }
        debug!(session_id = %session_id, "control connection closed");
    }

    #[cfg(all(not(unix), target_os = "windows"))]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        // Retry the pipe open briefly — the daemon may still be spinning up its
        // next instance (mirrors send_request's open-retry).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let client = loop {
            match ClientOptions::new().open(&socket_path) {
                Ok(c) => break Some(c),
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => {
                    debug!(session_id = %session_id, "control pipe open failed (daemon starting?): {e}");
                    break None;
                }
            }
        };
        let mut client = match client {
            Some(c) => c,
            None => return,
        };
        if let Err(e) = client.write_all(line.as_bytes()).await {
            debug!("control connection: write session_begin failed: {e}");
            return;
        }
        let _ = client.flush().await;
        debug!(session_id = %session_id, "control connection established (session_begin sent)");

        let mut reader = BufReader::new(client);
        let mut buf = String::new();
        match reader.read_line(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if let Err(error) = validate_session_begin_ack(&buf) {
                    warn!("control connection: {error}");
                    return;
                }
                let _ = control_ready.send(());
            }
        }
        loop {
            buf.clear();
            match reader.read_line(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
        debug!(session_id = %session_id, "control connection closed");
    }

    #[cfg(all(not(unix), not(target_os = "windows")))]
    {
        let _ = (line, session_id, socket_path, control_ready);
    }
}

fn validate_session_begin_ack(line: &str) -> Result<(), String> {
    let response: DaemonResponse = serde_json::from_str(line)
        .map_err(|_| "malformed session_begin acknowledgement".to_owned())?;
    if !response.ok
        || response
            .result
            .as_ref()
            .and_then(|result| result.get("session_begin"))
            != Some(&serde_json::Value::Bool(true))
    {
        return Err("daemon did not acknowledge session_begin".to_owned());
    }
    Ok(())
}

/// Mint a session id unique among the live proxies sharing one daemon, for the
/// lifetime of this proxy process. `pid + process-start nanos` is dep-free and
/// sufficient: two proxies can't share a pid concurrently, and the nanos guard
/// disambiguates pid reuse across the daemon's lifetime. We deliberately avoid
/// the `uuid` crate — a single v4 mint isn't worth a new dependency.
fn mint_session_id() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("mcp-{pid}-{nanos}")
}

/// One-shot daemon `list` over the UDS, reshaped into a MCP
/// `tools/list` result. The daemon now returns the full ToolDef
/// (`name`, `description`, `input_schema`, annotation hints) per
/// commit 3's `serve.rs` change.
fn fetch_tools_list_from_daemon(
    socket_path: &str,
    session_id: &str,
) -> anyhow::Result<(serde_json::Value, bool)> {
    let req = DaemonRequest {
        method: "list".into(),
        name: None,
        args: None,
        session_id: Some(session_id.to_owned()),
        observation_origin: None,
        client_kind: None,
    };
    let resp = send_request(socket_path, &req)?;
    if !resp.ok {
        anyhow::bail!(
            "daemon refused tool list on {socket_path}: {}",
            resp.error.unwrap_or_else(|| "(no error message)".into())
        );
    }
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("daemon list response missing `result` field"))?;
    let tools_array = result
        .get("tools")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("daemon list response missing `tools` array"))?;

    validate_daemon_output_schemas(tools_array)?;

    // Reshape the daemon's `{name, description, input_schema, output_schema,
    // read_only, ..., capabilities}` envelope into MCP's `{name, description,
    // inputSchema, outputSchema, annotations: {...}, capabilities}` shape.
    // Same translation `ToolDef::to_list_entry` defines for the core protocol.
    //
    // `capabilities` is passed through verbatim when the daemon
    // provides it; older daemons that don't emit the field fall back
    // to a name-keyed lookup so the proxy still surfaces capability
    // metadata without an extra round-trip.
    let mcp_tools: Vec<serde_json::Value> = tools_array
        .iter()
        .map(|t| {
            let name = t.get("name").cloned().unwrap_or(serde_json::Value::Null);
            let description = t
                .get("description")
                .cloned()
                .unwrap_or(serde_json::Value::String(String::new()));
            let input_schema = t
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
            let read_only = t
                .get("read_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let destructive = t
                .get("destructive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let idempotent = t
                .get("idempotent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let open_world = t
                .get("open_world")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let capabilities = t
                .get("capabilities")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_else(|| {
                    // Fallback: derive from the centralised name + schema
                    // resolver. Keeps the proxy compatible with daemon
                    // builds that pre-date the capabilities field.
                    name.as_str()
                        .map(|name| {
                            cua_driver_core::tool::advertised_capabilities_for(name, &input_schema)
                        })
                        .unwrap_or_default()
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect()
                });
            let risk = t.get("risk").cloned().unwrap_or_else(|| {
                name.as_str()
                    .map(cua_driver_core::authorization::risk_metadata_json)
                    .unwrap_or_else(|| {
                        serde_json::json!({
                            "class": "unclassified",
                            "enforcement": "metadata_only",
                            "operation_sensitive": false,
                            "version": cua_driver_core::authorization::RISK_METADATA_VERSION,
                        })
                    })
            });
            let mut tool = serde_json::json!({
                "name": name,
                "description": description,
                "inputSchema": input_schema,
                "annotations": {
                    "readOnlyHint": read_only,
                    "destructiveHint": destructive,
                    "idempotentHint": idempotent,
                    "openWorldHint": open_world,
                },
                "capabilities": capabilities,
                "risk": risk,
            });
            // Do not derive a new schema when an older daemon omitted it:
            // mixed-version proxies must advertise only the result contract
            // that the executing daemon actually owns.
            if let Some(output_schema) = t.get("output_schema") {
                tool.as_object_mut()
                    .expect("MCP tool entry is an object")
                    .insert("outputSchema".into(), output_schema.clone());
            }
            tool
        })
        .collect();

    // `capability_version` and `schema_version` are passed through
    // when the daemon emits them; older daemons fall back to the
    // proxy's compiled-in `CAPABILITY_VERSION` so MCP clients always
    // see the envelope keys.
    let capability_version = result
        .get("capability_version")
        .cloned()
        .unwrap_or_else(|| {
            serde_json::Value::String(cua_driver_core::tool::CAPABILITY_VERSION.to_owned())
        });
    let schema_version = result.get("schema_version").cloned().unwrap_or_else(|| {
        serde_json::Value::String(cua_driver_core::tool::TOOLS_LIST_SCHEMA_VERSION.to_owned())
    });

    let daemon_observes_tool_calls = daemon_owns_tool_observation(&result);

    Ok((
        serde_json::json!({
            "tools": mcp_tools,
            "capability_version": capability_version,
            "schema_version": schema_version,
        }),
        daemon_observes_tool_calls,
    ))
}

fn validate_daemon_output_schemas(tools: &[serde_json::Value]) -> anyhow::Result<()> {
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("daemon tool entry missing name"))?;
        cua_driver_core::mcp_result::validate_proxy_output_schema(name, tool.get("output_schema"))
            .map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

fn proxy_output_schema<'a>(
    tools: &'a serde_json::Value,
    name: &str,
) -> Option<&'a serde_json::Value> {
    let tools = tools.get("tools")?.as_array()?;
    tools
        .iter()
        .find(|tool| tool.get("name").and_then(serde_json::Value::as_str) == Some(name))
        .or_else(|| {
            let current = if name == "type_text_chars" {
                Some("type_text")
            } else {
                cua_driver_core::tool::renamed_tool(name)
            }?;
            tools
                .iter()
                .find(|tool| tool.get("name").and_then(serde_json::Value::as_str) == Some(current))
        })?
        .get("outputSchema")
}

fn proxy_tool_result_response(
    id: serde_json::Value,
    result: Result<serde_json::Value, String>,
) -> Response {
    match result {
        Ok(result) => Response::ok(id, result),
        Err(error) => Response::error(id, -32603, error),
    }
}

fn daemon_owns_tool_observation(result: &serde_json::Value) -> bool {
    result
        .get("tool_observation_owner")
        .and_then(serde_json::Value::as_str)
        == Some("daemon")
}

/// JSON-RPC method dispatcher for the proxy. Mirrors
/// `cua_driver_core::server::handle_request`:
/// - `initialize` → static `initialize_result()` (same envelope as the core
///   protocol server; the daemon's identity is hidden from the MCP client).
/// - `tools/list` → return the cached daemon tool list.
/// - `tools/call` → forward to the daemon and reshape the response into MCP's
///   `CallTool.Result`.
/// - other → method-not-found.
async fn handle_proxy_request(
    req: Request,
    id: serde_json::Value,
    socket_path: &str,
    cached_tools_list: &Arc<serde_json::Value>,
    session_id: &str,
    daemon_observes_tool_calls: bool,
) -> Response {
    let era = match mcp_wire::classify_request(&req) {
        Ok(era) => era,
        Err(response) => return response,
    };
    if let Err(response) = mcp_wire::validate_tool_call(&req, id.clone(), era, cached_tools_list) {
        return response;
    }
    let method = req.method.clone();
    let response = if let Some(response) = mcp_wire::handle_metadata_request(&req, id.clone()) {
        response
    } else {
        handle_proxy_tool_request(
            req,
            id,
            socket_path,
            cached_tools_list,
            session_id,
            daemon_observes_tool_calls,
        )
        .await
    };
    mcp_wire::finish_response(era, &method, response)
}

async fn handle_proxy_tool_request(
    req: Request,
    id: serde_json::Value,
    socket_path: &str,
    cached_tools_list: &Arc<serde_json::Value>,
    session_id: &str,
    daemon_observes_tool_calls: bool,
) -> Response {
    match req.method.as_str() {
        "initialize"
            if mcp_wire::classify_request(&req).ok() == Some(mcp_wire::ProtocolEra::Legacy) =>
        {
            Response::ok(id, initialize_result())
        }

        "tools/list" => Response::ok(id, (**cached_tools_list).clone()),

        "tools/call" => match req.tool_call() {
            Err(e) => Response::error(id, -32602, format!("Invalid params: {e}")),
            Ok(call) => {
                if let Err(error) = authorize_tool_call(&call.name, &call.args) {
                    return proxy_tool_result_response(
                        id,
                        cua_driver_core::mcp_result::conforming_proxy_tool_result(
                            &call.name,
                            cua_driver_core::mcp_result::tool_error_result(
                                error.to_string(),
                                // Constructed locally before any daemon call; never copied from tool JSON.
                                serde_json::json!({"code": "permission_denied", "disposition": cua_driver_contract::ErrorDisposition::NOT_DELIVERED}),
                            ),
                            proxy_output_schema(cached_tools_list, &call.name),
                        ),
                    );
                }
                forward_tool_call(
                    id,
                    call.name.clone(),
                    call.args,
                    socket_path,
                    session_id,
                    daemon_observes_tool_calls,
                    proxy_output_schema(cached_tools_list, &call.name).cloned(),
                )
                .await
            }
        },

        other => {
            warn!(method = other, "unknown method");
            Response::method_not_found(id, other)
        }
    }
}

/// Forward a single MCP `tools/call` to the daemon as a `call`
/// request, then translate the `DaemonResponse` back into an MCP
/// `CallTool.Result` envelope.
///
/// Error mapping:
///   - Tool ran and reported failure (`!resp.ok`, including unknown
///     tool / bad params) → JSON-RPC success with `result.isError =
///     true`. Mirrors the core protocol's tool-error envelope.
///   - Transport failure (UDS unreachable, decode error, blocking
///     task panic) → JSON-RPC error (`-32603`), because the MCP
///     client really does need to distinguish "tool said no" from
///     "I couldn't reach the tool at all."
async fn forward_tool_call(
    id: serde_json::Value,
    name: String,
    mut args: serde_json::Value,
    socket_path: &str,
    session_id: &str,
    daemon_observes_tool_calls: bool,
    output_schema: Option<serde_json::Value>,
) -> Response {
    let permit = match DAEMON_FORWARD_PERMITS.try_acquire() {
        Ok(permit) => permit,
        Err(_) => return Response::error(id, -32029, "in_flight_limit"),
    };
    cua_driver_core::tool_args::sanitize_reserved_args(&mut args);
    let req = DaemonRequest {
        method: "call".into(),
        name: Some(name.clone()),
        args: Some(args),
        session_id: Some(session_id.to_owned()),
        observation_origin: daemon_observes_tool_calls.then_some(ToolObservationOrigin::McpProxy),
        client_kind: None,
    };

    // The daemon client is sync, so jump to a blocking thread to keep
    // the tokio reactor responsive while the AX-heavy call (e.g.
    // `screenshot`, `get_window_state`) does its thing on the daemon
    // side.
    let socket = socket_path.to_owned();
    #[cfg(unix)]
    let blocking = {
        // The per-call deadline drops this future; the guard then shuts the
        // daemon socket so the blocking request ends with it and frees its
        // in-flight slot, instead of waiting out the daemon client's longer
        // reply deadline.
        let abandon = AbandonDaemonCallOnDrop::default();
        let slot = abandon.0.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let stream = std::os::unix::net::UnixStream::connect(&socket)
                .map_err(|error| anyhow::anyhow!("connect to {socket}: {error}"))?;
            {
                let mut slot = slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if slot.abandoned {
                    anyhow::bail!("the call was abandoned before it reached the daemon");
                }
                slot.stream = stream.try_clone().ok();
            }
            cua_driver_core::daemon::send_request_on(stream, &req)
        })
        .await
    };
    // The overlapped pipe client lives in this future, so the per-call
    // deadline dropping it closes the pipe and frees the in-flight slot.
    #[cfg(windows)]
    let blocking = {
        let _permit = permit;
        Ok::<_, tokio::task::JoinError>(
            cua_driver_core::daemon::send_request_async(&socket, &req).await,
        )
    };
    #[cfg(not(any(unix, windows)))]
    let blocking = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        send_request(&socket, &req)
    })
    .await;

    let resp = match blocking {
        Err(join_err) => {
            return Response::error(
                id,
                -32603,
                format!("internal join error forwarding to daemon: {join_err}"),
            );
        }
        Ok(Err(e)) => {
            return Response::error(
                id,
                -32603,
                format!("daemon transport error forwarding `{name}`: {e}"),
            );
        }
        Ok(Ok(r)) => r,
    };

    proxy_tool_result_response(
        id,
        daemon_response_to_tool_result(&name, resp, output_schema.as_ref()),
    )
}

#[cfg(unix)]
#[derive(Default)]
struct DaemonCallSlot {
    abandoned: bool,
    stream: Option<std::os::unix::net::UnixStream>,
}

/// Held by a forwarded call's future. Dropping it, whether the call finished
/// or the deadline dropped it, marks the call abandoned and shuts its daemon
/// socket down, which ends a blocking read still waiting for a reply.
#[cfg(unix)]
#[derive(Default)]
struct AbandonDaemonCallOnDrop(Arc<std::sync::Mutex<DaemonCallSlot>>);

#[cfg(unix)]
impl Drop for AbandonDaemonCallOnDrop {
    fn drop(&mut self) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.abandoned = true;
        if let Some(stream) = slot.stream.take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Translate one `DaemonResponse` into the `CallTool.Result` the MCP client
/// receives.
///
/// MCP separates two failure modes:
///   - JSON-RPC errors → `Response::error(...)`, used for transport /
///     protocol failures (unknown method, bad params shape, server crash).
///     Those are handled by the caller, above.
///   - Tool-level errors → `Response::ok(...)` carrying a `CallTool.Result`
///     with `isError: true` and the error message in `content[]`. The tool
///     ran, returned a well-formed result that says "I failed."
///
/// A non-`ok` daemon response means the tool call reached the daemon and the
/// daemon decided the tool returned an error (or rejected the call). That's
/// tool-level, not transport-level, so the core protocol surfaces it as
/// `Response::ok` with `isError: true`. Mirror that shape here — CodeRabbit #2.
///
/// Every branch — including the empty result synthesised for an `ok` response
/// that carries none — leaves through the same boundary the direct dispatch
/// uses, so a daemon-backed call is held to the tool's advertised
/// `outputSchema` exactly as a direct one is.
fn daemon_response_to_tool_result(
    name: &str,
    resp: DaemonResponse,
    output_schema: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let result = if resp.ok {
        resp.result.unwrap_or_else(|| {
            serde_json::json!({
                "content": [],
                "isError": false
            })
        })
    } else {
        let msg = resp
            .error
            .unwrap_or_else(|| "daemon reported failure".into());
        let exit_code = resp.exit_code.unwrap_or(1);
        cua_driver_core::mcp_result::tool_error_result(
            msg,
            serde_json::json!({ "exit_code": exit_code }),
        )
    };
    cua_driver_core::mcp_result::conforming_proxy_tool_result(name, result, output_schema)
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// The daemon-backed integration harness exercises the full proxy lifecycle.
// These tests lock in the I/O loop's transport contract and the per-branch
// response reshaping without requiring a live daemon.

#[cfg(test)]
mod tests {
    use super::*;

    fn modern_request(id: u64, method: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": {
                "_meta": {
                    mcp_wire::PROTOCOL_VERSION_KEY: mcp_wire::MODERN_PROTOCOL_VERSION,
                    mcp_wire::CLIENT_CAPABILITIES_KEY: {}
                }
            }
        })
        .to_string()
            + "\n"
    }

    fn modern_tool_call(id: u64, name: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "_meta": {
                    mcp_wire::PROTOCOL_VERSION_KEY: mcp_wire::MODERN_PROTOCOL_VERSION,
                    mcp_wire::CLIENT_CAPABILITIES_KEY: {}
                },
                "name": name,
                "arguments": {}
            }
        })
        .to_string()
            + "\n"
    }

    async fn next_response(
        reader: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    ) -> serde_json::Value {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .expect("response deadline")
            .expect("read response");
        assert!(n > 0, "unexpected transport EOF");
        serde_json::from_str(line.trim()).expect("valid JSON response")
    }

    #[tokio::test]
    async fn concurrent_transport_times_out_one_request_without_blocking_the_next() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let run = tokio::spawn(run_concurrent_mcp_io(
            BufReader::new(server_read),
            server_write,
            Duration::from_millis(40),
            |_| Ok(()),
            |request, id| async move {
                if request.method == "tools/call" {
                    std::future::pending::<()>().await;
                }
                Response::ok(id, serde_json::json!({"ok": true}))
            },
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        client_write
            .write_all((modern_tool_call(1, "slow") + &modern_request(2, "fast")).as_bytes())
            .await
            .unwrap();
        let first = next_response(&mut client_read).await;
        let second = next_response(&mut client_read).await;
        let replies = [first, second];
        assert!(replies.iter().any(|reply| reply["id"] == 2));
        assert!(replies.iter().any(|reply| {
            reply["id"] == 1
                && reply["result"]["isError"] == true
                && reply["result"]["structuredContent"]["effect"] == "unknown"
        }));
        client_write.shutdown().await.unwrap();
        drop(client_write);
        run.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancelled_request_gets_one_unknown_effect_response_and_followup_runs() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let run = tokio::spawn(run_concurrent_mcp_io(
            BufReader::new(server_read),
            server_write,
            Duration::from_secs(1),
            |_| Ok(()),
            |request, id| async move {
                if request.method == "tools/call" {
                    std::future::pending::<()>().await;
                }
                Response::ok(id, serde_json::json!({"ok": true}))
            },
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        client_write
            .write_all(modern_tool_call(7, "never_finishes").as_bytes())
            .await
            .unwrap();
        client_write
            .write_all(
                br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}
"#,
            )
            .await
            .unwrap();
        client_write
            .write_all(modern_request(8, "fast").as_bytes())
            .await
            .unwrap();
        let cancelled = next_response(&mut client_read).await;
        let followup = next_response(&mut client_read).await;
        let replies = [cancelled, followup];
        assert!(replies.iter().any(|reply| {
            reply["id"] == 7
                && reply["result"]["isError"] == true
                && reply["result"]["structuredContent"]["effect"] == "unknown"
        }));
        assert!(replies.iter().any(|reply| reply["id"] == 8));
        let mut unexpected = String::new();
        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            client_read.read_line(&mut unexpected)
        )
        .await
        .is_err());
        client_write.shutdown().await.unwrap();
        drop(client_write);
        run.await.unwrap().unwrap();
    }

    include!("proxy_key_cancel_tests.rs");

    #[tokio::test]
    async fn duplicate_pending_request_id_does_not_replace_original() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let started = Arc::new(tokio::sync::Notify::new());
        let handler_started = started.clone();
        let run = tokio::spawn(run_concurrent_mcp_io(
            BufReader::new(server_read),
            server_write,
            Duration::from_secs(5),
            |_| Ok(()),
            move |request, id| {
                let handler_started = handler_started.clone();
                async move {
                    if request.method == "slow" {
                        handler_started.notify_one();
                        std::future::pending::<()>().await;
                    }
                    Response::ok(id, serde_json::json!({"ok": true}))
                }
            },
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        let started_signal = started.notified();
        client_write
            .write_all(modern_request(10, "slow").as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), started_signal)
            .await
            .expect("original request started");
        client_write
            .write_all(modern_request(10, "fast").as_bytes())
            .await
            .unwrap();
        let duplicate = next_response(&mut client_read).await;
        assert_eq!(duplicate["error"]["code"], -32600);
        assert_eq!(duplicate["id"], 10);
        client_write
            .write_all(
                br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":10}}
"#,
            )
            .await
            .unwrap();
        let original = next_response(&mut client_read).await;
        assert_eq!(original["error"]["code"], -32800);
        assert_eq!(original["id"], 10);
        client_write.shutdown().await.unwrap();
        drop(client_write);
        run.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn panicked_request_gets_one_internal_error_and_followup_runs() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let run = tokio::spawn(run_concurrent_mcp_io(
            BufReader::new(server_read),
            server_write,
            Duration::from_secs(5),
            |_| Ok(()),
            |request, id| async move {
                assert_ne!(request.method, "panic", "handler panic");
                Response::ok(id, serde_json::json!({"ok": true}))
            },
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        client_write
            .write_all((modern_request(11, "panic") + &modern_request(12, "fast")).as_bytes())
            .await
            .unwrap();
        let first = next_response(&mut client_read).await;
        let second = next_response(&mut client_read).await;
        let replies = [first, second];
        assert!(replies
            .iter()
            .any(|reply| { reply["id"] == 11 && reply["error"]["code"] == -32603 }));
        assert!(replies.iter().any(|reply| reply["id"] == 12));
        client_write.shutdown().await.unwrap();
        drop(client_write);
        run.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn eof_aborts_pending_transport_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct DropProbe(Arc<AtomicUsize>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let probe = drops.clone();
        let handler_started = started.clone();
        let run = tokio::spawn(run_concurrent_mcp_io(
            BufReader::new(server_read),
            server_write,
            Duration::from_secs(30),
            |_| Ok(()),
            move |request, id| {
                let probe = (request.method == "slow").then(|| DropProbe(probe.clone()));
                let handler_started = handler_started.clone();
                async move {
                    let _probe = probe;
                    if request.method == "slow" {
                        handler_started.notify_one();
                        std::future::pending::<()>().await;
                    }
                    Response::ok(id, serde_json::json!({"ok": true}))
                }
            },
        ));
        let (_client_read, mut client_write) = tokio::io::split(client);
        let started_signal = started.notified();
        client_write
            .write_all(modern_request(9, "slow").as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), started_signal)
            .await
            .expect("pending handler started");
        client_write.shutdown().await.unwrap();
        drop(client_write);
        run.await.unwrap().unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_transport_rejects_requests_over_the_inflight_cap() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let run = tokio::spawn(run_concurrent_mcp_io(
            BufReader::new(server_read),
            server_write,
            Duration::from_secs(30),
            |_| Ok(()),
            |_request, _id| async move {
                std::future::pending::<()>().await;
                unreachable!()
            },
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        let input = (1..=17)
            .map(|id| modern_request(id, "slow"))
            .collect::<String>();
        client_write.write_all(input.as_bytes()).await.unwrap();
        let response = next_response(&mut client_read).await;
        assert_eq!(response["id"], 17);
        assert_eq!(response["error"]["code"], -32029);
        client_write.shutdown().await.unwrap();
        drop(client_write);
        run.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn modern_proxy_metadata_and_rejections_need_no_daemon_call() {
        use serde_json::json;
        let meta = json!({
            mcp_wire::PROTOCOL_VERSION_KEY: mcp_wire::MODERN_PROTOCOL_VERSION,
            mcp_wire::CLIENT_CAPABILITIES_KEY: {}
        });
        let mut input = Vec::new();
        for (id, method, mut params) in [
            (1, "server/discover", json!({})),
            (2, "skills/list", json!({})),
            (
                3,
                "resources/read",
                json!({"uri":"skill://cua-driver/SKILL.md"}),
            ),
            (4, "tools/call", json!({"name":"get_config","arguments":[]})),
            (
                5,
                "tools/call",
                json!({"name":"unknown_tool","arguments":{"session":"must-not-start"}}),
            ),
        ] {
            params["_meta"] = meta.clone();
            serde_json::to_writer(
                &mut input,
                &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            )
            .unwrap();
            input.push(b'\n');
        }
        let inventory = Arc::new(json!({"tools":[{"name":"get_config"}]}));
        let mut output = Vec::new();
        run_proxy_io(
            BufReader::new(input.as_slice()),
            &mut output,
            "unreachable-test-endpoint",
            &inventory,
            "modern-test",
            false,
        )
        .await
        .unwrap();
        let replies: Vec<serde_json::Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replies.len(), 5);
        assert_eq!(
            replies[0]["result"]["supportedVersions"],
            json!(["2026-07-28"])
        );
        assert_eq!(
            replies[1]["result"]["skills"][0]["uri"],
            "skill://cua-driver/SKILL.md"
        );
        assert!(replies[2]["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("---"));
        for reply in &replies[..3] {
            assert_eq!(reply["result"]["resultType"], "complete");
            assert_eq!(reply["result"]["cacheScope"], "private");
        }
        for reply in &replies[3..] {
            assert_eq!(reply["error"]["code"], -32602);
        }
    }

    #[tokio::test]
    async fn proxy_authorization_refusal_is_a_tool_error_without_forwarding() {
        const CHILD_ENV: &str = "CUA_DRIVER_PROXY_REFUSAL_TEST_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            // Policy configuration is process-wide and immutable; keep this
            // denial out of other tests without mutating the parent environment.
            let directory = tempfile::tempdir().expect("temporary policy directory");
            let policy = directory.path().join("deny.yaml");
            std::fs::write(&policy, "deny:\n  tools: [click]\n").expect("write policy");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "proxy::tests::proxy_authorization_refusal_is_a_tool_error_without_forwarding",
                    "--nocapture",
                ])
                .env(CHILD_ENV, "1")
                .env(cua_driver_core::policy::POLICY_FILE_ENV, &policy)
                .env_remove(cua_driver_core::policy::MANAGED_POLICY_FILE_ENV)
                .output()
                .expect("run isolated refusal test");
            assert!(
                output.status.success(),
                "isolated refusal test failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let directory = tempfile::tempdir().expect("temporary socket directory");
        let socket = directory.path().join("daemon.sock");
        #[cfg(unix)]
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind test daemon");
        let request = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0", "id": 19, "method": "tools/call",
            "params": {"name": "click", "arguments": {"pid": 42, "x": 10, "y": 20}}
        }))
        .expect("valid tool request");
        let cached_tools = Arc::new(serde_json::json!({"tools": [{"name": "click"}]}));
        let response = handle_proxy_request(
            request,
            serde_json::json!(19),
            socket.to_str().unwrap(),
            &cached_tools,
            "refusal-test-session",
            false,
        );
        #[cfg(unix)]
        let response = tokio::select! {
            response = response => response,
            connection = listener.accept() => {
                drop(connection);
                panic!("a locally denied call must never connect to the daemon");
            }
        };
        #[cfg(not(unix))]
        let response = response.await;
        let value = serde_json::to_value(response).expect("serialize response");
        assert_eq!(value["id"], 19);
        assert!(value.get("error").is_none(), "{value}");
        let result = &value["result"];
        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["code"], "permission_denied");
        assert_eq!(
            result["structuredContent"]["disposition"],
            serde_json::json!({"delivery":"not_delivered", "retry":"safe"})
        );
        assert_eq!(result["content"][0]["type"], "text");
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("explicitly denied"));
    }

    #[test]
    fn control_disconnect_exits_with_stdin_open() {
        const CHILD_ENV: &str = "CUA_TEST_MCP_CONTROL_STDIN_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            let result = crate::cli::run_mcp_runtime(async {
                let cached_tools = Arc::new(serde_json::json!({"tools": []}));
                supervise_control_connection(
                    // Let run_proxy_io park in the real blocking stdin read.
                    tokio::time::sleep(std::time::Duration::from_millis(100)),
                    run_proxy_io(
                        BufReader::new(tokio::io::stdin()),
                        tokio::io::sink(),
                        "unused.sock",
                        &cached_tools,
                        "disconnect",
                        false,
                    ),
                )
                .await
            });
            assert!(result.is_err());
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "proxy::tests::control_disconnect_exits_with_stdin_open",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "MCP runtime subprocess failed: {:?}",
                    child.wait_with_output().unwrap()
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("control disconnect left the MCP process waiting for client stdin EOF");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn control_disconnect_ends_idle_proxy() {
        let (control, daemon) = tokio::io::duplex(64);
        let control = async move {
            let mut reader = BufReader::new(control);
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await;
        };
        let proxy =
            supervise_control_connection(control, std::future::pending::<anyhow::Result<()>>());
        tokio::pin!(proxy);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut proxy)
                .await
                .is_err(),
            "an idle live control connection must remain usable"
        );
        drop(daemon);
        let error = tokio::time::timeout(std::time::Duration::from_millis(250), proxy)
            .await
            .expect("control EOF must end the proxy promptly")
            .unwrap_err();
        assert!(error.to_string().contains("reconnect the MCP client"));
    }

    #[tokio::test]
    async fn proxy_eof_drops_its_control_connection() {
        let (control, daemon) = tokio::io::duplex(64);
        let control = async move {
            let mut reader = BufReader::new(control);
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await;
        };
        let cached_tools = Arc::new(serde_json::json!({"tools": []}));
        let mut writer = Vec::new();
        supervise_control_connection(
            control,
            run_proxy_io(
                BufReader::new(&b""[..]),
                &mut writer,
                "unused.sock",
                &cached_tools,
                "eof",
                false,
            ),
        )
        .await
        .unwrap();
        let mut daemon = BufReader::new(daemon);
        let mut line = String::new();
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_millis(250),
                daemon.read_line(&mut line)
            )
            .await
            .expect("proxy EOF must release daemon session ownership")
            .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn closed_control_prevents_ready_request_dispatch() {
        let dispatched = std::sync::atomic::AtomicBool::new(false);
        let result = supervise_control_connection(async {}, async {
            dispatched.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(result.is_err());
        assert!(!dispatched.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn proxy_loop_returns_promptly_on_clean_eof() {
        let reader = BufReader::new(&b""[..]);
        let mut writer = Vec::new();
        let cached_tools = Arc::new(serde_json::json!({"tools": []}));

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            run_proxy_io(
                reader,
                &mut writer,
                "unused.sock",
                &cached_tools,
                "eof-test-session",
                false,
            ),
        )
        .await
        .expect("clean EOF must return promptly");

        assert!(result.is_ok(), "clean EOF must not error: {result:?}");
        assert!(writer.is_empty(), "no request means no output");
    }

    #[tokio::test]
    async fn proxy_loop_serves_initialize_before_eof() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n";
        let reader = BufReader::new(&input[..]);
        let mut writer = Vec::new();
        let cached_tools = Arc::new(serde_json::json!({"tools": []}));

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            run_proxy_io(
                reader,
                &mut writer,
                "unused.sock",
                &cached_tools,
                "initialize-test-session",
                false,
            ),
        )
        .await
        .expect("initialize followed by EOF must return promptly");

        assert!(
            result.is_ok(),
            "initialize exchange must not error: {result:?}"
        );
        let response: serde_json::Value =
            serde_json::from_slice(&writer).expect("response must be JSON");
        assert_eq!(response["id"], 1);
        assert!(response.get("result").is_some());
    }

    #[test]
    fn session_begin_requires_a_positive_typed_acknowledgement() {
        assert!(
            validate_session_begin_ack(r#"{"ok":true,"result":{"session_begin":true}}"#).is_ok()
        );
        for line in [
            "not JSON",
            r#"{"ok":false,"error":"denied"}"#,
            r#"{"ok":false,"result":{"session_begin":true}}"#,
            r#"{"ok":true}"#,
            r#"{"ok":true,"result":{"session_begin":false}}"#,
            r#"{"ok":true,"result":{"session_begin":"true"}}"#,
            r#"{"ok":true,"result":{"other":true}}"#,
        ] {
            assert!(validate_session_begin_ack(line).is_err(), "accepted {line}");
        }
    }

    #[test]
    fn incompatible_daemon_schema_is_rejected_before_advertising() {
        let schema = serde_json::json!({"type": "object", "required": ["legacy_clicked"]});
        let tools = vec![serde_json::json!({"name": "click", "output_schema": schema})];
        let error = validate_daemon_output_schemas(&tools)
            .unwrap_err()
            .to_string();
        assert!(error.contains("incompatible daemon output schema for click"));
        assert!(error.contains("matching proxy and daemon versions"));

        let daemon_resp = DaemonResponse {
            ok: true,
            result: Some(serde_json::json!({
                "content": [], "structuredContent": {"legacy_clicked": true}
            })),
            error: None,
            exit_code: None,
        };
        let response = proxy_tool_result_response(
            serde_json::json!(1),
            daemon_response_to_tool_result("click", daemon_resp, Some(&schema)),
        );
        let response = serde_json::to_value(response).unwrap();
        assert_eq!(response["error"]["code"], -32603);
        assert!(response.get("result").is_none());
    }

    #[test]
    fn daemon_without_schema_preserves_legacy_success() {
        let tools = vec![serde_json::json!({"name": "click"})];
        validate_daemon_output_schemas(&tools).unwrap();
        for result in [
            serde_json::json!({"content": [], "isError": false}),
            serde_json::json!({
                "content": [], "isError": false,
                "structuredContent": {"legacy_clicked": true}
            }),
        ] {
            let daemon_resp = DaemonResponse {
                ok: true,
                result: Some(result.clone()),
                error: None,
                exit_code: None,
            };
            assert_eq!(
                daemon_response_to_tool_result("click", daemon_resp, None).unwrap(),
                result
            );
        }
    }

    #[test]
    fn matching_daemon_schema_retains_local_output_validation() {
        let tools =
            vec![serde_json::json!({"name": "click", "output_schema": click_output_schema()})];
        validate_daemon_output_schemas(&tools).unwrap();
    }

    fn click_output_schema() -> Option<serde_json::Value> {
        cua_driver_contract::advertised_tool_output_schema("click")
    }

    /// Serialize the production conversion the way the proxy answers it, so
    /// these assertions run against `forward_tool_call`'s own branch without
    /// spinning up a real daemon.
    fn tool_call_response(id: serde_json::Value, resp: DaemonResponse) -> Response {
        Response::ok(
            id,
            daemon_response_to_tool_result("click", resp, click_output_schema().as_ref()).unwrap(),
        )
    }

    #[test]
    fn daemon_tool_failure_wraps_as_jsonrpc_success_with_iserror_true() {
        let daemon_resp = DaemonResponse {
            ok: false,
            result: None,
            error: Some("missing required field `pid`".into()),
            exit_code: Some(64),
        };
        let resp = tool_call_response(serde_json::json!(7), daemon_resp);
        let value = serde_json::to_value(&resp).expect("serialize");

        // Top-level JSON-RPC envelope: success (`result`), not error.
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], serde_json::json!(7));
        assert!(
            value.get("error").is_none(),
            "tool-level failure must NOT surface as JSON-RPC error: got {value}"
        );
        assert!(
            value.get("result").is_some(),
            "tool-level failure must carry a `result` payload: got {value}"
        );

        // CallTool.Result inside `result`: isError + content text.
        let result = &value["result"];
        assert_eq!(result["isError"], serde_json::json!(true));
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], "missing required field `pid`");
        assert_eq!(result["structuredContent"]["exit_code"], 64);
        // Strict MCP clients validate this payload against the tool's
        // advertised `outputSchema`. The refusal marker is what keeps the
        // response readable instead of being discarded as `-32602`.
        assert_eq!(
            result["structuredContent"]["code"],
            cua_driver_core::TOOL_INVOCATION_FAILED_CODE
        );
    }

    #[test]
    fn daemon_failure_with_no_error_message_uses_fallback_text() {
        let daemon_resp = DaemonResponse {
            ok: false,
            result: None,
            error: None,
            exit_code: None,
        };
        let resp = tool_call_response(serde_json::json!("abc"), daemon_resp);
        let value = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(value["result"]["isError"], serde_json::json!(true));
        assert_eq!(
            value["result"]["content"][0]["text"],
            "daemon reported failure"
        );
        assert_eq!(value["result"]["structuredContent"]["exit_code"], 1);
        assert_eq!(
            value["result"]["structuredContent"]["code"],
            cua_driver_core::TOOL_INVOCATION_FAILED_CODE
        );
    }

    /// An `ok` daemon response that carries no result used to be forwarded as
    /// an empty success. For a tool that declares an `outputSchema` that is a
    /// response a strict client rejects outright (`-32600`), so the boundary
    /// converts it into an error the client can actually read.
    #[test]
    fn an_ok_daemon_response_with_no_result_becomes_a_conforming_error() {
        let daemon_resp = DaemonResponse {
            ok: true,
            result: None,
            error: None,
            exit_code: None,
        };

        let result =
            daemon_response_to_tool_result("click", daemon_resp, click_output_schema().as_ref())
                .unwrap();

        assert_eq!(result["isError"], serde_json::json!(true));
        assert_eq!(
            result["structuredContent"]["code"],
            cua_driver_core::mcp_result::TOOL_OUTPUT_INVALID_CODE
        );
    }

    /// A daemon result that already conforms is forwarded untouched.
    #[test]
    fn a_conforming_daemon_success_is_forwarded_unchanged() {
        let result = serde_json::json!({
            "content": [{ "type": "text", "text": "clicked" }],
            "isError": false,
            "structuredContent": {
                "effect": "confirmed",
                "route": "accessibility",
                "delivery": { "mode": "background" },
                "evidence": [{ "kind": "value_readback" }],
            },
        });
        let daemon_resp = DaemonResponse {
            ok: true,
            result: Some(result.clone()),
            error: None,
            exit_code: None,
        };

        assert_eq!(
            daemon_response_to_tool_result("click", daemon_resp, click_output_schema().as_ref())
                .unwrap(),
            result
        );
    }

    #[test]
    fn cached_proxy_tool_allowlist_is_exact() {
        let cached = serde_json::json!({
            "tools": [
                {"name":"click"},
                {"name":"type_text"}
            ]
        });
        assert!(proxy_knows_tool(&cached, "click"));
        assert!(
            proxy_knows_tool(&cached, "type_text_chars"),
            "deprecated alias stays bounded/known"
        );
        assert!(!proxy_knows_tool(&cached, "click/private-user-value"));
        assert!(!proxy_knows_tool(&cached, ""));
    }

    #[test]
    fn observation_ownership_requires_the_daemon_capability() {
        assert!(daemon_owns_tool_observation(&serde_json::json!({
            "tool_observation_owner": "daemon"
        })));
        assert!(!daemon_owns_tool_observation(&serde_json::json!({})));
        assert!(!daemon_owns_tool_observation(&serde_json::json!({
            "tool_observation_owner": "proxy"
        })));
    }

    /// When the proxy's per-call deadline drops a forwarded call, the
    /// blocking daemon request must end with it. Before, it kept its socket
    /// and an in-flight slot until the daemon client's own reply deadline
    /// (TIMEOUT_MS_MAX plus overhead), past the proxy's 110 s.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dropped_forwarded_call_releases_its_daemon_connection() {
        use std::io::{BufRead, Read};
        let directory = tempfile::Builder::new()
            .prefix("px")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = directory.path().join("d.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        // A daemon that reads the request and never answers.
        let daemon = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            reader
                .get_ref()
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut rest = [0_u8; 16];
            matches!(reader.read(&mut rest), Ok(0))
        });
        let call = forward_tool_call(
            serde_json::json!(1),
            "list_apps".into(),
            serde_json::json!({}),
            socket.to_str().unwrap(),
            "s",
            false,
            None,
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), call)
                .await
                .is_err(),
            "the silent daemon must hold the call past the deadline"
        );
        let closed = tokio::task::spawn_blocking(move || daemon.join().unwrap())
            .await
            .unwrap();
        assert!(
            closed,
            "the daemon connection must close once the proxy drops the call"
        );
    }

    /// Windows counterpart: the forwarded call used to run a synchronous pipe
    /// read on a blocking thread, which kept the pipe and its in-flight slot
    /// after the deadline dropped the call.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_dropped_forwarded_call_releases_its_daemon_pipe() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        let pipe_name = format!(
            r"\\.\pipe\cua-driver-proxy-drop-test-{}",
            std::process::id()
        );
        let server = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .unwrap();
        // A daemon that reads the request, reports it, and never answers.
        // It then waits for the proxy to close its end of the pipe.
        let (received_tx, received_rx) = tokio::sync::oneshot::channel();
        let mut daemon = tokio::spawn(async move {
            server.connect().await.unwrap();
            let mut reader = tokio::io::BufReader::new(server);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let _ = received_tx.send(request);
            let mut rest = [0_u8; 16];
            matches!(reader.read(&mut rest).await, Ok(0) | Err(_))
        });
        let mut call = Box::pin(forward_tool_call(
            serde_json::json!(1),
            "list_apps".into(),
            serde_json::json!({}),
            &pipe_name,
            "s",
            false,
            None,
        ));
        // Drop the call only once the daemon holds the whole request, so the
        // proxy is waiting on the reply.
        let received = tokio::select! {
            _ = &mut call => None,
            received = tokio::time::timeout(std::time::Duration::from_secs(10), received_rx) => {
                received.ok().and_then(Result::ok)
            }
        };
        let Some(request) = received else {
            // Dropping the server end also ends a blocking client read.
            daemon.abort();
            panic!("the daemon must receive the request and hold the call");
        };
        assert!(request.contains("list_apps"), "request: {request}");
        drop(call);
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), &mut daemon).await;
        if closed.is_err() {
            daemon.abort();
        }
        assert!(
            matches!(closed, Ok(Ok(true))),
            "the daemon pipe must close once the proxy drops the call"
        );
    }
}
