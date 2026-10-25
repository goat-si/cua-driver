WS13 measurement procedure — goat-si/goat#1593 option (A)

No latency measurements were performed on the implementation Mac: it has no Accessibility or Screen Recording TCC grants. The harness below has passed syntax checks and fake MCP helper checks, but has not been exercised against a granted helper.

1. On the TCC-granted WS13 Mac, record hardware, macOS, power mode, display count, load averages, helper identity/version and both driver SHAs. Use the same signed dev helper identity that has the grants from the original #1593 measurements. Keep the user idle with Terminal (or another fixed app) frontmost. Open one English Calculator window in Basic mode, press Escape to expose “All Clear”, then leave Calculator behind the user's app. Do not front Calculator during the samples. Record human HID input and discard/re-run an affected run. Keep the same cursor settings in before/after runs.

2. Build two isolated source snapshots from the fork, without changing another checkout's branch. Run on WS13, replacing FORK with its local clone path:

```sh
export PATH=$HOME/.cargo/bin:$PATH
export TMPDIR=/private/tmp
FORK=/path/to/cua-driver
WS13_ROOT=$(mktemp -d /private/tmp/ws13-window-latency.XXXXXX)
mkdir "$WS13_ROOT/before" "$WS13_ROOT/after"
git -C "$FORK" rev-parse origin/main
git -C "$FORK" rev-parse goat/window-latency
git -C "$FORK" archive origin/main | tar -x -C "$WS13_ROOT/before"
git -C "$FORK" archive goat/window-latency | tar -x -C "$WS13_ROOT/after"
(cd "$WS13_ROOT/before/rust" && cargo build --locked -p cua-driver --release --target aarch64-apple-darwin)
(cd "$WS13_ROOT/after/rust" && cargo build --locked -p cua-driver --release --target aarch64-apple-darwin)
```

For this handoff the before source is `2dd4a301` (origin/main at setup), after is `ab02df832f78998e9cb7f596a8459e0888407a68`. If main has advanced, record its exact SHA and report that difference rather than silently comparing to the older source. Before means a current fork-main build, not the upstream compatibility baseline.

3. Copy the granted signed helper bundle into two disposable directories under WS13_ROOT; do not replace the live install. In each copy replace only `Contents/Resources/cua-driver/cua-driver` with that variant's release binary. Preserve the adjacent cursor theme/resources. Sign the replaced driver, then the enclosing helper bundle, using the same Developer ID team, bundle id, entitlements and designated requirement as the working dev helper. For example, when the original signatures already contain those metadata:

```sh
# Set WS13_SIGN_ID to the existing Developer ID Application signing identity.
# BEFORE_APP and AFTER_APP are the two disposable copied .app bundles.
for WS13_VARIANT in before after; do
  if [ "$WS13_VARIANT" = before ]; then WS13_APP=$BEFORE_APP; else WS13_APP=$AFTER_APP; fi
  cp "$WS13_ROOT/$WS13_VARIANT/rust/target/aarch64-apple-darwin/release/cua-driver" "$WS13_APP/Contents/Resources/cua-driver/cua-driver"
  codesign --force --options runtime --sign "$WS13_SIGN_ID" "$WS13_APP/Contents/Resources/cua-driver/cua-driver"
  codesign --force --sign "$WS13_SIGN_ID" --preserve-metadata=identifier,requirements,entitlements,flags "$WS13_APP"
  codesign --verify --deep --strict "$WS13_APP"
done
```

Use the launcher's `Contents/MacOS/GOAT Computer Use`, not the internal service executable. Verify grants through that helper before measuring. Keep the original helper unchanged. Do not use an ad-hoc signature for the comparison: it changes TCC identity.

4. Write an absolute-path Calculator-only capability manifest (same file for both variants):

```yaml
version: 3
resources:
  desktop:
    display: true
  apps:
    - bundle_id: "com.apple.calculator"
allow:
  tools:
    - "list_windows"
    - "get_window_state"
    - "get_cursor_position"
    - "click"
    - "run_steps"
```

5. Run `goat/bench/latency/measure.mjs` from the repository root. It starts the helper via native pipes as in the original comment:

`GOAT Computer Use --mcp --capability-manifest /absolute/manifest.yaml`

