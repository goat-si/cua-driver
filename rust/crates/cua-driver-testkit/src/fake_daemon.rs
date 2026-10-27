// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! A scripted Unix-socket daemon for startup and compatibility tests.
//!
//! Each accepted connection carries one newline-delimited request, like the
//! real daemon protocol. The fake records every request method in arrival
//! order and answers with whatever the test's reply function returns. A
//! successful `shutdown` or `shutdown_if_pid` reply ends the fake, the same
//! way a real daemon releases its socket after honouring `cua-driver stop`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;

const STOP_METHOD: &str = "__cua_fake_daemon_stop";

/// How the fake answers one request.
#[derive(Debug, Clone)]
pub enum FakeReply {
    /// Write this value as the one-line response.
    Json(serde_json::Value),
    /// Keep the connection open and never answer while the fake runs.
    Silent,
}

impl FakeReply {
    /// A successful response carrying `result`.
    pub fn ok(result: serde_json::Value) -> Self {
        Self::Json(serde_json::json!({ "ok": true, "result": result }))
    }

    /// The reply an older daemon gives for a method it does not implement.
    pub fn unknown_method(method: &str) -> Self {
        Self::Json(serde_json::json!({
            "ok": false,
            "error": format!("Unknown method: {method}"),
        }))
    }
}

/// A socket path nothing is bound to yet, for tests that bind the fake only
/// when a launcher under test "starts" a daemon.
pub struct FakeEndpoint {
    socket: String,
    directory: tempfile::TempDir,
}

impl FakeEndpoint {
    pub fn new() -> Self {
        // macOS limits socket paths to 104 bytes; the per-user temp directory
        // can exceed that, so bind under /tmp like the other socket tests.
        let directory = tempfile::Builder::new()
            .prefix("cua-fake-daemon-")
            .tempdir_in("/tmp")
            .expect("fake daemon directory");
        let socket = directory.path().join("d.sock").display().to_string();
        Self { socket, directory }
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// Bind the endpoint and answer each request with `reply(method)`.
    pub fn start(self, reply: impl Fn(&str) -> FakeReply + Send + 'static) -> FakeDaemon {
        let listener = UnixListener::bind(&self.socket).expect("bind fake daemon socket");
        let (methods_tx, methods) = mpsc::channel();
        let server = std::thread::Builder::new()
            .name("cua-fake-daemon".into())
            .spawn(move || serve(listener, reply, methods_tx))
            .expect("spawn fake daemon");
        FakeDaemon {
            socket: self.socket,
            methods,
            server: Some(server),
            _directory: self.directory,
        }
    }
}

impl Default for FakeEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

/// A test-owned daemon endpoint. Dropping it stops the server.
pub struct FakeDaemon {
    socket: String,
    methods: Receiver<String>,
    server: Option<JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

impl FakeDaemon {
    /// Bind a fresh socket and answer each request with `reply(method)`.
    pub fn start(reply: impl Fn(&str) -> FakeReply + Send + 'static) -> Self {
        FakeEndpoint::new().start(reply)
    }

    /// Socket path to pass as `--socket`.
    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// Stop the server (if a `shutdown` has not already) and return every
    /// request method it received, in order.
    pub fn stop(mut self) -> Vec<String> {
        self.shutdown_server();
        self.methods.try_iter().collect()
    }

    fn shutdown_server(&mut self) {
        let Some(server) = self.server.take() else {
            return;
        };
        // After an honoured `shutdown` the listener is gone and connect fails;
        // the server thread has already returned in that case.
        if let Ok(mut stream) = UnixStream::connect(Path::new(&self.socket)) {
            let _ = writeln!(stream, "{}", serde_json::json!({ "method": STOP_METHOD }));
        }
        server.join().expect("fake daemon thread panicked");
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.shutdown_server();
        }
    }
}

fn serve(listener: UnixListener, reply: impl Fn(&str) -> FakeReply, methods: mpsc::Sender<String>) {
    // Silent connections stay open until the fake stops, so a client blocked
    // on its reply observes EOF only at teardown.
    let mut held = Vec::new();
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            break;
        };
        let mut line = String::new();
        let Ok(reader) = stream.try_clone() else {
            continue;
        };
        if BufReader::new(reader).read_line(&mut line).unwrap_or(0) == 0 {
            continue;
        }
        let method = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|request| request["method"].as_str().map(str::to_owned))
            .unwrap_or_default();
        if method == STOP_METHOD {
            break;
        }
        let _ = methods.send(method.clone());
        match reply(&method) {
            FakeReply::Silent => held.push(stream),
            FakeReply::Json(response) => {
                let honoured = response["ok"] == true;
                let mut writer = stream;
                let _ = writeln!(writer, "{response}");
                if honoured && matches!(method.as_str(), "shutdown" | "shutdown_if_pid") {
                    break;
                }
            }
        }
    }
}
