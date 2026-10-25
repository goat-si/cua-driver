#!/usr/bin/env python3
"""Protocol compatibility between a fork build and the Cua Driver release GOAT ships.

GOAT starts the driver in the launch forms below and reads its CLI manifest, MCP surface and
daemon metadata. A fork change that removes or retypes a tool, subcommand or argument, changes a
tool's input schema or a capability, changes the daemon contract, or stops a launch form from
starting breaks GOAT without a GOAT change. Those fail unless goat/compat/allowed-changes.json
records the change with a reason. Additions pass and are listed. Standard library only.
"""

import argparse
import hashlib
import json
import os
import queue
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.request
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
QUIET_ENV = {
    "CUA_DRIVER_RS_TELEMETRY_ENABLED": "false",
    "CUA_TELEMETRY_ENABLED": "false",
    "CUA_DRIVER_RS_UPDATE_CHECK": "false",
    "DO_NOT_TRACK": "1",
}
HOST_ID = "si.goat.compat-check"
REPLY_SECONDS = 30
EXIT_SECONDS = 15
MAX_PAGES = 50
# Daemon contract fields GOAT reads and compares (goat-si/goat apps/desktop/src/cua/ComputerUseHelper.ts).
CONTRACT_FIELDS = ("contract_version", "mcp_protocol_version", "capability_version", "tools_list_schema_version")


def fail(message):
    sys.exit(message)


def fetch_baseline(spec, key, into):
    archive = spec["archives"][key]
    target = into / archive["name"]
    with urllib.request.urlopen(spec["baseUrl"] + archive["name"], timeout=120) as response:
        target.write_bytes(response.read())
    data = target.read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    if len(data) != archive["bytes"] or digest != archive["sha256"]:
        fail(f"baseline {archive['name']} does not match its pin: {len(data)} bytes, sha256 {digest}")
    out = into / "baseline"
    out.mkdir()
    if archive["name"].endswith(".zip"):
        with zipfile.ZipFile(target) as z:
            z.extractall(out)
    else:
        with tarfile.open(target) as t:
            # The "data" filter (3.12+, backported to 3.8.17+) refuses links and paths escaping out.
            t.extractall(out, **({"filter": "data"} if hasattr(tarfile, "data_filter") else {}))
    exe = "cua-driver.exe" if archive["name"].endswith(".zip") else "cua-driver"
    found = [p for p in out.rglob(exe) if p.is_file()]
    if len(found) != 1:
        fail(f"baseline archive has {len(found)} {exe} entries")
    found[0].chmod(0o755)
    return found[0]


class Child:
    """A driver subprocess whose stdout lines and stderr are drained by threads, so no pipe can
    fill and block the driver, and every read has a deadline."""

    def __init__(self, argv, stdout=True):
        self.argv = argv
        self.proc = subprocess.Popen(
            argv,
            env=dict(os.environ, **QUIET_ENV),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE if stdout else subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        self.lines = queue.Queue()
        self.stderr = bytearray()
        threading.Thread(target=self._drain_stderr, daemon=True).start()
        if stdout:
            threading.Thread(target=self._read_stdout, daemon=True).start()

    def _drain_stderr(self):
        for chunk in iter(lambda: self.proc.stderr.read1(65536), b""):
            self.stderr += chunk
            del self.stderr[:-8192]

    def _read_stdout(self):
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def describe_failure(self, what):
        return f"{' '.join(self.argv[1:])}: {what}; stderr tail: {self.stderr.decode(errors='replace')[-1500:]}"

    def send(self, message):
        self.proc.stdin.write((json.dumps(message) + "\n").encode())
        self.proc.stdin.flush()

    def reply(self, request_id):
        deadline = time.monotonic() + REPLY_SECONDS
        while True:
            try:
                line = self.lines.get(timeout=max(0.0, deadline - time.monotonic()))
            except queue.Empty:
                fail(self.describe_failure(f"no reply to request {request_id} within {REPLY_SECONDS}s"))
            if line is None:
                fail(self.describe_failure("closed stdout"))
            message = json.loads(line)
            if message.get("id") == request_id:
                if "error" in message:
                    fail(self.describe_failure(f"MCP error {message['error']}"))
                return message["result"]

    def close(self, require_exit=False):
        """Close stdin; with require_exit the child must then exit on its own."""
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            code = self.proc.wait(timeout=EXIT_SECONDS)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=EXIT_SECONDS)
            if require_exit:
                fail(self.describe_failure(f"did not exit within {EXIT_SECONDS}s after its parent pipe closed"))
            return None
        return code


