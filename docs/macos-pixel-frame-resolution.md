# macOS pixel frame resolution without a per-action capture

This C3 change removes the screenshot used only to measure the backing scale
of window-local pixel actions. It changes no tool schema, GOAT pin, input
delivery route, or capture ID admission rules.

## Plan and implementation

1. `rust/crates/platform-macos/src/tools/px_frame.rs`: keep the shared resolver
   used by click, scroll, drag, right-click, move-cursor, hotkey and focus
   read-back. Decide from lazy metadata/capture inputs so tests can prove
   whether capture ran without a clock or an app.
2. `rust/crates/platform-macos/src/tools/get_window_state.rs`: after successful
   capture publication, remember its raw dimensions and the WindowServer
   bounds used to validate it. Delivered/resized PNG dimensions remain in the
   existing screenshot-to-action transform and are never mistaken for native
   dimensions here.
3. `rust/crates/platform-macos/src/tools/capture_binding.rs`: reuse the same
   resolver for live native dimensions, removing the additional screenshot
   previously taken during capture-bound click admission. Keep the owner,
   generation, target, size, coordinate and atomic consumption checks.

The resolver first requires a live WindowServer record and finite, positive
bounds. A matching publication from that window's current PID wins only when
all four bounds fields match exactly. Its dimensions still pass the existing
1x/2x coherence validator; a mismatch refuses rather than being hidden by
another source.

Otherwise, the resolver checks the active display with the largest overlap
with the window. Both axes of the current display mode must prove the same
1x or 2x pixel-to-point ratio. A tie across different scales, no overlap,
missing display metadata or an unsupported ratio requires a capture. There
is no guessed 1x or primary-display fallback. Fresh capture failures and frame
mismatches keep their existing refusal paths, including
`px_window_not_found`, `px_frame_mismatch` and `px_capture_unavailable`.

The process-wide publication index holds at most 128 geometry records, replaces
records by window ID and checks PID on lookup. It holds no PNGs, tokens,
session bindings or permission grants. The immutable capture service remains
the authority for capture-bound admission. Cache lookup clones one record and
releases its mutex before display queries or capture. Failed publications and
tree-only reads do not overwrite the last published frame.

## Deterministic verification

Final source verification (2026-10-28 KST): `kache doctor` passed with Rust
1.97.1; `cargo test -p platform-macos --lib` exited 0 with 719 passed, zero
failed and seven existing ignored tests. This includes 17 pixel-frame tests
and eight capture-binding tests. Changed-file rustfmt and `git diff --check`
also passed. The ignored live/platform tests were not enabled.

From `rust/`, run:

```sh
kache doctor
cargo test -p platform-macos --lib
cargo clippy -p platform-macos --lib --tests --no-deps --message-format=json
rustfmt --edition 2021 --check crates/platform-macos/src/tools/px_frame.rs crates/platform-macos/src/tools/get_window_state.rs crates/platform-macos/src/tools/capture_binding.rs
git diff --check
```

Decision tests cover cached 1x/2x frames during a capture stall, every changed
bounds field, cold Retina metadata, conflicting publication dimensions,
fallback capture count and errors, invalid bounds, bounded replacement and
owner isolation, exact native dimensions with capture rounding, and display
selection across negative origins and mixed scales.

Mutation proof: disabling both metadata sources restores unconditional capture
and fails four decision tests; restoring them passes. The new tests use no
sleeps or elapsed-time assertions. Existing capture-binding tests also cover
resize refusal without consumption, session retirement, another generation,
coordinate rejection and one-time consumption.

An initial strict clippy run without `--no-deps` was blocked by the
`clippy::type_complexity` diagnostic at
`rust/crates/cua-driver-core/src/snapshot_compaction.rs:702`. Strict package-only
clippy (`--lib --no-deps -- -D warnings`) was then blocked by
`clippy::needless_borrow` at `rust/crates/platform-macos/src/input/keyboard.rs:348`
and `clippy::unnecessary_lazy_evaluations` at
`rust/crates/platform-macos/src/tools/mod.rs:308`. All three files are unchanged
from the base. Strict clippy remains blocked; no warnings were suppressed and
no unrelated code was edited. The JSON command above retains diagnostics,
includes test code and allows checking the changed files separately.

The JSON clippy run exited 0 with no diagnostics in the three changed Rust
files. Its four warning records are the two unchanged macOS warnings above,
each reported once for the library and once for the test target.

## Coordinator-only live checks (not run by this lane)

1. Use a disposable helper build from this branch and the pre-change base
   `657bdfb11850dc1af9ae1f990c99db2f5cfe3447`. Keep the same permitted app,
   window, action, capture resize settings and delivery mode. Do not change the
   GOAT pin for this comparison.
2. Enable the driver's `CUA_LOG=platform_macos::tools::px_frame=debug` and
   existing `CUA_DRIVER_GOAT_LATENCY_TRACE=1` diagnostics. For an embedded host,
   verify that its diagnostic launch configuration actually forwards these
   variables. `resolved window pixel frame` reports `source` as `published`,
   `display` or `capture`. This reports frame selection, not the action's
   success or a measurement of capture latency.
3. Read a window with a screenshot, then issue window-pixel click, right-click,
   scroll, drag and cursor actions while its bounds stay fixed. Include a
   click with the returned capture ID. Verify `source="published"`, the
   expected target/effect, and no new screenshot solely to resolve the frame.
   Compare the action latency distributions against the base, including an
   Electron window; report host, build SHAs, sample count and resize settings.
4. Move/resize a permitted window and repeat: the old publication must not
   prove the frame. On a known 1x/2x display expect `source="display"`.
   Exercise mixed-scale displays, negative origins and the equal-overlap
   boundary separately. Re-snapshot before reusing screenshot coordinates.
5. With a safely simulated capture-backend failure in the coordinator's test
   environment, repeat fixed-bounds published-frame and display-backed
   actions. Frame resolution must remain independent of capture. When neither
   metadata source proves the frame, capture failure must still refuse.
6. Close the target window and retry its stale ID: expect
   `px_window_not_found` and no input. Exercise a mismatched capture fixture:
   expect `px_frame_mismatch`; do not reinterpret local coordinates as desktop
   coordinates. Confirm existing capture-bound resize/session/consumption
   refusals in the real helper as well.

These are pending runtime checks. Unit tests establish source selection and
refusal behavior; they do not establish a measured latency reduction, live
ScreenCaptureKit recovery, or release qualification.
