# GOAT's fork of Cua Driver

This repository is [GOAT](https://github.com/goat-si/goat)'s fork of Cua Driver, the computer-use driver
from [trycua/cua](https://github.com/trycua/cua) (`libs/cua-driver`, MIT, Copyright (c) 2025 Cua AI, Inc.).
GOAT builds the driver from here and ships it inside its own signed helper app instead of downloading
upstream release binaries. Tracking issue: goat-si/goat#1595.

## Branches and versions

- `upstream` holds pure `git subtree split --prefix=libs/cua-driver` output of trycua/cua, with upstream
  history. Nothing else is committed there. `upstream-v0.37.0` tags the split of
  `cua-driver-rs-v0.37.0` (trycua/cua `2d2f99228d`).
- `main` is `upstream` plus GOAT's changes. Changes land through pull requests.
- Releases are tagged `cua-driver-rs-v<upstream version>-goat.<N>`, for example `cua-driver-rs-v0.37.0-goat.1`.

## What differs from upstream

- `vendor/` carries the monorepo files the driver needs to build on its own; see vendor/README.md.
- `LICENSE.md` is the monorepo's root license, which the subtree split leaves out.
- GOAT fixes, each linked to an issue here or in goat-si/goat. Every fix that is not GOAT-specific is also
  sent upstream as a separate pull request against trycua/cua `main`, and driver bugs are reported upstream.

## Signing

Release artifacts here are unsigned. GOAT signs the driver when it builds its helper app, so the helper
keeps its existing signing identity and the permissions people granted it. This repository holds no
signing certificates or secrets.

## Syncing upstream

Upstream moves several times a day; we sync deliberately, not continuously.

1. In a trycua/cua clone: `git subtree split --prefix=libs/cua-driver <tag or commit>`, then fast-forward
   `upstream` here to the result.
2. Review the `upstream` diff for security and behavior changes.
3. Merge `upstream` into a branch from `main`, refresh `vendor/`, and run `cargo deny check`.
4. Open a pull request; CI runs the protocol-compatibility test and builds every platform.
5. After merge and one live check per OS in GOAT, tag a release and bump GOAT's pin.
