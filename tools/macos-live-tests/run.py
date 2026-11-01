#!/usr/bin/env python3
"""Run every platform-macos lib test marked #[ignore = "live: ..."] alone, one
process per test (goat-si/cua-driver#188).

The list comes from the source markers, so it cannot drift from the code. The
script fails unless every marker names exactly one test the binary lists as
ignored, every listed live test is marked, and each process exits 0 having run
exactly one test that passed.

Usage: run.py <platform-macos lib test binary>
"""
import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
SRC = ROOT / "rust" / "crates" / "platform-macos" / "src"
MARKER = re.compile(
    r'#\[ignore\s*=\s*"live:[^"]*"\]\s*\n(?:\s*#\[[^\n]*\]\s*\n)*\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w+)\s*\('
)
# Every string literal that starts with "live:", whatever attribute spelling
# surrounds it: plain or raw strings (r"..", r#".."#), spaces after # or
# inside the brackets or quotes, cfg_attr. Each one must be a canonical
# marker that MARKER ties to its fn; anything else (a comment or an unusual
# layout before the fn, a non-canonical spelling) fails the run instead of
# silently dropping that test from the lane.
ANY_LIVE = re.compile(r'"\s*live:')


def marked_fns(failures):
    names = []
    for path in sorted(SRC.rglob("*.rs")):
        source = path.read_text()
        found = MARKER.findall(source)
        attributes = len(ANY_LIVE.findall(source))
        if attributes != len(found):
            failures.append(
                f"{path.relative_to(ROOT)}: {attributes} \"live:\" literals but only {len(found)} "
                'canonical markers tied to a fn; write each as #[ignore = "live: ..."] '
                "with the fn (or other attributes) directly after it"
            )
        names += found
    return names


def listed_ignored(binary):
    out = subprocess.run(
        [binary, "--list", "--ignored"], check=True, stdout=subprocess.PIPE, text=True
    ).stdout
    return [line[: -len(": test")] for line in out.splitlines() if line.endswith(": test")]


def main():
    binary = sys.argv[1]
    failures = []
    fns = marked_fns(failures)
    ignored = listed_ignored(binary)
    tests = []
    for fn in fns:
        matches = [t for t in ignored if t.split("::")[-1] == fn]
        if len(matches) != 1:
            failures.append(f"marker on fn {fn} matches {len(matches)} ignored tests: {matches}")
        else:
            tests.append(matches[0])
    if len(set(tests)) != len(tests):
        failures.append("two markers resolve to the same test")
    env = dict(os.environ, CUA_TEST_ALLOW_CLIPBOARD="1")
    passed = 0
    for test in tests:
        run = subprocess.run(
            [binary, test, "--exact", "--ignored", "--test-threads=1"],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env=env,
        )
        ok = run.returncode == 0 and re.search(
            r"test result: ok\. 1 passed; 0 failed; 0 ignored", run.stdout
        )
        print(("PASS " if ok else "FAIL ") + test, flush=True)
        if ok:
            passed += 1
        else:
            failures.append(f"{test}: exit {run.returncode}\n{run.stdout[-3000:]}")
    print(f"live tests: {len(fns)} markers, {len(tests)} resolved, {passed} passed alone")
    if not tests:
        failures.append("no live tests found: the marker pattern or the source path is wrong")
    if failures:
        print("\n".join(failures))
        sys.exit(1)


if __name__ == "__main__":
    main()
