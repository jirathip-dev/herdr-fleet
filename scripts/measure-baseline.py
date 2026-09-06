#!/usr/bin/env python3
"""measure-baseline.py — deterministic per-command latency/resource baselines.

Issue #10 (AC5): measures internal (no-network, no-external-tool) CLI
commands of the release binary, records p50/p95 wall time + per-run peak
RSS + every raw exit code, and can CHECK a re-measurement against a
committed baseline's regression budgets.

Method mirrors docs/contracts/benchmarks.md: budgets are per command, per
host class, derived from local p95 + headroom; external/network latency is
never folded into an internal budget; any non-zero run exit fails loudly
(partial failures are never hidden). NOT wired into CI: wall-clock budgets
on shared runners would flake — the check is opt-in on a reference host
class (run it on the same host class the baseline was measured on).

Stdlib only. Public-data rule: output names only the platform token (e.g.
linux-x86_64) and toolchain versions — never host paths, distro/user
identity, or private names.

Usage:
  measure-baseline.py measure --binary PATH --out FILE [--samples 30]
  measure-baseline.py check --binary PATH --baseline FILE [--samples 30]

Exit codes: 0 ok, 1 baseline violated or operational error, 2 usage.
"""

from __future__ import annotations

import argparse
import csv
import io
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time

# Internal scenarios: argv shapes that never touch the network, git, gh,
# Herdr, or a daemon socket. The config-consuming scenarios use a synthetic
# config generated once per run by the binary itself (`config init`) — no
# fixture coupling, works on any host.
SCENARIOS = [
    ("help", ["--help"], False),
    ("version", ["--version"], False),
    ("capabilities", ["capabilities", "--json"], False),
    ("config-validate", ["config", "validate", "--config", "{config}"], True),
    ("config-show", ["config", "show", "--config", "{config}"], True),
    ("service-install-plan", ["service", "install-plan", "--config",
                              "{config}"], True),
]

DEFAULT_SAMPLES = 30
DEFAULT_WARMUP = 2
BUDGET_HEADROOM_MULTIPLIER = 2.0
BUDGET_FLOOR_MS = 25.0


def platform_token() -> str:
    machine = platform.machine().lower()
    if machine in ("x86_64", "amd64"):
        machine = "x86_64"
    elif machine in ("aarch64", "arm64"):
        machine = "aarch64"
    system = platform.system().lower()
    if system == "darwin":
        system = "darwin"
    elif system == "linux":
        system = "linux"
    return f"{system}-{machine}"


def toolchain_versions() -> dict:
    versions = {"python": platform.python_version()}
    for tool, flag in (("cargo", "--version"), ("rustc", "--version")):
        try:
            proc = subprocess.run([tool, flag], stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, check=False,
                                  timeout=10)
            if proc.returncode == 0:
                versions[tool] = proc.stdout.decode("utf-8",
                                                    "replace").strip()
        except (OSError, subprocess.TimeoutExpired):
            pass
    return versions


def measure_once(binary: str, argv: list[str]) -> tuple[float, int, int]:
    """Run one sample; return (wall_seconds, exit_code, peak_rss_kb).

    peak_rss_kb uses os.wait4 rusage per child (POSIX; macOS reports bytes
    and Linux reports KB — both are normalized to KB here).
    """
    started = time.perf_counter()
    proc = subprocess.Popen([binary] + argv, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)
    _pid, status, usage = os.wait4(proc.pid, 0)
    elapsed = time.perf_counter() - started
    exit_code = os.waitstatus_to_exitcode(status)
    rss = usage.ru_maxrss
    if sys.platform == "darwin":
        rss_kb = rss // 1024
    else:
        rss_kb = rss
    return elapsed, exit_code, int(rss_kb)


def percentile(sorted_values: list[float], percentile: float) -> float:
    if not sorted_values:
        return 0.0
    index = (len(sorted_values) - 1) * percentile
    lower = int(index)
    upper = min(lower + 1, len(sorted_values) - 1)
    frac = index - lower
    return sorted_values[lower] * (1 - frac) + sorted_values[upper] * frac


def generate_synthetic_config(binary: str) -> str:
    """Generate one synthetic hf-config/v1 TOML via `config init`."""
    proc = subprocess.run([binary, "config", "init"], stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, check=False)
    if proc.returncode != 0:
        sys.stderr.write(
            f"error: `{binary} config init` failed (exit {proc.returncode}): "
            + proc.stderr.decode("utf-8", "replace")
        )
        sys.exit(1)
    directory = tempfile.mkdtemp(prefix="hf-baseline-config-")
    path = os.path.join(directory, "config.toml")
    with open(path, "wb") as handle:
        handle.write(proc.stdout)
    return path


def measure(binary: str, samples: int, warmup: int) -> list[dict]:
    rows = []
    config = generate_synthetic_config(binary)
    for scenario_id, template, needs_config in SCENARIOS:
        argv = [part.format(config=config) if needs_config else part
                for part in template]
        # Warm-up before sampling (benchmarks.md method).
        for _ in range(warmup):
            measure_once(binary, argv)
        wall_times: list[float] = []
        rss_kb_values: list[int] = []
        exit_codes: list[int] = []
        for _ in range(samples):
            elapsed, exit_code, rss_kb = measure_once(binary, argv)
            wall_times.append(elapsed)
            exit_codes.append(exit_code)
            rss_kb_values.append(rss_kb)
        sorted_times = sorted(wall_times)
        rows.append({
            "scenario": scenario_id,
            "samples": samples,
            "warmup": warmup,
            "p50_ms": round(percentile(sorted_times, 0.50) * 1000.0, 2),
            "p95_ms": round(percentile(sorted_times, 0.95) * 1000.0, 2),
            "mean_ms": round(statistics.mean(wall_times) * 1000.0, 2),
            "min_ms": round(min(wall_times) * 1000.0, 2),
            "max_ms": round(max(wall_times) * 1000.0, 2),
            "peak_rss_kb": max(rss_kb_values),
            "nonzero_exits": exit_codes.count(0) != samples,
        })
    return rows


