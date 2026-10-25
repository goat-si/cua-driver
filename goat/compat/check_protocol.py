#!/usr/bin/env python3
"""Protocol compatibility between a fork build and the Cua Driver release GOAT ships.

GOAT drives the binary through its CLI manifest and MCP (initialize, tools/list). A fork change
that removes a tool, subcommand or argument, or changes a tool's input schema, breaks GOAT
without a GOAT change, so this fails unless goat/compat/allowed-changes.json names the change
with a reason. Additions pass and are listed. Standard library only.
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys
import tarfile
import tempfile
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


def fetch_baseline(key: str, into: Path) -> Path:
    spec = json.loads((HERE / "baseline.json").read_text())
    archive = spec["archives"][key]
    target = into / archive["name"]
    with urllib.request.urlopen(spec["baseUrl"] + archive["name"], timeout=120) as response:
        target.write_bytes(response.read())
    data = target.read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    if len(data) != archive["bytes"] or digest != archive["sha256"]:
        sys.exit(f"baseline {archive['name']} does not match its pin: {len(data)} bytes, sha256 {digest}")
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
        sys.exit(f"baseline archive has {len(found)} {exe} entries")
    found[0].chmod(0o755)
    return found[0]


def mcp_args() -> list:
    # GOAT launches the macOS driver with --direct so it does not proxy through CuaDriver.app.
    return ["mcp", "--direct"] if sys.platform == "darwin" else ["mcp"]


def describe(binary: Path) -> dict:
    env = dict(os.environ, **QUIET_ENV)
    manifest = json.loads(subprocess.run([str(binary), "manifest"], env=env, check=True, capture_output=True, timeout=60).stdout)
    proc = subprocess.Popen([str(binary), *mcp_args()], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def send(message):
        proc.stdin.write(json.dumps(message) + "\n")
        proc.stdin.flush()

    def reply(request_id):
        while True:
            line = proc.stdout.readline()
            if not line:
                sys.exit(f"{binary} closed MCP stdout: {proc.stderr.read()[:2000]}")
            message = json.loads(line)
            if message.get("id") == request_id:
                if "error" in message:
                    sys.exit(f"{binary} MCP error: {message['error']}")
                return message["result"]

    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "goat-compat", "version": "1"}}})
        init = reply(1)
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        tools, cursor, request_id = {}, None, 2
        while True:
            send({"jsonrpc": "2.0", "id": request_id, "method": "tools/list", "params": {"cursor": cursor} if cursor else {}})
            page = reply(request_id)
            for tool in page["tools"]:
                tools[tool["name"]] = tool.get("inputSchema")
            cursor, request_id = page.get("nextCursor"), request_id + 1
            if not cursor:
                break
    finally:
        proc.stdin.close()
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
    return {"manifest": manifest, "protocolVersion": init.get("protocolVersion"), "capabilities": sorted(init.get("capabilities", {})), "tools": tools}


def compare(baseline: dict, candidate: dict, allowed: dict) -> list:
    problems = []

    def check(kind, name, message):
        if name not in allowed.get(kind, {}):
            problems.append(f"{kind} {name}: {message}")

    if candidate["protocolVersion"] != baseline["protocolVersion"]:
        check("mcp", "protocolVersion", f"{baseline['protocolVersion']} -> {candidate['protocolVersion']}")
    for capability in sorted(set(baseline["capabilities"]) - set(candidate["capabilities"])):
        check("mcp", f"capability:{capability}", "removed")
    for name, schema in baseline["tools"].items():
        if name not in candidate["tools"]:
            check("tools", name, "removed")
        elif candidate["tools"][name] != schema:
            check("tools", name, "input schema changed")
    old, new = baseline["manifest"], candidate["manifest"]
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
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--candidate", required=True, type=Path, help="fork-built cua-driver executable")
    parser.add_argument("--baseline-key", required=True, help="key in goat/compat/baseline.json, e.g. linux-x86_64")
    args = parser.parse_args()
    allowed = json.loads((HERE / "allowed-changes.json").read_text())
    with tempfile.TemporaryDirectory() as tmp:
        baseline = describe(fetch_baseline(args.baseline_key, Path(tmp)))
    candidate = describe(args.candidate.resolve())
    added = sorted(set(candidate["tools"]) - set(baseline["tools"]))
    print(f"baseline {baseline['manifest'].get('binary_version')}: {len(baseline['tools'])} tools; candidate {candidate['manifest'].get('binary_version')}: {len(candidate['tools'])} tools")
    if added:
        print("added tools: " + ", ".join(added))
    problems = compare(baseline, candidate, allowed)
    for problem in problems:
        print("INCOMPATIBLE " + problem)
    if problems:
        sys.exit(f"{len(problems)} incompatible change(s); fix them or record each in goat/compat/allowed-changes.json with a reason")
    print("protocol-compatible with the baseline")


if __name__ == "__main__":
    main()
