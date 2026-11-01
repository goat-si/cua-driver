// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! `cua-driver mcp` must refuse a daemon that does not speak this build's
//! protocol, promptly and without stopping a daemon it did not start
//! (goat-si/goat#1717: an older CuaDriver.app answered `Unknown method:
//! metadata` and the MCP client never received an initialize response).

#![cfg(unix)]

use cua_driver_testkit::{FakeDaemon, FakeReply, IsolatedStateRoot};
use std::io::Read as _;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// Hang guard only. The startup probe's own deadline is a few seconds; the
/// daemon client's reply deadline is minutes, so a missing startup bound
/// fails here instead of passing late.
const BOUND: Duration = Duration::from_secs(30);

fn legacy_daemon(method: &str) -> FakeReply {
    match method {
        "list" => FakeReply::ok(serde_json::json!({ "tools": [] })),
        other => FakeReply::unknown_method(other),
    }
}

/// Run `cua-driver mcp --socket <fake>` with stdin held open, as an MCP client
/// does while it waits for `initialize`.
fn run_mcp(socket: &str) -> (ExitStatus, String) {
    let state = IsolatedStateRoot::new().expect("isolated state root");
    let mut child = Command::new(env!("CARGO_BIN_EXE_cua-driver"))
        .args(["mcp", "--socket", socket])
        .envs(state.env())
        .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cua-driver mcp");
    let _stdin = child.stdin.take();
    let mut stderr = child.stderr.take().expect("stderr");
    let pid = child.id();
    let (exited_tx, exited) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        let _ = exited_tx.send((child.wait().expect("wait for mcp"), text));
    });
    match exited.recv_timeout(BOUND) {
        Ok(result) => result,
        Err(_) => {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            panic!("cua-driver mcp did not exit within {BOUND:?} against an incompatible daemon");
        }
    }
}

/// The fake must answer the listening probe, or a macOS run would fall back
/// to launching the installed app.
fn assert_listening(daemon: &FakeDaemon) {
    assert!(
        cua_driver_core::daemon::is_daemon_listening(daemon.socket()),
        "fake daemon must answer the listening probe"
    );
}

#[test]
fn legacy_running_daemon_is_refused_and_left_running() {
    let daemon = FakeDaemon::start(legacy_daemon);
    assert_listening(&daemon);

    let (status, stderr) = run_mcp(daemon.socket());

    assert!(!status.success(), "mcp must fail: {stderr}");
    assert!(stderr.contains("incompatible"), "{stderr}");
    assert!(stderr.contains("Unknown method: metadata"), "{stderr}");
    let methods = daemon.stop();
    assert!(
        methods.iter().any(|method| method == "metadata"),
        "{methods:?}"
    );
    assert!(
        !methods.iter().any(|method| method == "shutdown"),
        "a daemon this process did not start must not be stopped: {methods:?}"
    );
}

#[test]
fn daemon_that_never_answers_metadata_is_refused_within_the_startup_bound() {
    let daemon = FakeDaemon::start(|method| match method {
        "metadata" => FakeReply::Silent,
        other => legacy_daemon(other),
    });
    assert_listening(&daemon);

    let (status, stderr) = run_mcp(daemon.socket());

    assert!(!status.success(), "mcp must fail: {stderr}");
    assert!(stderr.contains("did not answer"), "{stderr}");
    let methods = daemon.stop();
    assert!(
        !methods.iter().any(|method| method == "shutdown"),
        "{methods:?}"
    );
}

#[test]
fn socket_that_never_answers_is_refused_without_launching_a_daemon() {
    let daemon = FakeDaemon::start(|_| FakeReply::Silent);

    let (status, stderr) = run_mcp(daemon.socket());

    assert!(!status.success(), "mcp must fail: {stderr}");
    assert!(stderr.contains("did not answer"), "{stderr}");
    assert!(!stderr.contains("auto-launching"), "{stderr}");
    let methods = daemon.stop();
    assert_eq!(
        methods,
        ["list"],
        "only the bounded listening probe may run"
    );
}

#[test]
fn daemon_that_stops_answering_after_admission_fails_proxy_startup_within_the_bound() {
    // Compatible for the admission handshake, then silent: proxy setup must
    // not inherit the daemon client's tool-call reply deadline.
    let admitted = AtomicBool::new(false);
    let daemon = FakeDaemon::start(move |method| match method {
        "session_end" => FakeReply::unknown_method(method),
        _ if admitted.load(Ordering::SeqCst) => FakeReply::Silent,
        "metadata" => {
            admitted.store(true, Ordering::SeqCst);
            FakeReply::ok(
                serde_json::to_value(cua_driver_core::daemon::current_daemon_metadata()).unwrap(),
            )
        }
        other => legacy_daemon(other),
    });
    assert_listening(&daemon);

    let (status, stderr) = run_mcp(daemon.socket());

    assert!(!status.success(), "mcp must fail: {stderr}");
    assert!(stderr.contains("did not answer"), "{stderr}");
    let methods = daemon.stop();
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "metadata")
            .count(),
        1,
        "{methods:?}"
    );
    assert!(
        !methods.iter().any(|method| method == "shutdown"),
        "{methods:?}"
    );
}
