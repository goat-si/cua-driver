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
| Stale | destroy, fresh read invalidates token, stale_element_token with a valid disposition that never claims delivery (not_delivered/safe or unknown/unknown), and no dispatch | Required | Required | Required |
| ForeignSession | session A token refused in B, zero dispatches | Required | Required | Required |
| GoneReadError | actual injected read error, gone_unconfirmed/lookup_failed | Required | Required | Required |
| Disposition | well-formed delivery/retry pair on every refusal above | Required | Required | Required |
| Actionability | disabled action refused before dispatch | Not wired | Not wired | Not wired |
| Completeness | observation_complete for adopting platform | Not wired | Not wired | Not wired |

An incomplete tree cannot establish element absence or uniqueness. Batch
lookups report `incomplete_observation`, while observed ambiguity still
requires at least two candidates and zero dispatches. Incomplete-tree probes
need their own injected omissions and independent dispatch evidence; they do
not replace the ambiguity or read-error contracts. The optional
`incomplete_absence` and `incomplete_uniqueness` probes require a fixture to
count injected incomplete reads. The latter also requires exactly one named,
addressable baseline candidate and checks that the refusal lists it.

Outcomes are `Pass`, `Fail { observed }`, `NotWired { reason }`, and
`NotSupported { reason }`. Only `Pass` satisfies `assert_required`. Each
platform job declares its required set in code; removing an unwired item must
be explicit and reviewable. The matrix is a handoff requirement, not a claim
that any platform fixture has been implemented or passed. Contracts 6 and 7
remain `NotWired` until platform adoption. Text editing, window identity,
batch session overrides and FFI identity are outside this battery.

### Observation scope and closed menus

Closed menubar menus, context/popup menus, and collapsed combobox lists are
outside the default observation scope. Open menus and contents explicitly
requested by a read are in scope; a failed attempt to read them is incomplete.
An exclusion alone does not make `observation_complete` false, and a
`collapsed_menu` row boundary is permitted without a read-level reason. Other
skips, cuts, untrusted reads and timeouts remain incomplete.

`observation_scope_exclusions` optionally records `closed_menus_not_expanded`.
Its `element_indices` identify excluded owner rows in the returned snapshot's
index namespace, sorted and deduplicated; omission means ownership is unknown
or could not be retained. Absence of the field does not prove full scope,
including for old producers. Locate uses completeness and exclusions in
absence and uniqueness proofs: with an exclusion, those proofs are unknown for selectors that could
match excluded content, including canonical or native menu/menuitem roles,
untyped name/text selectors, or selectors scoped beneath an excluded owner.
Selectors typed to a non-menu role are unaffected by this exclusion.

Platform handoff (not implemented by this contract change):

For text-folding completeness, Windows must use
`fold_actionless_text_subtrees_with_reasons` at both UIA folding sites, merge
every returned reason into the corresponding walk budget via `record_skip`,
implement `CompactableNode::offscreen` from observed UIA visibility, and
implement `CompactableNode::boundary_mut` and retain node boundaries in output.
macOS must do the same wherever it folds before asserting observation
completeness; Linux must do so if it adds folding. A truncated folded
snippet makes the read incomplete even if the native walk was exhaustive.
The reason-aware path marks absorbed rows with `folded_children`, including
lossless folds; that marker does not make the read incomplete by itself.
Merge the returned read reasons, not every node-boundary reason: copying
`folded_children` into read-level reasons would incorrectly mark it incomplete.
Locators apply its role-specific absence, uniqueness and nth guard even when
the producer leaves completeness unasserted. Nodes without a boundary slot
keep their child rows. Platforms must also preserve the emitted markdown marker
and typed boundary; the legacy wrapper does not supply this evidence.

- macOS: replace only the intentional `AXMenuBarItem` collapse's
  `note_skipped` with an exclusion; retain unrelated skips. Existing `query`
  and `full_output:true` request menu traversal.
- Linux: record exclusions only on positive closed evidence; failed state or
  peek reads remain uncertainty.
- Windows: add a closed-menu policy hook using positive collapsed
  ExpandCollapse state; missing/unknown state is not evidence. Cover the MSAA
  fallback as well.
- All platforms must cover collapsed combobox lists. A targeted menu/item read
  attempts that scope and reports failure as incomplete; it must not silently
  open UI. Platform implementations must map walk-local owners to the final
  returned snapshot indices using `WalkOutcome::apply_with_owner_mapping`, or
  preserve the exclusion with unknown ownership when a row is dropped or its
  identity after renumbering cannot be proven. Plain `apply` conservatively
  leaves all recorded ownership unknown.