def mcp_argv(binary):
    # The forms GOAT launches: macOS through its helper (main.swift, McpRelay.swift), elsewhere as
    # packages/shared/src/cuaDriverManifest.ts does.
    if sys.platform == "darwin":
        return [str(binary), "mcp", "--direct", "--embedded", "--host-bundle-id", HOST_ID]
    return [str(binary), "mcp"]


def mcp_surface(binary):
    child = Child(mcp_argv(binary))
    try:
        child.send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "goat-compat", "version": "1"}}})
        init = child.reply(1)
        child.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        tools, cursor, seen = {}, None, set()
        for page_number in range(MAX_PAGES):
            request_id = 2 + page_number
            child.send({"jsonrpc": "2.0", "id": request_id, "method": "tools/list", "params": {"cursor": cursor} if cursor else {}})
            page = child.reply(request_id)
            for tool in page["tools"]:
                tools[tool["name"]] = tool.get("inputSchema")
            cursor = page.get("nextCursor")
            if not cursor:
                break
            if cursor in seen:
                fail(child.describe_failure(f"tools/list repeated cursor {cursor!r}"))
            seen.add(cursor)
        else:
            fail(child.describe_failure(f"tools/list did not finish within {MAX_PAGES} pages"))
    finally:
        child.close()
    return {"protocolVersion": init.get("protocolVersion"), "capabilities": init.get("capabilities", {}), "tools": tools}


def daemon_metadata(binary):
    """Start the embedded daemon the way GOAT's Windows helper does (DriverService.cs), read its
    metadata like the SDK's embedded startup, and require it to exit when its parent pipe closes."""
    if sys.platform == "win32":
        path = rf"\\.\pipe\goat-compat-{os.getpid()}-{time.monotonic_ns()}"
    else:
        path = str(Path(tempfile.mkdtemp(prefix="gc", dir="/tmp")) / "s")  # short: sun_path limit
    child = Child([str(binary), "serve", "--embedded", "--parent-liveness-stdio", "--no-permissions-gate", "--host-bundle-id", HOST_ID, "--socket", path], stdout=False)
    deadline = time.monotonic() + REPLY_SECONDS
    response = None
    try:
        while response is None:
            if child.proc.poll() is not None:
                fail(child.describe_failure(f"exited {child.proc.returncode} before serving metadata"))
            if time.monotonic() > deadline:
                fail(child.describe_failure(f"no metadata on {path} within {REPLY_SECONDS}s"))
            try:
                response = request_metadata(path)
            except OSError:
                time.sleep(0.2)
    finally:
        child.close(require_exit=True)
    if not response.get("ok") or not isinstance(response.get("result"), dict):
        fail(f"metadata request failed: {response}")
    return response["result"]


def request_metadata(path):
    request = b'{"method":"metadata"}\n'
    if sys.platform == "win32":
        with open(path, "r+b", buffering=0) as pipe:
            pipe.write(request)
            line = b""
            while not line.endswith(b"\n"):
                chunk = pipe.read(4096)
                if not chunk:
                    break
                line += chunk
    else:
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(REPLY_SECONDS)
            conn.connect(path)
            conn.sendall(request)
            line = conn.makefile("rb").readline()
    return json.loads(line)


def describe(binary):
    manifest = json.loads(subprocess.run([str(binary), "manifest"], env=dict(os.environ, **QUIET_ENV), check=True, capture_output=True, timeout=60).stdout)
    return {"manifest": manifest, **mcp_surface(binary), "daemon": daemon_metadata(binary)}


def validate(surface, label, version, required_tools):
    """Shape and identity checks that keep a comparison from passing on empty or wrong data."""
    problems = []
    manifest = surface["manifest"]
    if manifest.get("schema_version") in (None, "") or not manifest.get("subcommands"):
        problems.append(f"{label}: manifest has no schema_version or subcommands")
    if manifest.get("binary_version") != version:
        problems.append(f"{label}: manifest binary_version {manifest.get('binary_version')!r}, expected {version!r}")
    if not surface["protocolVersion"] or not surface["tools"]:
        problems.append(f"{label}: MCP initialize/tools/list returned no protocol version or no tools")
    missing = sorted(set(required_tools) - set(surface["tools"]))
    if missing:
        problems.append(f"{label}: missing tools GOAT allows agents to use: {', '.join(missing)}")
    daemon = surface["daemon"]
    if daemon.get("driver_version") != version:
        problems.append(f"{label}: daemon driver_version {daemon.get('driver_version')!r}, expected {version!r}")
    if daemon.get("embedded") is not True or daemon.get("host_bundle_id") != HOST_ID:
        problems.append(f"{label}: daemon is not embedded for {HOST_ID}: {daemon}")
    if any(not daemon.get(field) for field in CONTRACT_FIELDS):
        problems.append(f"{label}: daemon metadata lacks {CONTRACT_FIELDS}: {daemon}")
    return problems


