# goat-cu: GOAT's fork of Cua Driver

This repository is [GOAT](https://github.com/goat-si/goat)'s fork of Cua Driver, the computer-use driver
from [trycua/cua](https://github.com/trycua/cua) (`libs/cua-driver`, MIT, Copyright (c) 2025 Cua AI, Inc.).
GOAT builds the driver from here and ships it inside its own signed helper app instead of downloading
upstream release binaries. The repository was named goat-si/cua-driver until 2026-11-01. Tracking issue: goat-si/goat#1595.

## History, branches and versions

- The history of this repository starts from one commit made on 2026-11-01. It holds Cua Driver as last synced from upstream (trycua/cua `main` `aeae5569473b`, after `cua-driver-rs-v0.38.0`) plus every GOAT change made before that date. The earlier history, including the upstream subtree split, is kept in a private archive.
- `main` is the only long-lived branch. Changes land through pull requests.
- Releases are tagged `cua-driver-rs-v<upstream version>-goat.<N>`, for example `cua-driver-rs-v0.38.0-goat.17`. Tags made before 2026-11-01 point into the archived history.

## What differs from upstream

- `vendor/` carries the monorepo files the driver needs to build on its own; see vendor/README.md.
- `LICENSE.md` is the monorepo's root license, which the subtree split leaves out.
- GOAT fixes, each linked to an issue here or in goat-si/goat. Every fix that is not GOAT-specific is also
  sent upstream as a separate pull request against trycua/cua `main`, and driver bugs are reported upstream.

## Signing

Release artifacts here are unsigned. GOAT signs the driver when it builds its helper app, so the helper
keeps its existing signing identity and the permissions people granted it. This repository holds no
signing certificates or secrets.

## Embedded process lifetime

On macOS and Linux, `mcp --direct --embedded`, `serve --embedded` and the
SDK's `__private-worker` belong to their launching process. macOS watches its
startup parent with kqueue; Linux uses a parent-death SIGTERM and rechecks the
parent after arming it. Linux delivers that signal when the launching thread
exits, so a host keeps the thread that created the driver alive while it owns
it. Existing MCP stdin EOF and `serve --parent-liveness-stdio` shutdown remain
supported.
Standalone daemons and the app's automatic daemon launch keep their lifetime.

These embedded runtimes own a process group with a pipe sentinel, so even a
SIGKILL of the driver ends its helpers. Driver-owned isolated browsers retain
their separate groups with the same containment; user-launched Linux apps
are detached from cleanup. Perception keeps its existing OS containment.
Sentinels require waitable SIGCHLD and exclusive reaping; embedded startup
rejects inherited SIGCHLD auto-reaping.

Parent death ends the group at once with SIGKILL, unlike the graceful stdin EOF
shutdown, so nothing in flight is finalized: an active recording is left
incomplete. A host that records should stop the recording, or close the
driver's stdin, before it exits.

Windows hosts must assign the driver to a Job Object with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and retain the job handle for the desired
lifetime. This fork does not create or validate that host-owned job; without
it, Windows has no equivalent parent-death guarantee. No Windows lifetime code
is changed by this Unix fix.

## Windows executable grants

On Windows, path-based application grants bind to the file at trusted manifest load:
volume/file ID, size, last-write time, and SHA-256. Each protected application or window
check re-reads that identity from the OS-attested executable path. A missing or unreadable
file fails closed, including files unavailable at manifest load. Replacing or modifying an
executable requires a fresh trusted manifest load; restarting the app alone does not renew
the grant. This covers `bundle_id` paths as well as `executable` grants (goat-si/goat#1382).

This is a launch-time identity boundary, not a signature requirement or an immutable
snapshot of an earlier app-listing/approval UI. It does not lock installed applications
against updates, pin loaded process memory, or make the OS check and subsequent action
atomic. Package identities, explicit PID/window grants, and non-Windows grants are unchanged.

## Syncing upstream

Upstream moves several times a day; we sync deliberately, not continuously.

1. In a trycua/cua clone, diff `libs/cua-driver` between the last synced upstream commit (recorded in the
   newest sync pull request, `aeae5569473b` at the 2026-11-01 restart) and the target tag or commit.
2. Review that diff for security and behavior changes.
3. Apply that diff to a branch from `main` as one pull request that names both upstream commits, refresh `vendor/`, and run `cargo deny check`. If the
   `uniffi-bindgen-react-native` pin in `typescript/package.json` changed, run
   `node scripts/build-node-runtime.mjs --update-lockfile` and commit `scripts/node-runtime/Cargo.lock`:
   the Node runtime builds with `--locked` against it, and CI checks it with
   `scripts/node-runtime/deny.toml`.
4. Open a pull request; CI runs the protocol-compatibility test and builds every platform. Slow unit
   and desktop-independent regression jobs run on release tags, `workflow_dispatch` and a daily
   scheduled run of `main` instead of blocking every PR. A push to `main` runs only the version and
   license checks, plus the platform builds when it changes the Cargo manifests, the lock or
   `goat-ci.yml`; a newer push to `main` cancels an older one, and documentation-only pushes do not
   run CI. A manual dispatch can select `slow_tests: false` for the fast build-only lane, or
   `slow_tests: true` (the default) for the full regression lane on any branch.
5. After merge and one live check per OS in GOAT, release from the version commit on `main`:
   push a branch at exactly that commit (for example `release/goat.<N>-verify`) and run
   `gh workflow run goat-ci --ref <branch> -f slow_tests=true`. Tag that commit only when this dispatch
   run is fully green on that SHA, every slow lane included; the main push run of the commit is not the
   release gate, because it skips the slow lanes. Then bump GOAT's pin. The tag run repeats the slow
   regression jobs and publishes only if they pass.
