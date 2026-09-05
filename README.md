# herdr-fleet

Typed, plan-first companion CLI for operating Herdr coding-agent fleets.

> **PRE-ALPHA — repository foundation only.** Live fleet mutations are **not
> implemented**. The binary truthfully reports `--help` and `--version` and
> nothing else. See [Roadmap](#roadmap).

herdr-fleet is an **independent community companion** compatible with Herdr —
it is not an official or endorsed Herdr project. It is a Herdr-first,
harness-neutral, software-repository fleet CLI planned as a local daemon for
one operator/trusted host on macOS and Linux.

**Corral companion, not Corral dependency.** Corral is a separate optional
read-only observability product. Neither herdr-fleet nor Corral requires the
other at runtime.

## Problem statement

Herdr hosts workspaces, panes, terminals, and agent processes. Operating a
fleet of coding agents on top of Herdr today means hand-driving plan/review/
merge discipline per repository. herdr-fleet's approved target is to own the
**portable, typed, plan-first orchestration layer** — typed plans, workflow
state, mediated effects, verification, and machine-readable outcomes — while
Herdr keeps hosting execution and Corral keeps being an optional visual
window into what is happening. In this bootstrap the project deliberately
implements none of that yet; it establishes the public foundation first
(CI, security gates, docs, architecture artifacts).

## Ecosystem

```text
CURRENT BOOTSTRAP — this repository today
    herdr-fleet --help / --version          (no daemon, no workflow, no mutations)

APPROVED TARGET (locked in issue #1 — NOT implemented; see docs/architecture/)
    Operator + Route Grant -> CLI -> local daemon (sole transition/state
      authority) -> typed workflow + plan engine -> typed effects/adapters
        -> Herdr (workspace/terminal/process) · Git/GitHub · agent harnesses
    XDG config (canonical) + optional private downstream policy overlay
    SQLite state + audit (leases · journal · recovery · epoch)

OPTIONAL FUTURE ADAPTER (not implemented, not required)
    - - - versioned read-only state/events - - - > optional read-only clients

CORRAL'S SEPARATE CURRENT PATH (independent product, unchanged by us)
    Herdr per-user Unix socket -> corrald -> FleetNotifier (iOS)
```

There is **no** herdr-fleet → Corral dependency in any current or required
path, and no harness, model, provider, role, or policy name is a core
assumption of the planned engine.

## Status and boundaries

- Public core vs private overlays: everything here is public and reusable;
  deployment policy, credentials, model/provider choices, host paths, and
  private routing stay **downstream** and out of this repository (see
  [ADR-0001](docs/decisions/0001-public-core-private-overlays.md)).
- Tested platforms: **macOS** and **Linux** (GitHub-hosted runners, both
  families, every PR).
- Branch model: `main` is stable/release; `staging` is the permanent
  integration branch. See [WORKFLOW.md](docs/WORKFLOW.md) and
  [ADR-0002](docs/decisions/0002-staging-integration-main-release.md).
- Safety principles: plan-first mutations, no shell-evaluated remote data,
  least-privilege CI, fail-closed public-data gates, no self-update, no
  plugin host until a real need is proven, and same-user TTY approval as an
  *accidental-safety* boundary — not cryptographic isolation (see
  [ADR-0003](docs/decisions/0003-companion-boundary-and-harness-neutrality.md)).

## Quickstart

Prerequisites: Rust 1.97.1 (see [DEVELOPMENT.md](docs/DEVELOPMENT.md) for
exact install commands), `just` (optional but recommended).

```console
$ git clone https://github.com/jirathip-dev/herdr-fleet.git
$ cd herdr-fleet
$ cargo build --locked --release
$ ./target/release/herdr-fleet --help
herdr-fleet — typed, plan-first companion CLI for Herdr coding-agent fleets
...
$ ./target/release/herdr-fleet --version
herdr-fleet 0.1.0
```

That is all the binary does in this bootstrap — by design.

## Development, CI, and branches

- Canonical developer gates: `just ci` (fmt-check → check → lint → test →
  doc → build-release → security). Full details live in
  [DEVELOPMENT.md](docs/DEVELOPMENT.md).
- Hosted CI runs on every PR to `staging`/`main` and every push to either
  long-lived branch: `policy`, `rust-ubuntu`, `rust-macos`, `supply-chain`,
  `secret-scan`.
- Feature branches cut from `staging`; PRs target `staging`; promotion to
  `main` is a dedicated human-only PR (with a narrow, human-approved
  `hotfix/*` exception). See [WORKFLOW.md](docs/WORKFLOW.md).

## Documentation index

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — current scaffold vs planned
  modules, boundaries, dependency direction, target flow.
- [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) — prerequisites, canonical gate
  list, toolchain process, troubleshooting.
- [docs/WORKFLOW.md](docs/WORKFLOW.md) — contributor + maintainer workflow.
- [docs/RELEASING.md](docs/RELEASING.md) — future release contract (not yet
  active).
- Architecture decisions:
  - [ADR-0001: public core vs private overlays](docs/decisions/0001-public-core-private-overlays.md)
  - [ADR-0002: staging integration, main release](docs/decisions/0002-staging-integration-main-release.md)
  - [ADR-0003: companion boundary and harness neutrality](docs/decisions/0003-companion-boundary-and-harness-neutrality.md)
- Locked target architecture artifacts (committed, with static previews):
  - [docs/architecture/README.md](docs/architecture/README.md)
  - [JSON source](docs/architecture/herdr-fleet.locked-target.architecture.json)
  - [Interactive HTML](docs/architecture/herdr-fleet.locked-target.architecture.html)
  - [Preview (light)](docs/architecture/herdr-fleet.locked-target.architecture.preview.light.png)
  - [Preview (dark)](docs/architecture/herdr-fleet.locked-target.architecture.preview.dark.png)
- [CONTRIBUTING.md](CONTRIBUTING.md) — how to contribute.
- [SECURITY.md](SECURITY.md) — supported versions and private vulnerability
  reporting.
- [AGENTS.md](AGENTS.md) — agent-facing repository contract.

## Roadmap

The umbrella issue
[herdr-fleet#1](https://github.com/jirathip-dev/herdr-fleet/issues/1) tracks
the approved target architecture and delivery graph; children #3–#10 are
**unrouted** and will each require their own route grant. This repository's
current state is only the bootstrap foundation (issue
[#2](https://github.com/jirathip-dev/herdr-fleet/issues/2)).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT), at your option.
