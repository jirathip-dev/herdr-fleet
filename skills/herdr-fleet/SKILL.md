---
name: herdr-fleet
description: "Use when working with or on the herdr-fleet repository or CLI. Read-only discovery; pre-alpha, no mutations."
version: 1.0.0
author: herdr-fleet contributors
license: Apache-2.0 OR MIT
platforms: [macos, linux]
metadata:
  hermes:
    tags: [herdr-fleet, cli, fleet, repository, pre-alpha]
---

# herdr-fleet

herdr-fleet is a public, pre-alpha companion CLI for operating Herdr
coding-agent fleets. **This repository is bootstrap-only**: the binary
truthfully exposes `--help` and `--version` and nothing else. No daemon,
workflow, mutation, migration, or release behavior is implemented.

## When to use

- You are contributing to the herdr-fleet repository (docs, CI, scaffold).
- You need to know what the CLI does today without guessing.

## Safe discovery (read-only, no side effects)

Start from the committed documentation — never invent commands:

1. `docs/README` index: `README.md` (status, boundaries, quickstart).
2. Canonical gates: `docs/DEVELOPMENT.md` (the `just` recipes; `just ci`
   is the full local aggregate mirroring CI).
3. Process: `docs/WORKFLOW.md`; architecture: `docs/ARCHITECTURE.md` and
   the ADRs under `docs/decisions/`.
4. Repository contract for agents: `AGENTS.md` (public-data boundary,
   branch discipline, smallest-change rule).
5. To inspect the compiled binary (after a build):
   `herdr-fleet --help` and `herdr-fleet --version`. That is the entire
   current command surface.

## Current limitation (pre-alpha)

The CLI performs **no fleet operations**: no status, spawn, rearm, review,
plugin, or release commands exist. Do not present the locked-target
architecture (under `docs/architecture/`) as current behavior — it is the
approved *target* model from the roadmap umbrella.

## Safety direction (future plan/apply)

The approved target is plan-first: mutations are typed, digest-bound,
journaled before effect, and exactly read back; production/destructive
actions require fresh interactive TTY confirmation and can never be
scheduled. Until that machinery exists, treat any proposal to add mutation
behavior as out-of-scope for this bootstrap and route it through the
roadmap.

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
