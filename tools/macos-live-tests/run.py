#!/usr/bin/env python3
"""Run every platform-macos lib test ignored with a reason starting "live:"
alone, one process per test (goat-si/cua-driver#188).

The list comes from the compiled test binary itself: running the ignored tests
by exact name without --ignored makes libtest report each one as
"test <name> ... ignored, <reason>" without executing it. That covers every way
the attribute can be written (raw strings, string continuation, macros) and
every module compiled into the lib, including #[path] modules outside src/.

The script fails unless libtest reports every listed ignored test exactly once
with its reason, at least one of them is live, and each live test's own process
exits 0 having run exactly one test that passed.

Usage: run.py <platform-macos lib test binary>
"""
import os
import re
import subprocess
import sys

REPORTED = re.compile(r"^test (\S+) \.\.\. ignored(?:, (.*))?$")


def listed_ignored(binary):
    out = subprocess.run(
        [binary, "--list", "--ignored"], check=True, stdout=subprocess.PIPE, text=True
    ).stdout
    return [line[: -len(": test")] for line in out.splitlines() if line.endswith(": test")]


def ignore_reasons(binary, ignored, failures):
    """Map each ignored test to the reason libtest reports for it."""
    report = subprocess.run(
        [binary, "--exact", "--test-threads=1", *ignored],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    reasons = {}
    for line in report.stdout.splitlines():
        match = REPORTED.match(line)
        if not match:
            continue
        name, reason = match.group(1), match.group(2) or ""
        if name in reasons:
            failures.append(f"libtest reported {name} twice")
        reasons[name] = reason
    summary = re.search(
        r"test result: ok\. 0 passed; 0 failed; (\d+) ignored", report.stdout
    )
    if report.returncode != 0 or not summary or int(summary.group(1)) != len(ignored):
        failures.append(
            f"reading ignore reasons: exit {report.returncode}, expected {len(ignored)} "
            f"ignored and none run\n{report.stdout[-3000:]}"
        )
    missing = sorted(set(ignored) - set(reasons))
    if missing:
        failures.append(f"libtest gave no ignore report for: {missing}")
    return reasons


def main():
    binary = sys.argv[1]
    failures = []
    ignored = listed_ignored(binary)
    if not ignored:
        # With no names, the reason read would run the whole suite instead.
        print("the binary lists no ignored tests: wrong binary?")
        sys.exit(1)
    reasons = ignore_reasons(binary, ignored, failures)
    tests = [name for name in ignored if reasons.get(name, "").startswith("live:")]
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
    print(f"live tests: {len(ignored)} ignored, {len(tests)} live, {passed} passed alone")
    if not tests:
        failures.append('no test is ignored with a "live:" reason')
    if failures:
        print("\n".join(failures))
        sys.exit(1)


if __name__ == "__main__":
    main()