def budget_for(p95_ms: float) -> float:
    return max(BUDGET_FLOOR_MS, round(p95_ms * BUDGET_HEADROOM_MULTIPLIER, 2))


def command_measure(args: argparse.Namespace) -> int:
    if not os.path.isfile(args.binary) or not os.access(args.binary, os.X_OK):
        sys.stderr.write(f"error: --binary {args.binary} is not executable "
                         "(build with `cargo build --release --locked`)\n")
        return 2
    rows = measure(args.binary, args.samples, args.warmup)
    header = ["scenario", "samples", "warmup", "p50_ms", "p95_ms", "mean_ms",
              "min_ms", "max_ms", "peak_rss_kb", "nonzero_exits", "budget_ms"]
    with open(args.out, "w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle)
        writer.writerow(["# platform", platform_token()])
        toolchain = toolchain_versions()
        writer.writerow(["# toolchain", json.dumps(toolchain, sort_keys=True)])
        writer.writerow(["# budgets", f"budget = max(p95 x "
                         f"{BUDGET_HEADROOM_MULTIPLIER}, "
                         f"{BUDGET_FLOOR_MS}ms); same-host-class check only"])
        writer.writerow(header)
        for row in rows:
            writer.writerow([
                row["scenario"], row["samples"], row["warmup"],
                row["p50_ms"], row["p95_ms"], row["mean_ms"],
                row["min_ms"], row["max_ms"], row["peak_rss_kb"],
                "yes" if row["nonzero_exits"] else "no",
                budget_for(row["p95_ms"]),
            ])
    failures = [row for row in rows if row["nonzero_exits"]]
    print(f"measured {len(rows)} scenarios on {platform_token()}:")
    for row in rows:
        status = "FAIL(nonzero exit)" if row["nonzero_exits"] else "ok"
        print(f"  {row['scenario']:<22} p50={row['p50_ms']:>8.2f}ms "
              f"p95={row['p95_ms']:>8.2f}ms rss={row['peak_rss_kb']:>7}KB "
              f"{status}")
    if failures:
        print("baseline measure: one or more runs exited non-zero; "
              "partial failures are never hidden", file=sys.stderr)
        return 1
    print(f"baseline CSV written to {args.out}")
    return 0


def read_baseline(path: str) -> dict[str, dict]:
    """Parse a baseline CSV, ignoring '#' metadata rows (the first row of
    the remaining text is the real header)."""
    with open(path, "r", newline="", encoding="utf-8") as handle:
        data_lines = [line for line in handle.read().splitlines()
                      if not line.startswith("#")]
    text = "\n".join(data_lines) + "\n"
    baseline_rows: dict[str, dict] = {}
    for row in csv.DictReader(io.StringIO(text)):
        scenario = row.get("scenario")
        if scenario is None or not scenario:
            continue
        baseline_rows[scenario] = row
    return baseline_rows


def command_check(args: argparse.Namespace) -> int:
    if not os.path.isfile(args.baseline):
        sys.stderr.write(f"error: baseline file not found: {args.baseline}\n")
        return 2
    baseline_rows = read_baseline(args.baseline)
    rows = measure(args.binary, args.samples, args.warmup)
    violations = []
    for row in rows:
        scenario = row["scenario"]
        baseline = baseline_rows.get(scenario)
        if baseline is None:
            violations.append(
                f"{scenario}: not present in the committed baseline")
            continue
        budget = float(baseline["budget_ms"])
        if row["nonzero_exits"]:
            violations.append(
                f"{scenario}: a run exited non-zero (partial failures are "
                "never hidden)")
        if row["p95_ms"] > budget:
            violations.append(
                f"{scenario}: p95 {row['p95_ms']}ms exceeds the committed "
                f"budget {budget}ms on {platform_token()}")
    if violations:
        for violation in violations:
            print(f"VIOLATION: {violation}", file=sys.stderr)
        return 1
    print(f"baseline check passed on {platform_token()}: all {len(rows)} "
          "scenarios within their committed budgets, all runs exited 0")
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="measure-baseline.py",
        description=(
            "Deterministic per-command latency/resource baselines for the "
            "release binary (issue #10 AC5); see docs/contracts/benchmarks.md."
        ),
    )
    sub = parser.add_subparsers(dest="command", required=True)

    measure_p = sub.add_parser("measure", help="measure and write a baseline CSV")
    measure_p.add_argument("--binary", required=True,
                           help="path to the release binary")
    measure_p.add_argument("--out", required=True,
                           help="output CSV path")
    measure_p.add_argument("--samples", type=int, default=DEFAULT_SAMPLES)
    measure_p.add_argument("--warmup", type=int, default=DEFAULT_WARMUP)

    check_p = sub.add_parser("check", help="check against a baseline CSV")
    check_p.add_argument("--binary", required=True)
    check_p.add_argument("--baseline", required=True)
    check_p.add_argument("--samples", type=int, default=DEFAULT_SAMPLES)
    check_p.add_argument("--warmup", type=int, default=DEFAULT_WARMUP)

    args = parser.parse_args(argv)
    if args.samples < 3 or args.warmup < 0:
        parser.error("--samples must be >= 3")
    return args


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.command == "measure":
        return command_measure(args)
    if args.command == "check":
        return command_check(args)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
