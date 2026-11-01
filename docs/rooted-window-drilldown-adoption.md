# Rooted window drill-down: platform adoption

The core supports a separate snapshot for a complete subtree walk. The macOS,
Linux and Windows backends still refuse `root_element_token` with
`root_unsupported`; Windows adoption is a separate change and must prove the
native behavior before the feature is considered complete.

Windows starts with [`UiaSnapshot`](../rust/crates/platform-windows/src/uia/snapshot.rs)
and the [`get_window_state` implementation](../rust/crates/platform-windows/src/tools/impl_.rs).
The other payloads are [`AxSnapshot`](../rust/crates/platform-macos/src/ax/snapshot.rs)
and [`AtspiSnapshot`](../rust/crates/platform-linux/src/atspi/snapshot.rs).

An adopting backend must:

1. Call `window_state_view::validate_root_args` before platform work. Rooted
   reads do not accept `since`, `display_only`, or screenshot-only mode.
2. Implement `SnapshotPayload::structural_path` using stable child ordinals
   relative to the payload root. The default returns `None`, which refuses
   rooted reads. A scoped payload's path is relative to its own root; core
   prepends the accumulated parent path for a subsequent drill-down.
3. Fetch a fresh process fingerprint outside the store lock and call
   `SnapshotStore::begin_scoped_capture` with the qualified root token, exact
   window and runtime session. An unprotected parent or a foreign session is
   refused.
4. Call `ScopedCapture::validate_root` outside the store lock. Its callback
   must re-resolve the structural path from the live window, verify the process
   instance and exact window, and compare the result with the selected root.
   Return the freshly resolved native element; a retained AX/UIA handle or a
   cached AT-SPI identity alone is not proof of liveness. Walk only the element
   returned by `ValidatedScopedCapture::root_element`.
5. Submit `ScopedWalk::Complete(payload)` only when the platform can assert
   observation completeness for the requested subtree. Otherwise submit
   `ScopedWalk::Incomplete(reasons)` with U02b reasons. Do not expose partial
   rows or tokens on an incomplete result. A stale parent is refused at commit.
6. For a complete publication, return its new `snapshot_id`,
   `drilldown_status: "complete"`, `observation_complete: true`, and
   `scope: {root_token, parent_snapshot_id}`. The parent ID is available from
   `ValidatedScopedCapture::parent_snapshot_id` before committing. Apply the
   normal query projection and call `window_state_view::apply_scoped` with the
   original root token and independently selected parent ID. Scoped responses
   do not replace the full-window `since` baseline or screenshot.

The store keeps scoped entries independently of full-window LRU entries, and
invalidates them with their parent. Scoped tokens resolve only by exact ID;
bare row numbers, screenshot and zoom lookups continue to use the full
snapshot. A retired session also invalidates in-flight rooted captures, even
if a later session reuses the same name. A platform must also prove its action
resolver can safely resolve scoped elements, including moved, vanished and
reattached roots, before advertising rooted walks. The same selection and
fresh re-resolution API can be used for a scoped action token: its callback
receives the accumulated path through the original full window. It must never
retry a refused rooted read without the root argument.

On the wire, incomplete, stale-parent and unsupported results remain
refusals with their respective codes. The typed SDK returns their
`WindowStateOutput` status and diagnostics when the refusal also identifies
the pid and window; an old daemon's unknown-argument refusal remains an error.
