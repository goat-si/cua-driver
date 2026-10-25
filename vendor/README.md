# Vendored monorepo files

`rust/crates/cua-driver` depends on `cua-telemetry`, which upstream keeps outside `libs/cua-driver` in the
trycua/cua monorepo. The subtree split drops it, so this directory carries it, laid out as in the
monorepo so its relative `include_str!` paths still resolve:

| Here | Upstream path |
| --- | --- |
| `cua/crates/cua-telemetry/` | `libs/cua/crates/cua-telemetry/` |
| `cua/crates/cua-teleport/src/ux/targets.json` | `libs/cua/crates/cua-teleport/src/ux/targets.json` |
| `images/sandbox-images.json` | `libs/images/sandbox-images.json` |

Source commit: trycua/cua `2d2f99228d4d2508dc6fcc84c176b296e146714a` (tag `cua-driver-rs-v0.37.0`). Files are
byte-identical to that commit except `cua-telemetry/Cargo.toml`, whose `*.workspace = true` fields are
written out with the values from `libs/cua/Cargo.toml` at the same commit.

The driver links this crate with `default-features = false`, which leaves out its HTTP sender: events are
built and logged locally and never leave the machine. GOAT also sets the driver's telemetry switches off.

Refresh on every upstream sync (see GOAT.md):

```sh
REV=<upstream commit>
git -C <trycua/cua clone> archive "$REV" libs/cua/crates/cua-telemetry | tar -x --strip-components=1 -C vendor
git -C <trycua/cua clone> show "$REV:libs/cua/crates/cua-teleport/src/ux/targets.json" > vendor/cua/crates/cua-teleport/src/ux/targets.json
git -C <trycua/cua clone> show "$REV:libs/images/sandbox-images.json" > vendor/images/sandbox-images.json
# then re-apply the Cargo.toml workspace values and check `git diff`
```