It initializes MCP, obtains Calculator's pid/window, reads the AX elements without a screenshot, finds the All Clear button token, performs one direct preflight click, performs five warmup calls, then runs N=50 sequential background `click(action:press, element_token:...)` calls with no result check/capture. The preflight and every direct-mode call require successful AX (`path:ax` / `route:accessibility`) and `effect:unverifiable`. The batch variant repeats three identical clicks per `run_steps`, including each warmup call, omitting `observe`, wait_for and expect. It rejects refusals/failed steps and reported recovery/fallback (see Batch validity below). The token stays valid because no new snapshot is requested during the run. Each measurement covers the call over helper stdio (relay + driver, excluding lease proxy/model); preflight, warmup and verification are outside the timed interval.

```sh
unset CUA_DRIVER_GOAT_LATENCY_TRACE CUA_DRIVER_WINDOW_CHANGE_TIMEOUT_MS CUA_DRIVER_WINDOW_CHANGE_POLL_MS
node goat/bench/latency/measure.mjs "$BEFORE_APP/Contents/MacOS/GOAT Computer Use" "$WS13_ROOT/manifest.yaml" 50 direct "$WS13_ROOT/before-direct"
node goat/bench/latency/measure.mjs "$AFTER_APP/Contents/MacOS/GOAT Computer Use" "$WS13_ROOT/manifest.yaml" 50 direct "$WS13_ROOT/after-direct"
node goat/bench/latency/measure.mjs "$BEFORE_APP/Contents/MacOS/GOAT Computer Use" "$WS13_ROOT/manifest.yaml" 50 batch "$WS13_ROOT/before-batch"
node goat/bench/latency/measure.mjs "$AFTER_APP/Contents/MacOS/GOAT Computer Use" "$WS13_ROOT/manifest.yaml" 50 batch "$WS13_ROOT/after-batch"
```

Repeat in reverse order to distinguish improvement from load/thermal drift. No `/usr/bin/sample` during the primary runs. If sampling is wanted, label a separate 20 s run as in #1593. Report N, median and nearest-rank p95 (`sorted[ceil(.95*N)-1]`); the harness calculates both. For batches report call duration and per-step trace segments separately; dividing a three-step call by three does not identify a particular step. No early quiet exit or timeout override is permitted in this comparison.

6. Trace breakdown is a separate after-only diagnostic run. GOAT's current Swift helper filters driver environment names (`apps/desktop/native/computer-use-helper/main.swift`, `hostSettableNames`); merely setting the new variable on the helper will not reach the driver. On WS13 make a disposable diagnostic helper build whose sole source change adds `"CUA_DRIVER_GOAT_LATENCY_TRACE"` to `hostSettableNames`, using the existing helper build/signing workflow and the same Developer ID identity. Embed the after binary and re-sign it as above. Do not publish that helper or change the live installation; this fork implementation changes no GOAT files. Set TRACE_APP to that diagnostic bundle and confirm `cua_goat_latency` records actually arrive on stderr:

```sh
CUA_DRIVER_GOAT_LATENCY_TRACE=1 node goat/bench/latency/measure.mjs "$TRACE_APP/Contents/MacOS/GOAT Computer Use" "$WS13_ROOT/manifest.yaml" 50 direct "$WS13_ROOT/after-trace-direct"
CUA_DRIVER_GOAT_LATENCY_TRACE=1 node goat/bench/latency/measure.mjs "$TRACE_APP/Contents/MacOS/GOAT Computer Use" "$WS13_ROOT/manifest.yaml" 50 batch "$WS13_ROOT/after-trace-batch"
WS13_COLD=1 CUA_DRIVER_GOAT_LATENCY_TRACE=1 node goat/bench/latency/measure.mjs "$TRACE_APP/Contents/MacOS/GOAT Computer Use" "$WS13_ROOT/manifest.yaml" 50 direct "$WS13_ROOT/after-trace-cold"
```

