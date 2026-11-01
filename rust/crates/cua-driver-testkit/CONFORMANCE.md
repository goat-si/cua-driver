# Platform conformance handoff

The `conformance` module probes a fixture using the normal `Driver` interface.
The fixture supplies a pid/window, two identically named Save buttons, a text
field, a disabled button, a destructible element, a read-error injection with
an independent error count, and an independent dispatch counter. The two
sessions must address the same daemon. MCP fixtures can use a peer proxy. CLI
fixtures must first prove session continuity with explicit session labels and a
read/act round trip; otherwise set `session_evidence` to an error, producing
`NotSupported`. Run each destructive probe on a fresh fixture.

| Contract | Expected evidence | macOS AX | Windows UIA | Linux AT-SPI |
|---|---|---|---|---|
| Ambiguity | ambiguous, two candidates, zero dispatches | Required | Required | Required |
| Stale | destroy, fresh read invalidates token, stale refusal and not_delivered/safe | Required | Required | Required |
| ForeignSession | session A token refused in B, zero dispatches | Required | Required | Required |
| GoneReadError | actual injected read error, gone_unconfirmed/lookup_failed | Required | Required | Required |
| Disposition | well-formed delivery/retry pair on every refusal above | Required | Required | Required |
| Actionability | disabled action refused before dispatch | Not wired | Not wired | Not wired |
| Completeness | observation_complete for adopting platform | Not wired | Not wired | Not wired |

Outcomes are `Pass`, `Fail { observed }`, `NotWired { reason }`, and
`NotSupported { reason }`. Only `Pass` satisfies `assert_required`. Each
platform job declares its required set in code; removing an unwired item must
be explicit and reviewable. The matrix is a handoff requirement, not a claim
that any platform fixture has been implemented or passed. Contracts 6 and 7
remain `NotWired` until platform adoption. Text editing, window identity,
batch session overrides and FFI identity are outside this battery.
