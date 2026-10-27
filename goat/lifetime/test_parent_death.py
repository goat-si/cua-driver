#!/usr/bin/env python3
"""Headless host-lifetime regression: python3 goat/lifetime/test_parent_death.py BINARY.

The controller keeps the driver's stdin writer open after killing its launcher.
macOS kqueue and Linux pidfd report exit without polling or synchronization sleeps.
"""

import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest

TIMEOUT = 15
BINARY = None


class ExitEvent:
    def __init__(self, pid):
        self.pid = pid
        if sys.platform == "darwin":
            self.queue = select.kqueue()
            event = select.kevent(pid, filter=select.KQ_FILTER_PROC,
                                  flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT,
                                  fflags=select.KQ_NOTE_EXIT)
            self.queue.control([event], 0, 0)
        else:
            self.fd = os.pidfd_open(pid)

    def wait(self):
        if sys.platform == "darwin":
            return bool(self.queue.control(None, 1, TIMEOUT))
        return bool(select.select([self.fd], [], [], TIMEOUT)[0])

    def close(self):
        if sys.platform == "darwin":
            self.queue.close()
        else:
            os.close(self.fd)


def read_until(pipe, predicate):
    deadline = time.monotonic() + TIMEOUT
    line = b""
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([pipe], [], [], remaining)[0]:
            raise AssertionError("driver readiness timed out")
        byte = os.read(pipe.fileno(), 1)
        if not byte:
            raise AssertionError(f"driver exited before readiness: {line!r}")
        line += byte
        if byte == b"\n":
            text = line.decode(errors="replace").strip()
            if predicate(text):
                return text
            line = b""


def launcher():
    # Only this short-lived process is the driver's parent. It does no cleanup.
    fd, binary, mode, root = int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]
    standalone = mode == "standalone"
    mode = "serve" if standalone else mode
    if mode == "worker":
        # The SDK's private worker: host-owned, started without --embedded,
        # blocked on stdin until the host sends its initialization line.
        args = [binary, "__private-worker", "--generation", "lifetime-test"]
    else:
        args = [binary, mode, "--no-overlay"]
    if not standalone and mode != "worker":
        args += ["--embedded"]
    if mode == "mcp":
        args += ["--direct"]
    elif mode == "serve":
        args += ["--no-permissions-gate", "--socket", root + "/s",
                 "--pid-file", root + "/pid"]
        if sys.argv[6] == "stdio":
            args += ["--parent-liveness-stdio"]
    cause = sys.argv[7]
    if cause == "parent-term":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGTERM})
    if cause == "startup":
        pid = os.fork()
        if pid == 0:
            # The controller releases exec only after reaping the launcher.
            # This deterministically exercises the already-orphaned startup.
            os.read(fd, 1)
            os.dup2(fd, 0)
            os.execv(binary, args)
        print(f"DRIVER_PID={pid}", flush=True)
        signal.pause()
        return
    child = subprocess.Popen(args, stdin=fd, stdout=sys.stdout, stderr=sys.stdout)
    print(f"DRIVER_PID={child.pid}", flush=True)
    signal.pause()


