#!/usr/bin/env python3
"""test-clean-host-probe.py — self-tests for scripts/clean-host-probe.py and
scripts/clean-host-verify.sh.

Runs the release binary's daemon surface on THIS host in disposable temp
dirs exactly the way the human clean-host gate will run it on maintainer
hosts (no service-manager activation, no network, no private repositories)
and proves the guards bite: a missing binary and a broken binary fail
loudly, and the real binary passes every check.

Stdlib only. Run from the repository root:
    python3 scripts/test-clean-host-probe.py
Build the release binary first (`cargo build --release --locked`).
"""

from __future__ import annotations

import os
import stat
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PROBE = os.path.join(REPO, "scripts", "clean-host-probe.py")
VERIFIER = os.path.join(REPO, "scripts", "clean-host-verify.sh")
BINARY = os.path.join(REPO, "target", "release", "canter")
SCHEDULE_DOC = os.path.join(REPO, "schemas", "fixtures", "schedule",
                            "schedule.valid.json")


def run(argv):
    return subprocess.run(argv, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, check=False)


def write_broken_binary(path: str) -> None:
    script = "#!/bin/sh\nexit 9\n"
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(script)
    os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)


def checks():
    if not os.path.isfile(BINARY) or not os.access(BINARY, os.X_OK):
        raise AssertionError("release binary missing; run "
                             "`cargo build --release --locked` first")

    # Guard: missing binary fails loudly with exit 2.
    proc = run([sys.executable, PROBE, "--bin",
                os.path.join(REPO, "no-such-binary"),
                "--schedule-doc", SCHEDULE_DOC])
    if proc.returncode != 2:
        raise AssertionError("probe must exit 2 for a missing binary")
    print("PASS: probe rejects a missing binary (exit 2)")

    # Guard: broken binary fails loudly with exit 1 (never a false pass).
    broken = os.path.join(REPO, "target", "hf-broken-bin")
    try:
        write_broken_binary(broken)
        proc = run([sys.executable, PROBE, "--bin", broken,
                    "--schedule-doc", SCHEDULE_DOC])
        if proc.returncode == 0:
            raise AssertionError("probe must fail for a broken binary")
        print("PASS: probe fails loudly for a broken binary")

        # The verify.sh wrapper surfaces the same guard failures.
        proc = run(["bash", VERIFIER, "--bin", broken])
        if proc.returncode == 0:
            raise AssertionError("verify.sh must fail for a broken binary")
        print("PASS: verify.sh fails loudly for a broken binary")

        proc = run(["bash", VERIFIER, "--bin", BINARY, "--repo", REPO])
        if proc.returncode != 0:
            raise AssertionError(
                "verify.sh failed against the real binary:\n"
                + proc.stdout.decode() + proc.stderr.decode())
        text = proc.stdout.decode()
        for expected in ("PASS: --help", "PASS: --version",
                         "PASS: config init", "PASS: config validate",
                         "PASS: service install-plan",
                         "PASS: service status-plan",
                         "PASS: service uninstall-plan",
                         "PASS: daemon round trip",
                         "clean-host verify: all checks passed"):
            if expected not in text:
                raise AssertionError(f"verify.sh output missing '{expected}':\n"
                                     + text)
        print("PASS: verify.sh passes all checks against the real binary "
              "(help/version, config, service plans, daemon round trip)")
    finally:
        if os.path.exists(broken):
            os.unlink(broken)

    print("test-clean-host-probe.py: all checks passed")


if __name__ == "__main__":
    try:
        checks()
    except AssertionError as exc:
        sys.stderr.write(f"FAIL: {exc}\n")
        sys.exit(1)
