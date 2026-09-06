---
name: herdr-fleet
description: "Use when working with or on the herdr-fleet repository or CLI. Read-only CLI core, daemon-mediated mutations, lifecycle (schedules/admission/recovery), and release-readiness machinery (archives/provenance/baselines); no live harness runs."
version: 1.6.0
author: herdr-fleet contributors
license: Apache-2.0 OR MIT
platforms: [macos, linux]
metadata:
  hermes:
    tags: [herdr-fleet, cli, fleet, repository, read-only-core]
---

# herdr-fleet

herdr-fleet is a public companion CLI for operating Herdr coding-agent
fleets. Roadmap slices #3–#10 are merged: a read-only CLI core (#4), a
daemon foundation with SQLite state and a local socket RPC (#5), the
deterministic workflow engine with the bundled Doctrine default workflow
(#6), the capability-negotiated harness adapters (#7: Hermes, Claude
Code, Codex + a declarative generic argv adapter, verified with
fake-executable contract tests that need no credentials), the
control-plane mutation layer (#8: the daemon mediates plan `apply` — one
typed, digest-bound, capability-gated plan step per request — with durable
review-evidence rows, recorded first-real-write approvals, and
worktree-confined harness execution), the lifecycle layer (#9:
durable recurring non-destructive `hf-schedule/v1` schedules with
single-flight/coalesced evaluation and cold-boot recovery, fan-out
admission with host-resource proofs and monorepo-overlap refusal, cleanup
archive/salvage with byte-verified manifests, a verified system-SSH
remote transport contract, and retention-bounded backup pruning), and the
release-readiness layer (#10: deterministic platform archives with
SHA-256 checksums, an offline Cargo.lock-derived SPDX SBOM and a
release-provenance/v1 record; clean-host verification scripts; a measured
baseline + regression-budget machinery; and the active in-repo
release/version policy). The daemon never starts/stops Herdr, never
mutates external repositories, and never stores credentials. No live
workflow execution or release execution exists yet, and no real harness
session runs from public CI (release execution and the clean-host matrix
are human-gated).

## When to use

- You are contributing to the herdr-fleet repository (Rust, docs, contracts,
  CI).
- You need to know what the CLI does today without guessing.

## Safe discovery (read-only, no side effects)

Start from the committed documentation — never invent commands:

1. Index and status: `README.md` (current slice, boundaries, quickstart).
2. Contracts: `docs/contracts/README.md` (schema registry, spec-cli/config/
   plans/capabilities, capability-map, compatibility — the #3 corpus this
   slice implements).
3. Canonical gates: `docs/DEVELOPMENT.md` (the `just` recipes; `just ci`
   is the full local aggregate mirroring CI).
4. Process: `docs/WORKFLOW.md`; architecture: `docs/ARCHITECTURE.md` and
   the ADRs under `docs/decisions/`.
5. Repository contract for agents: `AGENTS.md` (public-data boundary,
   branch discipline, smallest-change rule).
6. To inspect the compiled binary (after `cargo build --locked`):
   `herdr-fleet --help`, then per command
   `doctor|status|plan|capabilities|config init|validate|show`.

## Current command surface (issue #4 read-only core)

| Command | Behavior |
| --- | --- |
| `config init` | Prints an annotated `hf-config/v1` TOML template to stdout (writes nothing). |
| `config validate [--config PATH] [--json]` | Validates the config document and its named policy overlay. |
| `config show [--config PATH] [--json]` | Renders the effective configuration (repositories, identities, pins). |
| `doctor [--json]` | Checks prerequisites (git/herdr/gh) + config; never installs or starts anything. |
| `status [--config PATH] [--json]` | Bounded read-only observation of configured repositories via local git + authenticated `gh`; degradation is explicit, never hidden. |
| `plan <repository> <issue> [--revision HEX40] [--config PATH] [--json]` | Renders a deterministic read-only `hf-plan/v1` with a canonical digest and a redacted acceptance revision. |
| `capabilities [--json]` | Reports declared forge read capabilities. |

Exit codes: 0 ok · 1 operational error · 2 usage · 3 partial · 4 refusal ·
5 config error. In `--json` mode stdout carries exactly one `hf-output/v1`
envelope; diagnostics go to stderr. Adapter reads (git/gh/herdr) run through
a bounded env-isolated runner and one redaction pass at the boundary.

## Bundled Doctrine default workflow (issue #6, link not duplicate)

The Doctrine default workflow (fake/no-effect step kinds only) has one
canonical source in this repository; this skill links to it and never
duplicates it:

- Canonical source (versioned + hash-pinned):
  `schemas/fixtures/workflow/workflow.doctrine.json` (`workflow_id`
  `fleet-doctrine-1`, `hf-workflow/v1`).
- Normative spec and engine rules: `docs/contracts/spec-workflow.md`
  ("Engine semantics" and "Bundled Doctrine default", issue #6).
- Rust embedding + pinned digest: `src/engine.rs`
  (`engine::doctrine_default`, `engine::DOCTRINE_DIGEST`).
- The public skill uses this workflow's model-agnostic default roles
  (orchestrator/implementer/reviewer); workflow content can never carry
  policy, capabilities, target, or approval authority.

## Harness adapters (issue #7, link not duplicate)

The harness adapter contract has one canonical home in this repository;
this skill links to it and never duplicates it:

- Normative contract: `docs/contracts/spec-capabilities.md` ("Adapter
  contract", issue #7) — closed operation set, typed refusal codes,
  identity-triple rule, fake-adapter doctrine.
- Declared version ranges and measured version facts:
  `docs/contracts/compatibility.md` (Hermes Agent / Claude Code / Codex
  rows, 2026-09-06).
- Rust implementation: `src/adapters.rs` (`herdr_fleet::adapters`); contract
  tests with fake executables: `tests/harness_adapters.rs` — public CI and
  fork PRs need no harness credentials. Real harness parity is a
  human-gated clean-host smoke documented in `.report-7.md`, never run from
  this repository's CI (AC6/AC7).

## Control-plane mutations (issue #8, link not duplicate)

The daemon-mediated mutation layer has one canonical home; this skill links
to it and never duplicates it:

- Normative contract: `docs/contracts/spec-plans.md` ("Apply semantics",
  issue #8) and `docs/contracts/spec-review-evidence.md` (durable evidence
  rows, m0003/schema v3).
- Rust implementation: `src/mutation.rs` (`herdr_fleet::mutation` engine +
  effect registry), `src/daemon.rs` (`plan`/`apply` RPC handlers), durable
  rows + invalidation semantics in `src/state.rs`.
- Acceptance tests: `tests/mutation_engine.rs` (socket-level flows over
  disposable local repositories) — public CI needs no credentials.

## Release readiness (issue #10, link not duplicate)

The release-readiness machinery has one canonical home; this skill links to
it and never duplicates it:

- Normative contract + version/schema policy + documented command rows:
  `docs/RELEASING.md` (in-repo machinery active; release EXECUTION
  human-gated: no promotion/tag/upload/attestation/soak from CI or lanes).
- Deterministic archive builder + verifier (`scripts/build-archive.py`,
  `scripts/test-build-archive.py`): platform archives with SHA-256
  checksums, offline SPDX SBOM from `Cargo.lock`, and the
  `release-provenance/v1` record binding source ref + binary-reported
  schema facts (`herdr-fleet --version`).
- Clean-host verification (`scripts/clean-host-verify.sh`,
  `scripts/clean-host-probe.py`, `scripts/test-clean-host-probe.py`): the
  AC2 command rows humans run on fresh macOS/Linux hosts; fixture-level
  self-tests run the same checks in disposable temp dirs.
- Baseline machinery + committed table (`scripts/measure-baseline.py`,
  `docs/contracts/baseline-linux-x86_64.csv`,
  `docs/contracts/benchmarks.md`): pure-core latency/RSS budgets, opt-in
  same-host-class checks, never a CI gate.
- Compatibility probe mapping: `docs/contracts/compatibility.md`
  ("Capability-probe mapping", issue #10 AC4).

## Current limitation (CLI read-only; daemon-mediated effects only)

The CLI performs **no fleet mutations**: no spawn, rearm, review, plugin,
or release effects exist outside the daemon's `plan`/`apply` RPC, and apply
executes one capability-gated plan step per request against an authorized
grant + instance — never unplanned work. The daemon never mutates external
repositories in this repository's tests (disposable local repositories and
fakes only). Do not present the locked-target architecture (under
`docs/architecture/`) as current behavior — it is the approved *target*
model from the roadmap umbrella.

## Safety direction (plan/apply gates)

Mutations are typed, digest-bound, journaled before effect, exactly read
back, and revalidated against live grant/instance/epoch state immediately
before the effect; production/destructive actions require fresh interactive
TTY confirmation (and recorded first-write approval for real-external
scopes) and can never be scheduled. Any proposal to add mutation behavior
outside this machinery is out-of-scope for the read-only core and routes
through the roadmap.

## Adapter-example note

Hermes (or any agent harness) may appear in this repository only as a
clearly labeled *adapter example* of the future harness-neutral design —
never as a required invocation path, profile layout, or core dependency.
This skill is usable with any harness or with none.

## Repository rules that bind work here

- Public-data rule: never commit host paths, private repository names,
  credentials, provider/model policy, or live scheduler identity (CI
  scans every PR).
- Branches: PRs target `staging`; `main` changes only via human promotion
  (or the documented hotfix exception).
- License: Apache-2.0 OR MIT; DCO not adopted.
