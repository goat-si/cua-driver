#!/usr/bin/env python3
"""Protocol compatibility between a fork build and the Cua Driver release GOAT ships.

GOAT starts the driver in the launch forms below and reads its CLI manifest, MCP surface and
daemon metadata. A fork change that removes or retypes a tool, subcommand or argument, changes a
tool's input schema or a capability, drops or widens what a tool's output schema promises, changes
the daemon contract, or stops a launch form from starting breaks GOAT without a GOAT change. Those
fail unless goat/compat/allowed-changes.json records the change with a reason. Additions pass and
are listed. Standard library only.
"""

import argparse
import contextlib
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


def within(seconds, what, function):
    """Run function with an absolute deadline. A blocking read that outlives it is abandoned on
    a daemon thread; failing then kills the child process, which unblocks that read."""
    result, error = [], []

    def run():
        try:
            result.append(function())
        except BaseException as exc:  # noqa: BLE001 - re-raised on the caller's thread
            error.append(exc)

    worker = threading.Thread(target=run, daemon=True)
    worker.start()
    worker.join(max(0.0, seconds))
    if worker.is_alive():
        raise TimeoutError(f"{what} took longer than {seconds:.0f}s")
    if error:
        raise error[0]
    return result[0]


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
        self.readers = [threading.Thread(target=self._drain_stderr, daemon=True)]
        if stdout:
            self.readers.append(threading.Thread(target=self._read_stdout, daemon=True))
        for reader in self.readers:
            reader.start()

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
        """Close stdin and reap the child; with require_exit it must exit on its own."""
        with contextlib.suppress(OSError):
            self.proc.stdin.close()
        exited = True
        try:
            self.proc.wait(timeout=EXIT_SECONDS)
        except subprocess.TimeoutExpired:
            exited = False
            self.proc.kill()
            self.proc.wait(timeout=EXIT_SECONDS)
        for reader in self.readers:
            reader.join(timeout=EXIT_SECONDS)
        for stream in (self.proc.stdout, self.proc.stderr):
            if stream:
                stream.close()
        if require_exit and not exited:
            fail(self.describe_failure(f"did not exit within {EXIT_SECONDS}s after its parent pipe closed"))


def mcp_surface(argv):
    child = Child(argv)
    try:
        child.send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "goat-compat", "version": "1"}}})
        init = child.reply(1)
        child.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        tools, outputs, cursor, seen = {}, {}, None, set()
        for page_number in range(MAX_PAGES):
            request_id = 2 + page_number
            child.send({"jsonrpc": "2.0", "id": request_id, "method": "tools/list", "params": {"cursor": cursor} if cursor else {}})
            page = child.reply(request_id)
            for tool in page["tools"]:
                tools[tool["name"]] = tool.get("inputSchema")
                if tool.get("outputSchema") is not None:
                    outputs[tool["name"]] = tool["outputSchema"]
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
    return {"protocolVersion": init.get("protocolVersion"), "capabilities": init.get("capabilities", {}),
            "tools": tools, "outputSchemas": outputs}


@contextlib.contextmanager
def embedded_daemon(binary):
    """Start the embedded daemon as GOAT's Windows helper does (DriverService.cs) and read its
    metadata as the SDK's embedded startup does. Yields (endpoint, metadata) while it runs; on
    exit closes its parent pipe and requires it to shut down."""
    with tempfile.TemporaryDirectory(prefix="gc", dir=None if sys.platform == "win32" else "/tmp") as tmp:
        # Unix socket paths must stay under sun_path's ~100 bytes, hence a short /tmp directory.
        path = rf"\\.\pipe\goat-compat-{os.getpid()}-{time.monotonic_ns()}" if sys.platform == "win32" else str(Path(tmp) / "s")
        child = Child([str(binary), "serve", "--embedded", "--parent-liveness-stdio", "--no-permissions-gate", "--host-bundle-id", HOST_ID, "--socket", path], stdout=False)
        failed = False
        try:
            deadline = time.monotonic() + REPLY_SECONDS
            response = None
            while response is None:
                if child.proc.poll() is not None:
                    fail(child.describe_failure(f"exited {child.proc.returncode} before serving metadata"))
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    fail(child.describe_failure(f"no metadata on {path} within {REPLY_SECONDS}s"))
                try:
                    response = within(remaining, "metadata request", lambda: request_metadata(path))
                except TimeoutError as exc:
                    fail(child.describe_failure(str(exc)))
                except OSError:
                    time.sleep(0.2)
            if not response.get("ok") or not isinstance(response.get("result"), dict):
                fail(child.describe_failure(f"metadata request failed: {response}"))
            yield path, response["result"]
        except BaseException:
            failed = True
            raise
        finally:
            # Report the first failure; a daemon already being torn down after one is just reaped.
            child.close(require_exit=not failed)


def request_metadata(path):
    request = b'{"method":"metadata"}\n'
    line = b""
    if sys.platform == "win32":
        with open(path, "r+b", buffering=0) as pipe:
            pipe.write(request)
            while not line.endswith(b"\n"):
                chunk = pipe.read(4096)
                if not chunk:
                    break
                line += chunk
    else:
        with socket.socket(socket.AF_UNIX) as conn:
            conn.connect(path)
            conn.sendall(request)
            while not line.endswith(b"\n"):
                chunk = conn.recv(4096)
                if not chunk:
                    break
                line += chunk
    return json.loads(line)


