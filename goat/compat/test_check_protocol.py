import unittest

from check_protocol import compare

BASE = {
    "protocolVersion": "2025-06-18",
    "capabilities": ["resources", "tools"],
    "tools": {"click": {"type": "object", "properties": {"x": {"type": "number"}}}, "zoom": {"type": "object"}},
    "manifest": {
        "schema_version": 1,
        "mcp_invocation": {"args": ["mcp"]},
        "subcommands": [{"name": "serve", "args": [{"name": "socket", "type": "string"}]}],
    },
}


def variant(**changes):
    candidate = {**BASE, "tools": dict(BASE["tools"]), "manifest": dict(BASE["manifest"])}
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

    def test_cli_and_mcp_breaks_are_incompatible(self):
        retyped = {**BASE["manifest"], "subcommands": [{"name": "serve", "args": [{"name": "socket", "type": "path"}]}]}
        self.assertEqual(
            compare(BASE, variant(manifest=retyped), {}),
            ["subcommands serve socket: argument ('socket', 'string') removed or retyped"],
        )
        self.assertEqual(compare(BASE, variant(protocolVersion="2026-01-01"), {}), ["mcp protocolVersion: 2025-06-18 -> 2026-01-01"])
        self.assertEqual(compare(BASE, variant(capabilities=["tools"]), {}), ["mcp capability:resources: removed"])

    def test_recorded_changes_are_allowed(self):
        allowed = {"tools": {"zoom": "removed upstream; GOAT stopped using it in goat-si/goat#0"}}
        self.assertEqual(compare(BASE, variant(tools={"click": BASE["tools"]["click"]}), allowed), [])


if __name__ == "__main__":
    unittest.main()
