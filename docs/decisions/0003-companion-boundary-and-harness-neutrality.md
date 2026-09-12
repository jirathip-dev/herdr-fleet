# ADR-0003: Companion boundary and harness neutrality

- Status: accepted
- Date: 2026-09-05
- Related: umbrella issue #1 architecture decision (authoritative);
  ADR-0001, ADR-0002

## Context

canter sits in an ecosystem of overlapping tools (Herdr, Corral, agent
harnesses, scheduling platforms). Earlier wordings blurred boundaries:
daemon-authority overlaps, review-loop confusion, live-cutover wording, and
"canter replaces X" claims. This ADR records the locked boundary so
bootstrap documentation and future slices cannot drift into coupling.

## Decision

### Companion, not replacement; no runtime dependencies

- **Herdr** is the execution/workspace substrate: it owns workspaces, panes,
  terminals, and agent-process hosting.
- **canter** is a standalone, headless orchestration/control CLI (and,
  in the approved target, a local daemon) for software-repository fleets. It
  owns typed plans, workflow state, mediated effects, verification,
  recovery, and machine-readable outcomes.
- **Corral** is an optional, **read-only** human observability client. There
  is **no runtime dependency in either direction** between Corral and
  canter: no Corral library, daemon, HTTP route, process,
  configuration, or availability may be required by the CLI.
- canter does not replace Hermes Agent, the Hermes scheduler as a
  platform, Corral's daemon, Herdr's server, or unrelated research/office/
  host-maintenance automation. Portable fleet scheduling is distinct from a
  private cron census; the exact private census/disposition is a private
  concern, and any legacy-scheduler shutdown happens only through a
  human-approved atomic cutover tracked separately.

### Harness neutrality

- Harness, model/provider, role, skills, workflow, policy, repository, and
  execution substrate are **independent axes**; configuration composes them
  and none is inferred from another.
- The domain core never branches on product names. Hermes, Claude Code,
  Codex, and OpenCode appear only as **adapter examples** (and in adapter
  documentation), never as core assumptions or a promise that all adapters
  ship.
- Adapters sit at a small typed capability boundary (discovery, start,
  prompt delivery, observation, interruption/cancellation, terminal outcome
  collection, identity/read-back). Unsupported capabilities return typed
  refusals; subprocess adapters use argv arrays with bounded time/output,
  cancellation, redaction, and explicit exit outcomes. Tests exercise the
  plan engine with fake adapters before any real harness contract tests.

### Framework-neutral doctrine and downstream policy

- Framework-neutral Fleet Doctrine's eventual canonical home is this
  repository (judgment guidance under a public doctrine area; machine-
  enforceable rules become CLI invariants and discriminating tests). The
  concise public skill links to canonical chapters rather than duplicating
  them.
- Private deployment policy stays downstream (ADR-0001).

### Trust boundary honesty

- Same-user TTY approval is an **accidental-safety** boundary (it prevents
  accidental/cooperative misuse), **not cryptographic isolation** against a
  malicious peer process. Hardened isolation requires a separate OS
  principal/container plus external branch controls. Public docs say so.

## Consequences

- Diagrams must show Corral's actual current path separately (Herdr per-user
  Unix socket → corrald → iOS client) and any future canter read
  contract edge as dashed + labeled "optional future adapter".
- Bootstrap ships no adapter/plugin hierarchy, no harness implementation,
  and no empty speculative directories.
- The public skill must be usable without any specific harness profile
  layout; a specific harness may be named only as a clearly labeled adapter
  example.

## Links

- [ARCHITECTURE.md](../ARCHITECTURE.md) · [WORKFLOW.md](../WORKFLOW.md)
- Committed target diagram: [architecture artifacts](../architecture/README.md)
