#!/usr/bin/env python3
"""Fail-closed checks with the live-call probe (goat-si/cua-driver#188).

  check.py --probe live.dylib control <control binary>
      Fails unless the probe is active and every read-only family below logs.
  check.py --probe live.dylib default <platform-macos lib test binary>
      Runs the default (non-ignored) lib tests single-threaded under the probe.
      Fails unless the probe is active, the binary exits 0, libtest reports ok
      with 0 failed and passed + ignored equal to the listed test count, and no
      live call was logged at all.

What the probe sees: the intercepted C entry points and ObjC class methods in
live.c, and every dlopen/dlsym made by the binary itself (which covers the
lazily resolved SkyLight function pointers). Linked APIs or ObjC selectors
outside that list are not observed.
"""
import argparse
import hashlib
import os
import re
import subprocess
import sys

CONTROL_FAMILIES = [
    "AXUIElementCopyAttributeValue",
    "AXIsProcessTrusted",
    "CGWindowListCopyWindowInfo",
    "CGSessionCopyCurrentDictionary",
    "CGEventSourceSecondsSinceLastEventType",
    "CGEventCreate(NULL)",
    "CGMainDisplayID",
    "CGGetActiveDisplayList",
    "CGPreflightScreenCaptureAccess",
    "dlopen",
    "dlsym",
    "+[NSWorkspace sharedWorkspace]",
    "+[NSRunningApplication runningApplicationWithProcessIdentifier:]",
    "+[NSCursor currentSystemCursor]",
    "+[NSScreen screens]",
    "+[NSPasteboard generalPasteboard]",
]


def run(argv, probe):
    env = dict(os.environ, DYLD_INSERT_LIBRARIES=probe)
    env.pop("CUA_TEST_ALLOW_CLIPBOARD", None)
    return subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env)


def calls(output):
    return [m.strip() for m in re.findall(r"LIVECALL (.+)", output) if not m.startswith("PROBE_READY")]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--probe", required=True)
    parser.add_argument("mode", choices=["control", "default"])
    parser.add_argument("binary")
    args = parser.parse_args()
    digest = hashlib.sha256(open(args.binary, "rb").read()).hexdigest()
    print(f"{args.mode}: {args.binary} sha256 {digest}")
    failures = []
    if args.mode == "control":
        result = run([args.binary], args.probe)
        if "LIVECALL PROBE_READY" not in result.stdout:
            failures.append("probe not loaded (no PROBE_READY)")
        if result.returncode != 0:
            failures.append(f"control binary exited {result.returncode}")
        seen = calls(result.stdout)
        for family in CONTROL_FAMILIES:
            if not any(call.startswith(family) for call in seen):
                failures.append(f"probe did not observe {family}")
        print(f"control observed {len(seen)} calls")
    else:
        listed = subprocess.run(
            [args.binary, "--list"], check=True, stdout=subprocess.PIPE, text=True
        ).stdout
        total = sum(1 for line in listed.splitlines() if line.endswith(": test"))
        result = run([args.binary, "--test-threads=1"], args.probe)
        if "LIVECALL PROBE_READY" not in result.stdout:
            failures.append("probe not loaded (no PROBE_READY)")
        if result.returncode != 0:
            failures.append(f"test binary exited {result.returncode}")
        summary = re.search(
            r"test result: ok\. (\d+) passed; 0 failed; (\d+) ignored", result.stdout
        )
        if not summary:
            failures.append("no successful libtest result")
        elif int(summary.group(1)) + int(summary.group(2)) != total:
            failures.append(
                f"executed {summary.group(1)} passed + {summary.group(2)} ignored, binary lists {total}"
            )
        else:
            print(f"{summary.group(1)} passed, {summary.group(2)} ignored of {total}")
        current = "(before the first test)"
        for line in result.stdout.splitlines():
            for match in re.finditer(r"test (\S+) \.\.\. |LIVECALL (.+?)(?=LIVECALL|test \S+ \.\.\. |$)", line):
                if match.group(1):
                    current = match.group(1)
                elif not match.group(2).startswith("PROBE_READY"):
                    failures.append(f"live call during {current}: {match.group(2).strip()}")
    if failures:
        print("FAILED:\n" + "\n".join(failures))
        if args.mode == "default":
            # What libtest said, without the probe's lines: the failing tests
            # and their panics, or the tail when the binary died early.
            text = re.sub(r"LIVECALL [^\n]*", "", result.stdout)
            start = text.find("\nfailures:\n")
            print("--- libtest output ---")
            print(text[start:] if start >= 0 else text[-6000:])
        sys.exit(1)
    print("ok")


if __name__ == "__main__":
    main()