def load_allowed(path):
    allowed = json.loads(path.read_text())
    for kind, entries in allowed.items():
        if kind == "comment":
            continue
        for name, reason in entries.items():
            if not isinstance(reason, str) or not reason.strip():
                fail(f"{path.name}: {kind} {name} needs a nonblank reason")
    return allowed


def compare(baseline, candidate, allowed):
    problems = []

    def check(kind, name, message):
        if name not in allowed.get(kind, {}):
            problems.append(f"{kind} {name}: {message}")

    if candidate["protocolVersion"] != baseline["protocolVersion"]:
        check("mcp", "protocolVersion", f"{baseline['protocolVersion']} -> {candidate['protocolVersion']}")
    for name, value in baseline["capabilities"].items():
        if name not in candidate["capabilities"]:
            check("mcp", f"capability:{name}", "removed")
        elif candidate["capabilities"][name] != value:
            check("mcp", f"capability:{name}", "changed")
    for name, schema in baseline["tools"].items():
        if name not in candidate["tools"]:
            check("tools", name, "removed")
        elif candidate["tools"][name] != schema:
            check("tools", name, "input schema changed")
    old, new = baseline["manifest"], candidate["manifest"]
    for key in sorted(set(old) - set(new)):
        check("manifest", key, "removed")
    for name, value in old.get("features", {}).items():
        if new.get("features", {}).get(name) != value:
            check("manifest", f"feature:{name}", f"{value!r} -> {new.get('features', {}).get(name)!r}")
    if new.get("schema_version") != old.get("schema_version"):
        check("manifest", "schema_version", f"{old.get('schema_version')} -> {new.get('schema_version')}")
    if new.get("mcp_invocation", {}).get("args") != old.get("mcp_invocation", {}).get("args"):
        check("manifest", "mcp_invocation", "args changed")
    current = {s["name"]: {(a["name"], a["type"]) for a in s.get("args", [])} for s in new.get("subcommands", [])}
    for sub in old.get("subcommands", []):
        if sub["name"] not in current:
            check("subcommands", sub["name"], "removed")
            continue
        for arg in sorted({(a["name"], a["type"]) for a in sub.get("args", [])} - current[sub["name"]]):
            check("subcommands", f"{sub['name']} {arg[0]}", f"argument {arg} removed or retyped")
    for field in CONTRACT_FIELDS:
        if candidate["daemon"].get(field) != baseline["daemon"].get(field):
            check("daemon", field, f"{baseline['daemon'].get(field)} -> {candidate['daemon'].get(field)}")
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--candidate", required=True, type=Path, help="fork-built cua-driver executable")
    parser.add_argument("--candidate-version", required=True, help="version the fork build must report (rust/Cargo.toml)")
    parser.add_argument("--baseline-key", required=True, help="key in goat/compat/baseline.json, e.g. linux-x86_64")
    args = parser.parse_args()
    spec = json.loads((HERE / "baseline.json").read_text())
    allowed = load_allowed(HERE / "allowed-changes.json")
    with tempfile.TemporaryDirectory() as tmp:
        baseline = describe(fetch_baseline(spec, args.baseline_key, Path(tmp)))
        candidate = describe(args.candidate.resolve())
    problems = validate(baseline, "baseline", spec["version"], spec["goatTools"])
    problems += validate(candidate, "candidate", args.candidate_version, spec["goatTools"])
    if problems:
        fail("\n".join(problems))
    added = sorted(set(candidate["tools"]) - set(baseline["tools"]))
    print(f"baseline {spec['version']}: {len(baseline['tools'])} tools, contract {baseline['daemon']['contract_version']}; candidate {args.candidate_version}: {len(candidate['tools'])} tools, contract {candidate['daemon']['contract_version']}")
    if added:
        print("added tools: " + ", ".join(added))
    problems = compare(baseline, candidate, allowed)
    for problem in problems:
        print("INCOMPATIBLE " + problem)
    if problems:
        fail(f"{len(problems)} incompatible change(s); fix them or record each in goat/compat/allowed-changes.json with a reason")
    print("protocol-compatible with the baseline; GOAT's launch forms start and the daemon exits when its parent pipe closes")


if __name__ == "__main__":
    main()