def describe(binary):
    """Every launch form GOAT uses, keyed by name. The embedded proxy is how GOAT reaches the
    daemon on Windows (ComputerUseHelper.ts) and Linux (the SDK's embedded host); macOS runs
    the driver directly under its helper (main.swift, McpRelay.swift)."""
    manifest = json.loads(subprocess.run([str(binary), "manifest"], env=dict(os.environ, **QUIET_ENV), check=True, capture_output=True, timeout=60).stdout)
    forms = {}
    with embedded_daemon(binary) as (endpoint, metadata):
        forms["embedded-proxy"] = mcp_surface([str(binary), "mcp", "--embedded", "--socket", endpoint, "--host-bundle-id", HOST_ID])
    if sys.platform == "darwin":
        forms["direct-embedded"] = mcp_surface([str(binary), "mcp", "--direct", "--embedded", "--host-bundle-id", HOST_ID])
    return {"manifest": manifest, "daemon": metadata, "forms": forms}


def validate(surface, label, version, required_tools):
    """Shape and identity checks that keep a comparison from passing on empty or wrong data."""
    problems = []
    manifest = surface["manifest"]
    if manifest.get("schema_version") in (None, "") or not manifest.get("subcommands"):
        problems.append(f"{label}: manifest has no schema_version or subcommands")
    if manifest.get("binary_version") != version:
        problems.append(f"{label}: manifest binary_version {manifest.get('binary_version')!r}, expected {version!r}")
    if not surface["forms"]:
        problems.append(f"{label}: no MCP launch form was probed")
    for form, mcp in surface["forms"].items():
        if not mcp["protocolVersion"] or not mcp["tools"]:
            problems.append(f"{label} {form}: MCP initialize/tools/list returned no protocol version or no tools")
        missing = sorted(set(required_tools) - set(mcp["tools"]))
        if missing:
            problems.append(f"{label} {form}: missing tools GOAT allows agents to use: {', '.join(missing)}")
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


def schema_types(schema):
    declared = schema.get("type")
    if declared is None:
        return None
    return sorted(declared) if isinstance(declared, list) else [declared]


def output_schema_breaks(old, new, where=""):
    """What GOAT, reading a tool's result, could rely on in old and no longer in new: a property
    removed or no longer required, or a type or enum that now admits more values. New properties
    and narrower values are compatible."""
    label = f"property {where} " if where else ""
    if not isinstance(new, dict):
        return [f"{label}removed"]
    breaks = []
    old_types, new_types = schema_types(old), schema_types(new)
    if old_types is not None and (new_types is None or set(new_types) - set(old_types)):
        breaks.append(f"{label}type {old_types} -> {new_types}")
    if "enum" in old:
        if "enum" not in new:
            breaks.append(f"{label}enum removed")
        else:
            breaks += [f"{label}enum gained {value!r}" for value in new["enum"] if value not in old["enum"]]
    prefix = f"{where}." if where else ""
    for name in old.get("required", []):
        if name not in new.get("required", []):
            breaks.append(f"property {prefix}{name} no longer required")
    new_properties = new.get("properties", {})
    for name, schema in old.get("properties", {}).items():
        breaks += output_schema_breaks(schema, new_properties.get(name), f"{prefix}{name}")
    if isinstance(old.get("items"), dict):
        breaks += output_schema_breaks(old["items"], new.get("items"), f"{where}[]")
    return breaks


def compare(baseline, candidate, allowed):
    problems = []

    def check(kind, name, message):
        if name not in allowed.get(kind, {}):
            problems.append(f"{kind} {name}: {message}")

    for form, old_mcp in baseline["forms"].items():
        new_mcp = candidate["forms"].get(form)
        if new_mcp is None:
            problems.append(f"form {form}: not probed for the candidate")
            continue
        if new_mcp["protocolVersion"] != old_mcp["protocolVersion"]:
            check("mcp", "protocolVersion", f"{form}: {old_mcp['protocolVersion']} -> {new_mcp['protocolVersion']}")
        for name, value in old_mcp["capabilities"].items():
            if name not in new_mcp["capabilities"]:
                check("mcp", f"capability:{name}", f"{form}: removed")
            elif new_mcp["capabilities"][name] != value:
                check("mcp", f"capability:{name}", f"{form}: changed")
        for name, schema in old_mcp["tools"].items():
            if name not in new_mcp["tools"]:
                check("tools", name, f"{form}: removed")
            elif new_mcp["tools"][name] != schema:
                check("tools", name, f"{form}: input schema changed")
        new_outputs = new_mcp.get("outputSchemas", {})
        for name, schema in old_mcp.get("outputSchemas", {}).items():
            if name not in new_mcp["tools"]:
                continue  # reported above as a removed tool
            for detail in output_schema_breaks(schema, new_outputs.get(name)):
                check("tools", f"{name}:outputSchema", f"{form}: output schema {detail}")
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
    for form, mcp in candidate["forms"].items():
        added = sorted(set(mcp["tools"]) - set(baseline["forms"].get(form, {}).get("tools", {})))
        print(f"{form}: baseline {len(baseline['forms'][form]['tools'])} tools, candidate {len(mcp['tools'])}" + (f"; added: {', '.join(added)}" if added else ""))
    print(f"daemon contract: baseline {baseline['daemon']['contract_version']}, candidate {candidate['daemon']['contract_version']}")
    problems = compare(baseline, candidate, allowed)
    for problem in problems:
        print("INCOMPATIBLE " + problem)
    if problems:
        fail(f"{len(problems)} incompatible change(s); fix them or record each in goat/compat/allowed-changes.json with a reason")
    print("protocol-compatible with the baseline in every GOAT launch form; the daemon exits when its parent pipe closes")


if __name__ == "__main__":
    main()
