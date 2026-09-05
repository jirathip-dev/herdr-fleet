# herdr-fleet

Typed, plan-first companion CLI for operating Herdr coding-agent fleets.

> **READ-ONLY CORE (issue #4).** This binary configures, diagnoses, observes,
> and renders deterministic read-only plans. It never starts/stops Herdr and
> never mutates repositories or fleet state. Live fleet mutations are **not
> implemented**. See [Status and boundaries](#status-and-boundaries) and the
> [Roadmap](#roadmap).

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
CURRENT READ-ONLY CORE — this repository today (issue #4)
    herdr-fleet config init|validate|show   typed hf-config/v1 configuration
    herdr-fleet doctor                      prerequisite diagnosis (read-only)
    herdr-fleet status                      bounded local/git/gh observations
    herdr-fleet plan                        deterministic hf-plan/v1 rendering
    herdr-fleet capabilities                declared read capabilities
    (no daemon, no workflow execution, no live mutations; all versioned JSON
     output follows the #3 contract corpus under docs/contracts/)

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

- Current slice — **read-only core** (issue
  [#4](https://github.com/jirathip-dev/herdr-fleet/issues/4)): configuration
  (`config init|validate|show`), prerequisite diagnosis (`doctor`), bounded
  read-only observation (`status`), deterministic plan rendering (`plan`),
  and a capability declaration (`capabilities`). Every `--json` invocation
  writes exactly one versioned `hf-output/v1` envelope per the
  [#3 contract corpus](docs/contracts/README.md); plans bind a redacted
  acceptance revision and a canonical digest, and nothing in the binary
  installs/starts/stops Herdr, mutates a repository, or stores credentials.
- Public core vs private overlays: everything here is public and reusable;
  deployment policy, credentials, model/provider choices, host paths, and
  private routing stay **downstream** and out of this repository (see
  [ADR-0001](docs/decisions/0001-public-core-private-overlays.md)).
- Tested platforms: **macOS** and **Linux** (GitHub-hosted runners, both
  families, every PR).
- Branch model: `main` is stable/release; `staging` is the permanent
  integration branch. See [WORKFLOW.md](docs/WORKFLOW.md) and
  [ADR-0002](docs/decisions/0002-staging-integration-main-release.md).
- Safety principles: plan-first mutations (future), no shell-evaluated remote
  data, adapter reads are env-isolated and redacted at the boundary,
  least-privilege CI, fail-closed public-data gates, no self-update, no
  plugin host until a real need is proven, and same-user TTY approval as an
  *accidental-safety* boundary — not cryptographic isolation (see
  [ADR-0003](docs/decisions/0003-companion-boundary-and-harness-neutrality.md)).

## Quickstart

Prerequisites: Rust 1.97.1 (see [DEVELOPMENT.md](docs/DEVELOPMENT.md) for
exact install commands), `just` (optional but recommended). The read-only
commands need only the local tools they observe: `doctor`/`status`/`plan`
probe `herdr --version`, authenticated `gh`, and local git — never install
any of them.

```console
$ git clone https://github.com/jirathip-dev/herdr-fleet.git
$ cd herdr-fleet
$ cargo build --locked --release
$ ./target/release/herdr-fleet --help
herdr-fleet — typed, plan-first companion CLI for Herdr coding-agent fleets

READ-ONLY CORE: this binary observes configuration, prerequisites, and
repository state and renders deterministic plans. It never installs, starts,
stops, or upgrades Herdr; it never mutates repositories or fleet state; and
it never stores credentials (GitHub reads use the invoking environment's
authenticated `gh`).
...
```

Configure one repository (writes nothing; the template prints to stdout):

```console
$ ./target/release/herdr-fleet config init > ~/.config/herdr-fleet/config.toml
$ ./target/release/herdr-fleet config validate && ./target/release/herdr-fleet config show
```

Diagnose and observe:

```console
$ ./target/release/herdr-fleet doctor
$ cd /path/to/your/repo && /path/to/herdr-fleet status
$ herdr-fleet plan example-org/widgets 123
```

Commands: `config init|validate|show [--config PATH] [--json]`,
`doctor [--json]`, `status [--config PATH] [--json]`,
`plan <repository> <issue> [--revision HEX40] [--config PATH] [--json]`,
`capabilities [--json]`. Exit codes: 0 ok · 1 operational error · 2 usage ·
3 partial · 4 refusal · 5 config error. `--json` mode writes exactly one
`hf-output/v1` document to stdout (diagnostics on stderr, never prompts).

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

- [docs/contracts/README.md](docs/contracts/README.md) — the versioned
  contract corpus (schema registry, CLI/JSON contract, config/plan/capability
  specs) that machine and human outputs of this slice conform to.
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
the approved target architecture and delivery graph. The #3 contract corpus
(`docs/contracts/`) and the #4 read-only core (this repository's current
state) are implemented; the remaining children are **unrouted** and will each
require their own route grant.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT), at your option.
