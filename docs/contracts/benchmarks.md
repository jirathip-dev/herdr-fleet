# Benchmark corpus and method

Refs #3 (deliverable: "A benchmark corpus and the method for deriving
per-command latency/resource regression budgets without universal
network-latency promises"). Design commitment for corpus/method; every
number in this document is [awaiting-evidence] until a benchmark harness
exists (child slices). AC10 applies: nothing here claims a measured
compatibility or performance fact.

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
