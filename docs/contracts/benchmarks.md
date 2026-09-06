# Benchmark corpus and method

Refs #3 (deliverable: "A benchmark corpus and the method for deriving
per-command latency/resource regression budgets without universal
network-latency promises"). Design commitment for corpus/method. The
corpus scenarios S1-S8 below need the fake-adapter benchmark harness
(child slices) and remain [awaiting-evidence]; the release-readiness slice
(issue #10) built the measurement script and produced the first measured
baseline for the pure-core command set (§ [Measured baseline](#measured-baseline)).
AC10 applies: nothing here claims a measured compatibility or performance
fact beyond what the [measured baseline](#measured-baseline) section
actually records with its evidence trail.

## Why no universal network-latency promises

herdr-fleet commands spend most of their time in external tools: Git,
GitHub, Herdr, harness executables, filesystems. None of that latency is
owned or controllable by herdr-fleet, so a wall-clock budget stated in
milliseconds would be a promise about other people's networks. Budgets are
therefore:

- **per command, per scenario, per host class**, derived from local
  measurements (below), never universal; and
- split into **internal** (daemon/plan engine CPU + state, bounded by
  design: no network in the core path) and **external** (adapters; reported,
  capped, timed out, but not budgeted as if owned).

## Benchmark corpus (synthetic, deterministic, offline-first)

| Scenario | Shape | Purpose |
| --- | --- | --- |
| S1 doctor-offline | no Herdr, no `gh`, no repos | prerequisite diagnosis cost; standalone correctness |
| S1b doctor-present | fake adapters declare capabilities | negotiation cost with zero external I/O |
| S2 status-small | 1 repository, 3 branches, fake git adapter | read-back baseline; partial/freshness timing |
| S3 status-medium | 8 repositories, mixed fake adapters, 1 failing | bounded-concurrency behavior + p50/p95 at fixed parallelism |
| S4 status-large | 64 repositories, staggered fake latency | concurrency cap enforcement; no hidden partials |
| S5 plan-issue | 1 issue, closed workflow DAG, fake forge | plan determinism + digest cost, pure-core |
| S6 plan-conflict | stale epoch + moved base | refusal latency and stale-state path |
| S7 apply-verify | journal-before-effect + idempotent replay, fake git | internal journal/claim cost; replay read-back |
| S8 harness-roundtrip | fake harness adapter (deterministic transcript) | adapter bound behavior: timeout, output cap, redaction |

Corpus properties:

- All scenarios run against **fake adapters with deterministic injected
  latencies**; no live network, no live Herdr, no private repositories.
  Results are reproducible on any host.
- Synthetic repositories use only public/random identifiers (fixture
  policy, AC8); scenario descriptions carry no downstream names.
- A second, labeled tier (live smoke) exists later only for acceptance
  checks — never for regression budgets.

## Method for deriving budgets

1. **Fixed host class + fixed binary**: record toolchain, host CPU/RAM/OS,
   adapter latencies injected, and the exact binary revision
   (source SHA + schema version + declared Herdr version).
2. **Sampling**: N ≥ 30 runs per (command, scenario) after warm-up; report
   p50 and p95 wall time plus peak RSS; keep raw samples with the run
   (generated-evidence style, content-identity pinned).
3. **Budget derivation**: internal-core budget = p95 of the pure-core
   scenario on the reference host class + headroom; external share is
   reported as observed adapter time and is never folded into an internal
   budget promise.
4. **Regression gates**: budgets live per scenario and fail only on the
   reference host class; a change that moves internal p95 beyond budget
   fails CI. Adapter latency changes show up in the reported external
   share, not as a budget failure — no flaky network-dependent gates.
5. **Publication**: measured budgets and the runs that produced them are
   recorded in the release notes of the slice that first measures them;
   this document stays the method, and measured values live beside the
   evidence (AC10 distinction).

## Measured baseline

Measured by `scripts/measure-baseline.py` (issue #10 AC5) on 2026-09-06;
committed table: [baseline-linux-x86_64.csv](baseline-linux-x86_64.csv).

Host class caveats (public-data rule: no host identity is recorded):

- Host class token: `linux-x86_64` (the machine the measurement ran on —
  an unloaded developer host, NOT a shared CI runner; do not compare
  numbers across hosts or host classes).
- Toolchain recorded in the CSV header (cargo/rustc pinned by
  `rust-toolchain.toml`, Python version).
- Only **pure-core, offline commands** are measured (help, version,
  capabilities, config validate/show, service plan rendering — no git/gh/
  Herdr/daemon/network on the path). Corpus scenarios S1-S8 (doctor/
  status/plan with fake adapters at injected latencies) still need the
  fake-adapter benchmark harness and remain [awaiting-evidence].
- Per-run exit codes are recorded and any non-zero run fails the
  measurement loudly: partial failures are never hidden, and external
  adapter/network latency is never folded into an internal budget.

Method applied here (mirrors the corpus method):

- N = 30 runs per scenario after 2 warm-ups, p50/p95 wall time (ms) and
  peak RSS (KB, per-child `ru_maxrss` via `os.wait4`).
- Budget per row: `max(p95 x 2.0, 25.0 ms)` — p95 of the measured
  pure-core command + headroom; budgets live per scenario on the reference
  host class only.
- The check (`measure-baseline.py check --baseline ...`) re-measures on the
  same host class and fails loudly if any p95 exceeds its committed budget
  or any run exits non-zero. It is **not** a CI gate (wall-clock budgets on
  shared runners would flake); it is an opt-in regression mechanism the
  maintainer runs on the release host class and documents in the release
  issue.

| Scenario (row id) | p50 ms | p95 ms | peak RSS KB | Budget ms | Notes |
| --- | --- | --- | --- | --- | --- |
| help | 0.64 | 0.77 | 19116 | 25.0 | pure CLI startup envelope |
| version | 0.68 | 0.73 | 19260 | 25.0 | pure CLI startup envelope + schema facts |
| capabilities | 0.69 | 0.78 | 19260 | 25.0 | declared read caps, no external I/O |
| config-validate | 0.76 | 0.85 | 19260 | 25.0 | synthetic generated config |
| config-show | 0.76 | 0.87 | 19260 | 25.0 | synthetic generated config |
| service-install-plan | 0.65 | 0.92 | 19260 | 25.0 | plan rendering, no service activation |

The CSV is the machine-readable source of these rows (raw samples are
regenerated by re-running the measure command; they are not committed).
