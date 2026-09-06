#!/usr/bin/env python3
"""test-measure-baseline.py — self-tests for scripts/measure-baseline.py.

Proves the issue #10 AC5 baseline machinery bites on this host:
* measure writes a well-formed CSV whose rows all report exit 0 (partial
  failures are never hidden),
* check passes against the just-measured baseline,
* check FAILS against a baseline whose budget was forced below any
  achievable p95 (the regression mechanism bites), and
* check fails when a measured command exits non-zero (non-zero exit via a
  deliberately broken binary wrapper proves partial failures are surfaced).

Stdlib only. Run from the repository root:
    python3 scripts/test-measure-baseline.py
"""

from __future__ import annotations

import csv
import os
import stat
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MEASURER = os.path.join(REPO, "scripts", "measure-baseline.py")
BINARY = os.path.join(REPO, "target", "release", "herdr-fleet")


def run(argv):
    proc = subprocess.run(argv, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, check=False)
    return proc


def write_broken_binary(path: str) -> None:
    """A wrapper that only succeeds for `config init` (config generation)
    and exits 9 for every measured command — a 'binary' whose runs fail."""
    script = "#!/bin/sh\nif [ \"$1\" = \"config\" ] && [ \"$2\" = \"init\" ]; then\n  exit 0\nfi\nexit 9\n"
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(script)
    os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)


def checks():
    if not os.path.isfile(BINARY):
        raise AssertionError("release binary missing; run "
                             "`cargo build --release --locked` first")

    with tempfile.TemporaryDirectory(prefix="hf-test-baseline-") as tmp:
        baseline = os.path.join(tmp, "baseline.csv")

        proc = run([sys.executable, MEASURER, "measure", "--binary", BINARY,
                    "--out", baseline,
                    "--samples", "5", "--warmup", "1"])
        if proc.returncode != 0:
            raise AssertionError("measure failed:\n"
                                 + proc.stdout.decode() + proc.stderr.decode())
        with open(baseline, "r", newline="", encoding="utf-8") as handle:
            data_lines = [line for line in handle.read().splitlines()
                          if not line.startswith("#")]
        import csv as _csv
        import io as _io
        rows = list(_csv.DictReader(_io.StringIO("\n".join(data_lines))))
        if len(rows) != 6:
            raise AssertionError(f"expected 6 scenario rows, got {len(rows)}")
        for row in rows:
            if row["nonzero_exits"] != "no":
                raise AssertionError(
                    f"{row['scenario']}: a run exited non-zero on this host")
            if not (0 < float(row["p95_ms"]) <= float(row["budget_ms"])):
                raise AssertionError(
                    f"{row['scenario']}: budget must cover measured p95")
        print("PASS: measure wrote 6 rows; every run exited 0 and every "
              "budget covers its p95")

        proc = run([sys.executable, MEASURER, "check", "--binary", BINARY,
                    "--baseline", baseline,
                    "--samples", "5", "--warmup", "1"])
        if proc.returncode != 0:
            raise AssertionError("check against the fresh baseline failed:\n"
                                 + proc.stdout.decode() + proc.stderr.decode())
        print("PASS: check passes against the just-measured baseline")

        # The regression mechanism must bite: an impossibly small budget.
        with open(baseline, "r", newline="", encoding="utf-8") as handle:
            text = handle.read()
        text = text.replace(",25.0", ",0.0001").replace("25.0\n", "0.0001\n")
        impossible = os.path.join(tmp, "impossible.csv")
        with open(impossible, "w", newline="", encoding="utf-8") as handle:
            handle.write(text)
        proc = run([sys.executable, MEASURER, "check", "--binary", BINARY,
                    "--baseline", impossible,
                    "--samples", "5", "--warmup", "1"])
        if proc.returncode == 0:
            raise AssertionError("check must fail when p95 exceeds budget")
        if b"VIOLATION" not in proc.stderr:
            raise AssertionError("budget violation must be reported loudly")
        print("PASS: an impossible budget fails the check loudly")

        # Partial failures must never be hidden: a broken binary fails check.
        broken = os.path.join(tmp, "broken-bin")
        write_broken_binary(broken)
        proc = run([sys.executable, MEASURER, "check", "--binary", broken,
                    "--baseline", baseline,
                    "--samples", "3", "--warmup", "0"])
        if proc.returncode == 0:
            raise AssertionError("non-zero runs must fail the check")
        if b"non-zero" not in proc.stderr and b"exited non-zero" not in proc.stderr:
            raise AssertionError("the non-zero-run failure must be explicit")
        print("PASS: non-zero runs fail the check explicitly (never hidden)")

    print("test-measure-baseline.py: all checks passed")


if __name__ == "__main__":
    try:
        checks()
    except AssertionError as exc:
        sys.stderr.write(f"FAIL: {exc}\n")
        sys.exit(1)