For N=50, the warm direct file should contain 56 click records: one preflight, five warmups and 50 measured clicks. The warm batch file should contain 166 click records: one direct preflight, 15 warmup clicks and 150 measured clicks; group the measured records in threes. The cold direct file should contain 51 click records: one preflight and 50 measured clicks. The harness writes one `ws13_latency preflight_clicks=1` marker to each `.stderr` file. The parser requires that marker and discards the first click plus the mode's warmups; the marker declares a count rather than a boundary between independently delivered stdout/stderr streams. Rerun older unmarked traces. Cold mode omits warmups and idles 5 s outside each timed call, beyond the existing key-focus tap idle teardown (3 s tail + 1 s teardown delay); label it “idle-cooled taps”, not “fresh process”. To characterize true process-cold startup, separately take one first click from each of 50 fresh helper processes, excluding initialization/AX snapshot from the timed interval. Do not mix those samples with warm results.

For each retained record report median/p95 in microseconds divided by 1000 for: snapshot_us, ax_resolve_us, protect_us, focus_settle_us, action_guarded_us, action_blocking_us, lease_drop_us, detector_us, detector_sleep_us, detector_enumeration_us, other_us, and detector_enumerations. Tabulate detector_exit and tail counts. request_id is generated per platform invocation, not the wire JSON-RPC id; sequential calls and the recorded call order provide the mapping. Three batch steps get three ids. Never log element tokens, typed text or window contents into the timing report.

Use the supplied parser for the three trace files (verified with fake-helper traces):

```sh
python3 goat/bench/latency/summarize_trace.py "$WS13_ROOT/after-trace-direct.stderr" DIRECT
python3 goat/bench/latency/summarize_trace.py "$WS13_ROOT/after-trace-batch.stderr" BATCH
python3 goat/bench/latency/summarize_trace.py "$WS13_ROOT/after-trace-cold.stderr" COLD
```

It requires exactly 50 retained calls (150 click records for batches), checks unique ids and successful outcomes, verifies total reconciliation, and reports batch positions separately.

Stage columns are inclusive and overlap: do not sum them. `measured_us` is their interval union; `measured_us + other_us == total_us` (integer rounding accounted for). `action_blocking_us` includes blocking preparation, verification and foreground restoration; `action_guarded_us` covers the whole guarded future including blocking queue wait. Synchronous guard drops/restores are timed; detached detector/guard watchers are excluded. `detector_exit=report_window tail=detached` is the expected quiet result; the detached tail still runs to the original deadline. `skipped`, `change`, `timeout`, `join_error` and `tail=spawn_failed` remain distinguishable. The platform total excludes core authorization/coordinator/recording/relay; compare it to the helper RTT without labeling the difference “lease proxy”. Cancellation yields `outcome=interrupted` with partial metrics.

7. Optional WS15 semantic matrix: on both variants, N>=50 each for quiet, a same-process layer-0 window appearing at 250 ms, and an other-process layer-0 window appearing at 250 ms; repeat with a 450 ms appearance to exercise deferred reporting on the next result. Use an actual fixture whose trigger schedules the window, not arbitrary harness sleeps to guess state. Record current-result versus next-result delivery exactly once and verify that no early exit was introduced. Cover direct and batch positions 1/2/3 (100/100/300 ms reporting). No such fixtures were run on the implementation Mac.

NOT RUN: helper launch against TCC, N=50 timings against a real helper, live trace segments, cold/warm measurements, delayed same/other-process fixtures, lease proxy/model overhead. No speedup number is claimed by this implementation.

## Batch validity

`run_steps` reports only each step's first message line and omits structured effect. The click's suspected-no-op warning appears after that first line, so per-step suspected no-ops cannot be detected through `run_steps`. The direct AX preflight verifies only the first click's route/effect; direct mode checks every call. Batch checks reject visible no-op/skip messages but cannot establish each step's effect.

`measure.mjs` accepts a batch only when it returns exactly three successful steps without reported recovery/fallback or skips. The step report's recovery/fallback fields in `batch_tools.rs` are `refound` (a note string for stale-target retries or menu-bar rerouting) and `delivery_fallback` (`"foreground"` for a foreground retry); either field's presence is rejected, including `refound:true`. It also rejects `recovered:true`, a `recovery` field, `skipped:true`, and messages reporting recovery, no-ops or skips. Failed steps are rejected through `ok`; their `code`, `phase`, `hint` and `next_call` describe the failure or suggested recovery, rather than a successful recovery.
