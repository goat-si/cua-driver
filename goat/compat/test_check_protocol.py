import contextlib
import json
import os
import stat
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

import check_protocol
from check_protocol import HOST_ID, compare, embedded_daemon, load_allowed, mcp_surface, validate

DAEMON = {"driver_version": "1.0.0", "embedded": True, "host_bundle_id": HOST_ID, "contract_version": "0.8.0",
          "mcp_protocol_version": "2025-06-18", "capability_version": "1", "tools_list_schema_version": "1"}
MCP = {
    "protocolVersion": "2025-06-18",
    "capabilities": {"resources": {}, "tools": {}},
    "tools": {"click": {"type": "object", "properties": {"x": {"type": "number"}}}, "zoom": {"type": "object"}},
    "outputSchemas": {"click": {"type": "object", "required": ["ok"], "properties": {
        "ok": {"type": "boolean"},
        "state": {"type": "string", "enum": ["on", "off"]},
        "rows": {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "integer"}}}},
    }}},
}
BASE = {
    "manifest": {
        "schema_version": "1",
        "binary_version": "1.0.0",
        "features": {"portal_input": True},
        "mcp_invocation": {"args": ["mcp"]},
        "subcommands": [{"name": "serve", "args": [{"name": "socket", "type": "string"}]}],
    },
    "daemon": DAEMON,
    "forms": {"embedded-proxy": MCP},
}


def variant(manifest=None, daemon=None, **mcp_changes):
    return {
        "manifest": manifest or dict(BASE["manifest"]),
        "daemon": daemon or dict(DAEMON),
        "forms": {"embedded-proxy": {**MCP, "tools": dict(MCP["tools"]), "outputSchemas": dict(MCP["outputSchemas"]), **mcp_changes}},
    }


def click_output(**properties):
    schema = json.loads(json.dumps(MCP["outputSchemas"]["click"]))
    for name, value in properties.items():
        if value is None:
            del schema["properties"][name]
        else:
            schema["properties"][name] = value
    return schema


