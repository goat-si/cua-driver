#!/usr/bin/env python3
"""Check that rust/THIRD_PARTY_NOTICES.md names every MPL-2.0 component in the release binaries.

MPL-2.0 section 3.2 asks a binary distribution to tell recipients where the covered source is. The
notice's "MPL-2.0 components" table gives that pointer per component and version, so it must
change whenever the locked graph does. This compares it with the normal, non-proc-macro
dependencies of every shipped package on every target, and with the uniffi-bindgen-react-native
pin the Node runtime is built from (goat-si/cua-driver#3). Standard library only.
"""

import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
NOTICES = ROOT / "rust" / "THIRD_PARTY_NOTICES.md"
# Packages whose artifacts the release archives ship, and the features they are built with.
SHIPPED = ["cua-driver", "cursor-theme-cli", "cua-driver-sdk", "cua-driver-uia"]
FEATURES = ["cua-driver/portal-input"]
RUNTIME = "uniffi-bindgen-react-native"


def shipped_mpl_crates():
    command = ["cargo", "tree", "--locked", "--edges", "normal,no-proc-macro", "--target", "all",
               "--prefix", "none", "--format", "{p}|{l}", "--features", ",".join(FEATURES)]
    for package in SHIPPED:
        command += ["-p", package]
    output = subprocess.run(command, cwd=ROOT / "rust", check=True, capture_output=True, text=True).stdout
    crates = set()
    for line in output.splitlines():
        package, _, license_expression = line.partition("|")
        if "MPL" in license_expression:
            name, version = package.split()[:2]
            crates.add((name, version.removeprefix("v")))
    return crates


def notice_rows():
    text = NOTICES.read_text(encoding="utf-8")
    section = text.split("## MPL-2.0 components", 1)
    if len(section) != 2:
        sys.exit(f"{NOTICES.name} has no MPL-2.0 components section")
    rows = []
    for line in section[1].split("\n## ", 1)[0].splitlines():
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if line.startswith("|") and len(cells) == 4 and cells[0] not in ("Component", "---"):
            rows.append(cells)
    return rows


def main():
    rows = notice_rows()
    listed = {(name.strip(), version) for components, version, _, _ in rows
              if not version.startswith(RUNTIME) for name in components.split(",")}
    actual = shipped_mpl_crates()
    problems = [f"shipped but not in the notice: {name} {version}" for name, version in sorted(actual - listed)]
    problems += [f"in the notice but not shipped: {name} {version}" for name, version in sorted(listed - actual)]

    pin = json.loads((ROOT / "typescript" / "package.json").read_text())["devDependencies"][RUNTIME]
    runtime_rows = [version for _, version, _, _ in rows if version.startswith(RUNTIME)]
    if runtime_rows != [f"{RUNTIME} {pin}"]:
        problems.append(f"Node runtime row says {runtime_rows}, but typescript/package.json pins {RUNTIME} {pin}")
    if problems:
        sys.exit("\n".join([f"{NOTICES.relative_to(ROOT)} is out of date:"] + problems))
    print(f"{len(actual)} MPL-2.0 crates and the {RUNTIME} {pin} runtime are listed")


if __name__ == "__main__":
    main()
