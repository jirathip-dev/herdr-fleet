---
name: herdr-fleet
description: "Use when working with or on the herdr-fleet repository or CLI. Read-only core plus shipped adapter-contract library; no live harness runs or mutations."
version: 1.3.0
author: herdr-fleet contributors
license: Apache-2.0 OR MIT
platforms: [macos, linux]
metadata:
  hermes:
    tags: [herdr-fleet, cli, fleet, repository, read-only-core]
---

# herdr-fleet

herdr-fleet is a public companion CLI for operating Herdr coding-agent
fleets. Roadmap slices #3–#7 are merged: a read-only CLI core (#4), a
daemon foundation with SQLite state and a local socket RPC (#5), the
deterministic workflow engine with the bundled Doctrine default workflow
(#6), and the capability-negotiated harness adapters (#7: Hermes, Claude
Code, Codex + a declarative generic argv adapter, verified with
fake-executable contract tests that need no credentials). The daemon never
starts/stops Herdr, never mutates external repositories, and never stores
credentials. No live workflow execution, migration, or release behavior
exists yet, and no real harness session runs from public CI (children #8–#10
unrouted).

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

## Current limitation (read-only core)

The CLI performs **no fleet mutations**: no spawn, rearm, apply, review,
plugin, or release effects exist. Do not present the locked-target
architecture (under `docs/architecture/`) as current behavior — it is the
approved *target* model from the roadmap umbrella.

## Safety direction (future plan/apply)

The approved target is plan-first: mutations are typed, digest-bound,
journaled before effect, and exactly read back; production/destructive
actions require fresh interactive TTY confirmation and can never be
scheduled. Until that machinery exists, treat any proposal to add mutation
behavior as out-of-scope for the current read-only core and route it through
the roadmap.

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