class ParentDeath(unittest.TestCase):
    def run_driver(self, mode, cause, stdio=False):
        with tempfile.TemporaryDirectory(prefix="cualife-", dir="/tmp") as root:
            env = dict(os.environ, HOME=root, CUA_DRIVER_RS_HOME=root,
                       XDG_CONFIG_HOME=root, XDG_CACHE_HOME=root, XDG_STATE_HOME=root,
                       DO_NOT_TRACK="1", CUA_DRIVER_RS_UPDATE_CHECK="false",
                       CUA_DRIVER_RS_TELEMETRY_ENABLED="false", CUA_TELEMETRY_ENABLED="false")
            env.pop("CUA_DRIVER_PARENT_LIVENESS_STDIN", None)
            env.pop("CUA_DRIVER_EMBEDDED", None)
            read_fd, write_fd = os.pipe()
            parent = subprocess.Popen(
                [sys.executable, __file__, "--launcher", str(read_fd), BINARY,
                 mode, root, "stdio" if stdio else "no-stdio", cause],
                pass_fds=(read_fd,), stdout=subprocess.PIPE, env=env)
            os.close(read_fd)
            driver_pid = None
            event = None
            exited = False
            children = []
            try:
                line = read_until(parent.stdout, lambda s: s.startswith("DRIVER_PID="))
                driver_pid = int(line.split("=", 1)[1])
                event = ExitEvent(driver_pid)
                if cause == "startup":
                    pass
                elif mode == "worker":
                    # No readiness line before initialization. Killing the
                    # launcher at any point after exec must end the worker.
                    pass
                elif mode == "mcp":
                    request = {"jsonrpc": "2.0", "id": 1, "method": "initialize",
                               "params": {"protocolVersion": "2024-11-05",
                                          "capabilities": {}, "clientInfo": {
                                              "name": "lifetime-test", "version": "1"}}}
                    os.write(write_fd, json.dumps(request).encode() + b"\n")
                    read_until(parent.stdout, lambda s: s.startswith('{') and
                               json.loads(s).get("id") == 1)
                else:
                    read_until(parent.stdout, lambda s: "daemon listening on" in s)
                if mode not in {"standalone", "worker"} and cause != "startup":
                    self.assertEqual(os.getpgid(driver_pid), driver_pid,
                                     "embedded driver must own its process group")
                    if sys.platform == "darwin":
                        rows = subprocess.check_output(
                            ["/bin/ps", "-axo", "pid=,ppid="], text=True, timeout=TIMEOUT)
                        pids = " ".join(row.split()[0] for row in rows.splitlines()
                                        if row.split()[1] == str(driver_pid))
                    else:
                        pids = Path(f"/proc/{driver_pid}/task/{driver_pid}/children").read_text()
                    children = [ExitEvent(int(pid)) for pid in pids.split()]
                    self.assertTrue(children, "embedded group sentinel must be running")
                if cause in {"parent", "parent-term", "startup"}:
                    parent.kill()
                    parent.wait(timeout=TIMEOUT)
                    if cause == "startup":
                        os.write(write_fd, b"x")
                elif cause == "driver":
                    os.kill(driver_pid, signal.SIGKILL)
                elif cause == "sentinel":
                    os.kill(children[0].pid, signal.SIGKILL)
                else:
                    os.close(write_fd)
                    write_fd = None
                if mode == "standalone":
                    # An ordinary daemon must still answer after its launcher
                    # exits. Shut it down explicitly so this test owns cleanup.
                    with socket.socket(socket.AF_UNIX) as conn:
                        conn.settimeout(TIMEOUT)
                        conn.connect(root + "/s")
                        conn.sendall(b'{"method":"shutdown"}\n')
                        self.assertIn(b'"ok":true', conn.recv(4096))
                exited = event.wait()
                self.assertTrue(exited,
                                f"{mode} driver {driver_pid} survived {cause} death/EOF")
                for child in children:
                    self.assertTrue(child.wait(), f"helper {child.pid} survived the driver")
            finally:
                if write_fd is not None:
                    os.close(write_fd)
                if driver_pid is not None and not exited:
                    try:
                        os.kill(driver_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                if event is not None:
                    event.close()
                for child in children:
                    child.close()
                if parent.poll() is None:
                    parent.kill()
                parent.wait(timeout=TIMEOUT)
                parent.stdout.close()

    def test_direct_mcp_exits_when_parent_is_sigkilled_with_stdin_open(self):
        self.run_driver("mcp", "parent")

    def test_embedded_serve_exits_when_parent_is_sigkilled_with_stdin_open(self):
        self.run_driver("serve", "parent")

    def test_direct_mcp_stdin_eof(self):
        self.run_driver("mcp", "eof")

    def test_embedded_serve_lifetime_stdin_eof(self):
        self.run_driver("serve", "eof", stdio=True)

    def test_driver_sigkill_also_ends_its_helpers(self):
        self.run_driver("mcp", "driver")

    def test_sentinel_loss_stops_the_driver(self):
        self.run_driver("mcp", "sentinel")

    def test_parent_already_dead_at_exec(self):
        self.run_driver("mcp", "startup")

    def test_parent_death_with_inherited_ignored_and_blocked_sigterm(self):
        self.run_driver("mcp", "parent-term")

    def test_standalone_serve_survives_its_launcher(self):
        self.run_driver("standalone", "parent")

    def test_private_worker_exits_when_parent_is_sigkilled_with_stdin_open(self):
        self.run_driver("worker", "parent")

    def test_private_worker_parent_already_dead_at_exec(self):
        self.run_driver("worker", "startup")


if __name__ == "__main__":
    if sys.argv[1:2] == ["--launcher"]:
        launcher()
    else:
        BINARY = str(Path(sys.argv.pop(1)).resolve())
        unittest.main(verbosity=2)