class CompareTest(unittest.TestCase):
    def test_identical_and_additive_builds_are_compatible(self):
        self.assertEqual(compare(BASE, BASE, {}), [])
        self.assertEqual(compare(BASE, variant(tools={**MCP["tools"], "new_tool": {}}), {}), [])

    def test_removed_or_changed_tools_are_incompatible(self):
        self.assertEqual(compare(BASE, variant(tools={"click": MCP["tools"]["click"]}), {}), ["tools zoom: embedded-proxy: removed"])
        changed = {**MCP["tools"], "click": {"type": "object", "properties": {"x": {"type": "string"}}}}
        self.assertEqual(compare(BASE, variant(tools=changed), {}), ["tools click: embedded-proxy: input schema changed"])

    def test_cli_mcp_and_daemon_breaks_are_incompatible(self):
        retyped = {**BASE["manifest"], "subcommands": [{"name": "serve", "args": [{"name": "socket", "type": "path"}]}]}
        self.assertEqual(compare(BASE, variant(manifest=retyped), {}), ["subcommands serve socket: argument ('socket', 'string') removed or retyped"])
        self.assertEqual(compare(BASE, variant(manifest={**BASE["manifest"], "features": {}}), {}), ["manifest feature:portal_input: True -> None"])
        self.assertEqual(compare(BASE, variant(protocolVersion="2026-01-01"), {}), ["mcp protocolVersion: embedded-proxy: 2025-06-18 -> 2026-01-01"])
        self.assertEqual(
            compare(BASE, variant(capabilities={"tools": {"listChanged": True}}), {}),
            ["mcp capability:resources: embedded-proxy: removed", "mcp capability:tools: embedded-proxy: changed"],
        )
        self.assertEqual(compare(BASE, variant(daemon={**DAEMON, "contract_version": "0.9.0"}), {}), ["daemon contract_version: 0.8.0 -> 0.9.0"])
        self.assertEqual(compare(BASE, {**BASE, "forms": {}}, {}), ["form embedded-proxy: not probed for the candidate"])

    def test_recorded_changes_are_allowed(self):
        allowed = {"tools": {"zoom": "removed upstream; GOAT stopped using it in goat-si/goat#0"}}
        self.assertEqual(compare(BASE, variant(tools={"click": MCP["tools"]["click"]}), allowed), [])

    def test_output_schema_additions_are_compatible(self):
        widened = click_output(extra={"type": "string"})
        widened["properties"]["rows"]["items"]["properties"]["name"] = {"type": "string"}
        narrower = click_output(state={"type": "string", "enum": ["on"]})
        for outputs in ({"click": widened}, {"click": narrower}, {**MCP["outputSchemas"], "zoom": {"type": "object"}}):
            self.assertEqual(compare(BASE, variant(outputSchemas=outputs), {}), [])

    def test_output_schema_breaks_are_incompatible(self):
        cases = [
            ({}, "removed"),
            ({"click": click_output(state=None)}, "property state removed"),
            ({"click": click_output(ok={"type": ["boolean", "null"]})}, "property ok type ['boolean'] -> ['boolean', 'null']"),
            ({"click": click_output(state={"type": "string", "enum": ["on", "off", "auto"]})}, "property state enum gained 'auto'"),
            ({"click": {**click_output(), "required": []}}, "property ok no longer required"),
            ({"click": click_output(rows={"type": "array", "items": {"type": "object", "properties": {}}})}, "property rows[].id removed"),
        ]
        for outputs, detail in cases:
            self.assertEqual(
                compare(BASE, variant(outputSchemas=outputs), {}),
                [f"tools click:outputSchema: embedded-proxy: output schema {detail}"],
                detail,
            )
        allowed = {"tools": {"click:outputSchema": "GOAT no longer reads state; goat-si/goat#0"}}
        self.assertEqual(compare(BASE, variant(outputSchemas={"click": click_output(state=None)}), allowed), [])

    def test_advertised_output_unions_are_compared_by_variant(self):
        # The driver advertises {"type": "object", "anyOf": [success, refusal]}
        # (cua-driver-contract outputs.rs advertised_output_schema).
        def advertised(effect=("confirmed", "refused"), delivery=None, extra=None, required=("effect",)):
            success = {"type": "object", "additionalProperties": False, "required": list(required), "properties": {
                "effect": {"type": "string", "enum": list(effect)},
                "delivery": delivery or {"type": ["object", "null"], "properties": {"mode": {"type": "string"}}},
                **(extra or {}),
            }}
            refusal = {"type": "object", "required": ["error"], "properties": {"error": {"type": "object", "properties": {
                "code": {"type": "string"}}}, "effect": {"const": "refused"}}}
            return {"type": "object", "anyOf": [success, refusal]}

        def breaks(new):
            return compare(variant(outputSchemas={"click": advertised()}), variant(outputSchemas={"click": new}), {})

        self.assertEqual(breaks(advertised()), [])
        self.assertEqual(breaks(advertised(extra={"note": {"type": "string"}})), [])
        self.assertEqual(breaks(advertised(effect=("confirmed",))), [])
        # Nullable spelled as a union instead of a type list is the same output.
        nullable_union = {"anyOf": [{"type": "object", "properties": {"mode": {"type": "string"}}}, {"type": "null"}]}
        self.assertEqual(breaks(advertised(delivery=nullable_union)), [])
        self.assertEqual(compare(variant(outputSchemas={"click": advertised(delivery=nullable_union)}),
                                 variant(outputSchemas={"click": advertised()}), {}), [])

        gained = breaks(advertised(effect=("confirmed", "refused", "maybe")))
        self.assertEqual(gained, ["tools click:outputSchema: embedded-proxy: output schema variant 0: property effect enum gained 'maybe'"])
        unrequired = breaks(advertised(required=()))
        self.assertEqual(unrequired, ["tools click:outputSchema: embedded-proxy: output schema variant 0: property effect no longer required"])
        mode_removed = breaks(advertised(delivery={"type": ["object", "null"], "properties": {}}))
        self.assertTrue(mode_removed and all("property delivery.mode removed" in line for line in mode_removed), mode_removed)


