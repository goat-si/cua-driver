import json
import os
import stat
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

import check_protocol
from check_protocol import HOST_ID, compare, daemon_metadata, load_allowed, mcp_surface, validate

DAEMON = {"driver_version": "1.0.0", "embedded": True, "host_bundle_id": HOST_ID, "contract_version": "0.8.0",
          "mcp_protocol_version": "2025-06-18", "capability_version": "1", "tools_list_schema_version": "1"}
BASE = {
    "protocolVersion": "2025-06-18",
    "capabilities": {"resources": {}, "tools": {}},
    "tools": {"click": {"type": "object", "properties": {"x": {"type": "number"}}}, "zoom": {"type": "object"}},
    "manifest": {
        "schema_version": "1",
        "binary_version": "1.0.0",
        "features": {"portal_input": True},
        "mcp_invocation": {"args": ["mcp"]},
        "subcommands": [{"name": "serve", "args": [{"name": "socket", "type": "string"}]}],
    },
    "daemon": DAEMON,
}


def variant(**changes):
    candidate = {**BASE, "tools": dict(BASE["tools"]), "manifest": dict(BASE["manifest"]), "daemon": dict(DAEMON)}
    candidate.update(changes)
    return candidate


class CompareTest(unittest.TestCase):
    def test_identical_and_additive_builds_are_compatible(self):
        self.assertEqual(compare(BASE, BASE, {}), [])
        self.assertEqual(compare(BASE, variant(tools={**BASE["tools"], "new_tool": {}}), {}), [])

    def test_removed_or_changed_tools_are_incompatible(self):
        self.assertEqual(compare(BASE, variant(tools={"click": BASE["tools"]["click"]}), {}), ["tools zoom: removed"])
        changed = {**BASE["tools"], "click": {"type": "object", "properties": {"x": {"type": "string"}}}}
        self.assertEqual(compare(BASE, variant(tools=changed), {}), ["tools click: input schema changed"])

    def test_cli_mcp_and_daemon_breaks_are_incompatible(self):
        retyped = {**BASE["manifest"], "subcommands": [{"name": "serve", "args": [{"name": "socket", "type": "path"}]}]}
        self.assertEqual(compare(BASE, variant(manifest=retyped), {}), ["subcommands serve socket: argument ('socket', 'string') removed or retyped"])
        self.assertEqual(compare(BASE, variant(manifest={**BASE["manifest"], "features": {}}), {}), ["manifest feature:portal_input: True -> None"])
        self.assertEqual(compare(BASE, variant(protocolVersion="2026-01-01"), {}), ["mcp protocolVersion: 2025-06-18 -> 2026-01-01"])
        self.assertEqual(compare(BASE, variant(capabilities={"tools": {"listChanged": True}}), {}), ["mcp capability:resources: removed", "mcp capability:tools: changed"])
        self.assertEqual(compare(BASE, variant(daemon={**DAEMON, "contract_version": "0.9.0"}), {}), ["daemon contract_version: 0.8.0 -> 0.9.0"])

    def test_recorded_changes_are_allowed(self):
        allowed = {"tools": {"zoom": "removed upstream; GOAT stopped using it in goat-si/goat#0"}}
        self.assertEqual(compare(BASE, variant(tools={"click": BASE["tools"]["click"]}), allowed), [])


class ValidateTest(unittest.TestCase):
    def test_complete_surface_with_the_expected_identity_passes(self):
        self.assertEqual(validate(BASE, "candidate", "1.0.0", ["click"]), [])

    def test_empty_or_misidentified_surfaces_fail(self):
        empty = variant(tools={}, protocolVersion=None, manifest={"binary_version": "1.0.0"})
        self.assertEqual(len(validate(empty, "candidate", "1.0.0", ["click"])), 3)
        self.assertEqual(
            validate(BASE, "candidate", "1.0.0-goat.1", []),
            ["candidate: manifest binary_version '1.0.0', expected '1.0.0-goat.1'",
             "candidate: daemon driver_version '1.0.0', expected '1.0.0-goat.1'"],
        )
        self.assertEqual(len(validate(variant(daemon={**DAEMON, "embedded": False}), "candidate", "1.0.0", [])), 1)

    def test_allowed_changes_need_reasons(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "allowed.json"
            path.write_text(json.dumps({"tools": {"zoom": " "}}))
            with self.assertRaises(SystemExit):
                load_allowed(path)


FAKE = textwrap.dedent('''\
    #!PYTHON
    import json, os, socket, sys, time
    mode = os.environ["FAKE_MODE"]
    if sys.argv[1] == "serve":
        path = sys.argv[sys.argv.index("--socket") + 1]
        server = socket.socket(socket.AF_UNIX); server.bind(path); server.listen(1)
        conn, _ = server.accept(); conn.makefile().readline()
        conn.sendall((json.dumps({"ok": True, "result": {"driver_version": "1.0.0"}}) + "\\n").encode()); conn.close()
        while True:
            time.sleep(1)  # ignores its parent pipe closing
    if mode == "stderr-flood":
        sys.stderr.write("x" * (1 << 20)); sys.stderr.flush()
    for line in sys.stdin:
        msg = json.loads(line)
        if "id" not in msg:
            continue
        result = {"protocolVersion": "2025-06-18", "capabilities": {}} if msg["method"] == "initialize" else {"tools": [{"name": "click", "inputSchema": {}}]}
        if mode == "repeat-cursor" and msg["method"] == "tools/list":
            result["nextCursor"] = "same"
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
''')


@unittest.skipIf(sys.platform == "win32", "fake driver uses a shebang and a Unix socket")
class ChildProcessTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.driver = Path(self.dir.name) / "fake-driver"
        self.driver.write_text(FAKE.replace("PYTHON", sys.executable))
        self.driver.chmod(self.driver.stat().st_mode | stat.S_IXUSR)
        self.saved = (check_protocol.REPLY_SECONDS, check_protocol.EXIT_SECONDS)
        check_protocol.REPLY_SECONDS, check_protocol.EXIT_SECONDS = 5, 1

    def tearDown(self):
        check_protocol.REPLY_SECONDS, check_protocol.EXIT_SECONDS = self.saved
        self.dir.cleanup()

    def run_mode(self, mode, function):
        os.environ["FAKE_MODE"] = mode
        try:
            return function(self.driver)
        finally:
            del os.environ["FAKE_MODE"]

    def test_a_driver_flooding_stderr_does_not_hang_the_check(self):
        self.assertEqual(self.run_mode("stderr-flood", mcp_surface)["tools"], {"click": {}})

    def test_a_repeated_cursor_fails_instead_of_looping(self):
        with self.assertRaises(SystemExit) as raised:
            self.run_mode("repeat-cursor", mcp_surface)
        self.assertIn("repeated cursor", str(raised.exception))

    def test_a_daemon_that_outlives_its_parent_pipe_fails(self):
        with self.assertRaises(SystemExit) as raised:
            self.run_mode("plain", daemon_metadata)
        self.assertIn("after its parent pipe closed", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