class ValidateTest(unittest.TestCase):
    def test_complete_surface_with_the_expected_identity_passes(self):
        self.assertEqual(validate(BASE, "candidate", "1.0.0", ["click"]), [])

    def test_empty_or_misidentified_surfaces_fail(self):
        empty = variant(manifest={"binary_version": "1.0.0"}, tools={}, protocolVersion=None)
        self.assertEqual(len(validate(empty, "candidate", "1.0.0", ["click"])), 3)
        self.assertEqual(len(validate({**BASE, "forms": {}}, "candidate", "1.0.0", [])), 1)
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
        conn, _ = server.accept(); conn.recv(4096)
        reply = json.dumps({"ok": True, "result": {"driver_version": "1.0.0"}}) + "\\n"
        if mode == "trickle":
            for byte in reply:
                conn.sendall(byte.encode()); time.sleep(0.5)
        conn.sendall(reply.encode()); conn.close()
        if mode == "ignore-eof":
            while True:
                time.sleep(1)
        sys.stdin.read()
        sys.exit(0)
    if mode == "reject-proxy" and "--socket" in sys.argv:
        sys.exit(91)
    if mode == "stderr-flood":
        sys.stderr.write("x" * (1 << 20)); sys.stderr.flush()
    for line in sys.stdin:
        msg = json.loads(line)
        if "id" not in msg:
            continue
        result = {"protocolVersion": "2025-06-18", "capabilities": {}} if msg["method"] == "initialize" else {"tools": [{"name": "click", "inputSchema": {}}]}
        if mode == "output-schema" and msg["method"] == "tools/list":
            result["tools"][0]["outputSchema"] = {"type": "object"}
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
        check_protocol.REPLY_SECONDS, check_protocol.EXIT_SECONDS = 3, 1

    def tearDown(self):
        check_protocol.REPLY_SECONDS, check_protocol.EXIT_SECONDS = self.saved
        self.dir.cleanup()

    @contextlib.contextmanager
    def mode(self, name):
        os.environ["FAKE_MODE"] = name
        try:
            yield
        finally:
            del os.environ["FAKE_MODE"]

    def surface(self, *args):
        return mcp_surface([str(self.driver), "mcp", *args])

    def daemon(self):
        with embedded_daemon(self.driver) as (_, metadata):
            return metadata

    def test_a_driver_flooding_stderr_does_not_hang_the_check(self):
        with self.mode("stderr-flood"):
            self.assertEqual(self.surface()["tools"], {"click": {}})

    def test_tools_list_output_schemas_are_kept(self):
        with self.mode("output-schema"):
            surface = self.surface()
        self.assertEqual(surface["tools"], {"click": {}})
        self.assertEqual(surface["outputSchemas"], {"click": {"type": "object"}})

    def test_a_repeated_cursor_fails_instead_of_looping(self):
        with self.mode("repeat-cursor"), self.assertRaises(SystemExit) as raised:
            self.surface()
        self.assertIn("repeated cursor", str(raised.exception))

    def test_a_driver_rejecting_the_embedded_proxy_form_fails(self):
        with self.mode("reject-proxy"), self.assertRaises(SystemExit) as raised:
            self.surface("--embedded", "--socket", "x", "--host-bundle-id", HOST_ID)
        self.assertIn("closed stdout", str(raised.exception))

    def test_a_daemon_metadata_reply_is_read_and_the_daemon_exits(self):
        with self.mode("plain"):
            self.assertEqual(self.daemon(), {"driver_version": "1.0.0"})

    def test_a_trickling_metadata_reply_fails_at_the_deadline(self):
        started = time.monotonic()
        with self.mode("trickle"), self.assertRaises(SystemExit) as raised:
            self.daemon()
        self.assertIn("metadata request took longer than", str(raised.exception))
        self.assertLess(time.monotonic() - started, 10)

    def test_a_daemon_that_outlives_its_parent_pipe_fails(self):
        with self.mode("ignore-eof"), self.assertRaises(SystemExit) as raised:
            self.daemon()
        self.assertIn("after its parent pipe closed", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
