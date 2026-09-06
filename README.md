# herdr-fleet

Typed, plan-first companion CLI for operating Herdr coding-agent fleets.

> **Current surface (issues #3-#10).** This repository ships the full
> plan-first surface on `staging`: a read-only CLI core (`config`, `doctor`,
> `status`, `plan`, `capabilities`), a single-writer local state daemon
> (`daemon run|status`), per-user service-unit plan rendering (`service
> doctor|install-plan|status-plan|uninstall-plan`), and a daemon-mediated,
> grant-gated mutation path (plan `apply` over the local socket RPC — one
> digest-bound plan step per request). The CLI never performs fleet
> mutations directly, the daemon never starts/stops Herdr, and there is no
> live workflow execution or release execution yet. See
> [Status and boundaries](#status-and-boundaries) and the
> [operations runbook](docs/OPERATIONS.md).

herdr-fleet is an **independent community companion** compatible with Herdr —
it is not an official or endorsed Herdr project. It is a Herdr-first,
harness-neutral, software-repository fleet CLI operated as a local daemon for
one operator/trusted host on macOS and Linux.

**Corral companion, not Corral dependency.** Corral is a separate optional
read-only observability product. Neither herdr-fleet nor Corral requires the
other at runtime.

## Problem statement

Herdr hosts workspaces, panes, terminals, and agent processes. Operating a
fleet of coding agents on top of Herdr today means hand-driving plan/review/
merge discipline per repository. herdr-fleet owns the **portable, typed,
plan-first orchestration layer** — typed plans, workflow state, mediated
effects, verification, and machine-readable outcomes — while Herdr keeps
hosting execution and Corral keeps being an optional visual window into what
is happening. The roadmap slices #3-#10 build that layer in stages: contracts
first, then a read-only core, a state daemon, a workflow engine, harness
adapters, control-plane mutations, lifecycle safety, and release readiness
(see [Roadmap](#roadmap)).

## Ecosystem

```text
CURRENT SHIPPED SURFACE — this repository today (issues #3-#10, staging)
    herdr-fleet config init|validate|show    typed hf-config/v1 configuration
    herdr-fleet doctor                       prerequisite diagnosis (read-only)
    herdr-fleet status                       bounded local/git/gh observations
    herdr-fleet plan                         deterministic hf-plan/v1 rendering
    herdr-fleet capabilities                 declared forge read capabilities
    herdr-fleet daemon run|status            single-writer state daemon + probe
    herdr-fleet service doctor|install-plan|status-plan|uninstall-plan
                                             per-user launchd/systemd unit
                                             plan rendering (never activates)
    (mutations: daemon plan apply over the local socket RPC — plan-first,
     grant-gated, one digest-bound step per request; versioned JSON output
     follows the #3 contract corpus under docs/contracts/)

LOCKED TARGET (issue #1, docs/architecture/) — delivered in stages
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
assumption of the engine.

## Command surface

Mirrors `herdr-fleet --help` on the release binary exactly (one line per
shipped command):

| Command | Behavior (from `--help`) |
| --- | --- |
| `config init` | Print an annotated `hf-config/v1` template to stdout (writes nothing). |
| `config validate [--config PATH] [--json]` | Validate the config file and its named policy overlay. |
| `config show [--config PATH] [--json]` | Inspect the effective configuration. |
| `doctor [--json]` | Diagnose prerequisites; never installs/starts/stops. |
| `status [--config PATH] [--json]` | Observe configured repositories (read-only, bounded). |
| `plan <repository> <issue> [--revision HEX40] [--config PATH] [--json]` | Render a deterministic read-only `hf-plan/v1` plan. |
| `capabilities [--json]` | Report the CLI's declared forge read capabilities. |
| `daemon run [--socket PATH] [--config PATH]` | Run or probe the single-writer state daemon (foreground; flock + SQLite state + per-user Unix socket). |
| `daemon status [--config PATH] [--json]` | Probe the daemon socket and report live state. |
| `service doctor [--config PATH] [--json]` | Check the daemon environment read-only (platform, config, socket state, unit placement). |
| `service install-plan [--config PATH] [--json]` | Render the per-user launchd/systemd unit text + install steps (never installs). |
| `service status-plan [--config PATH] [--json]` | Render the status/verification steps for the unit (never queries the service manager). |
| `service uninstall-plan [--config PATH] [--json]` | Render the uninstall steps for the unit (never uninstalls). |

Exit codes (with or without `--json`): 0 ok · 1 operational error · 2 usage ·
3 partial · 4 refusal · 5 config error. In `--json` mode every command that
accepts it writes exactly one `hf-output/v1` document to stdout; diagnostics
go to stderr; JSON output never prompts.

## Status and boundaries

- **Read-only CLI core** (issues
  [#3](https://github.com/jirathip-dev/herdr-fleet/issues/3) and
  [#4](https://github.com/jirathip-dev/herdr-fleet/issues/4)): configuration
  (`config init|validate|show`), prerequisite diagnosis (`doctor`), bounded
  read-only observation (`status`), deterministic plan rendering (`plan`),
  and a capability declaration (`capabilities`). Every `--json` invocation
  writes exactly one versioned `hf-output/v1` envelope per the
  [#3 contract corpus](docs/contracts/README.md); plans bind a redacted
  acceptance revision and a canonical sha256 digest; nothing here
  installs/starts/stops Herdr, mutates a repository, or stores credentials.
- **Daemon foundation** (issue
  [#5](https://github.com/jirathip-dev/herdr-fleet/issues/5)): `daemon run`
  serves the single-writer state daemon in the foreground (per-user flock,
  migrated SQLite state, audit/event journals, backup/restore hooks) and
  `daemon status` probes its socket. Read-only commands stay available
  without the daemon.
- **Service plans** (issue #5 AC9): `service doctor` and the
  `service *-plan` commands render, never activate — no install/start/stop/
  query of the host service manager from the CLI.
- **Mutation surface — plan-first, daemon-mediated, grant-gated** (issue
  [#8](https://github.com/jirathip-dev/herdr-fleet/issues/8)): the only
  mutation path is the daemon `apply` RPC over the local socket — one typed,
  digest-bound, capability-gated plan step per request, executed against an
  authorized route grant + instance and journaled before the effect. The CLI
  has no direct mutation command; plans are never applied by `plan`.
  Durable review evidence, recorded first-write approvals, and
  worktree-confined harness execution live in
  [spec-plans.md](docs/contracts/spec-plans.md) and
  [spec-daemon.md](docs/contracts/spec-daemon.md). No production overclaim:
  this is a single-operator local daemon, not a distributed control plane.
- **Lifecycle and release readiness are contracts, not live execution**
  (issues [#9](https://github.com/jirathip-dev/herdr-fleet/issues/9),
  [#10](https://github.com/jirathip-dev/herdr-fleet/issues/10)): recurring
  schedules, admission, cleanup archive/salvage, remote transport, cold-boot
  recovery, archives/SBOM/provenance, and baselines are specified and
  implemented on `staging`; there is **no live workflow execution, no release
  execution, and no real harness session from public CI or fork PRs** (real
  harness parity and release execution are human-gated — issue #7 AC6,
  [RELEASING.md](docs/RELEASING.md)).
- Public core vs private overlays: everything here is public and reusable;
  deployment policy, credentials, model/provider choices, host paths, and
  private routing stay **downstream** and out of this repository (see
  [ADR-0001](docs/decisions/0001-public-core-private-overlays.md)).
- Tested platforms: **macOS** and **Linux** (GitHub-hosted runners, both
  families, every PR).
- Branch model: `main` is stable/release; `staging` is the permanent
  integration branch. See [WORKFLOW.md](docs/WORKFLOW.md) and
  [ADR-0002](docs/decisions/0002-staging-integration-main-release.md).
- Safety principles: plan-first, digest-bound, journaled-before-effect
  mutations with live grant/instance/epoch revalidation; no shell-evaluated
  remote data; adapter reads are env-isolated and redacted at the boundary;
  worktree-confined harness execution; no self-update; no plugin host until a
  real need is proven; same-user TTY approval as an *accidental-safety*
  boundary — not cryptographic isolation (see
  [ADR-0003](docs/decisions/0003-companion-boundary-and-harness-neutrality.md)).

## Quickstart

Prerequisites: Rust 1.97.1 (see [DEVELOPMENT.md](docs/DEVELOPMENT.md) for
exact install commands), `just` (optional but recommended). Read-only
commands need only the local tools they observe: `doctor`/`status`/`plan`
probe `herdr --version`, authenticated `gh`, and local git — they never
install any of them.

```console
$ git clone https://github.com/jirathip-dev/herdr-fleet.git
$ cd herdr-fleet
$ cargo build --locked --release
$ ./target/release/herdr-fleet --version
herdr-fleet 0.1.0
...
state schema version: 4
migration chain: m0001_initial_state_v1, m0002_workflow_engine_instances_v2, m0003_control_plane_evidence_v3, m0004_schedules_lifecycle_v4
...
```

Configure (the template prints to stdout; the CLI never writes files):

```console
$ ./target/release/herdr-fleet config init > ~/.config/herdr-fleet/config.toml
$ ./target/release/herdr-fleet config validate
config valid: .../.config/herdr-fleet/config.toml
```

**Observe** — diagnose prerequisites and observe a configured repository
from inside its local checkout:

```console
$ ./target/release/herdr-fleet doctor --json
{"command":"doctor","data":{"checks":[{"detail":"git version 2.52.0","name":"git","status":"ok"}, ...],
 "config":{...,"status":"ok"}},"exit_code":0,"kind":"ok","schema":"hf-output/v1"}
$ cd /path/to/widgets && /path/to/herdr-fleet status --json
{"command":"status","data":{"expected_repositories":1,"freshness":"fresh",
 "observations":[...],"observed_repositories":1,...},
 "exit_code":0,"kind":"ok","schema":"hf-output/v1"}
```

**Plan** — render a deterministic, read-only plan (never applied by the CLI):

```console
$ ./target/release/herdr-fleet plan example-org/widgets 7 \
    --revision 0123456789abcdef0123456789abcdef01234567 --json
{"command":"plan","data":{"digest":"8164...e87b","issue_source":"argument",
 "plan":{"issue":{"number":7,"revision":"0123...4567"},...,
 "repository":"example-org/widgets","schema":"hf-plan/v1","state_epoch":0,
 "steps":[{"id":"p1","kind":"checkout",...},...],
 "workflow_hash":"a906...c01f","workflow_id":"fleet-doctrine-1"}},
 "exit_code":0,"kind":"ok","schema":"hf-output/v1"}
```

**Daemon** — run the single-writer state daemon in one terminal, then probe
it from another:

```console
$ ./target/release/herdr-fleet daemon run        # foreground; Ctrl-C stops the process
                                                # (socket stays stale until a fresh `daemon run` reclaims it — see [docs/OPERATIONS.md](docs/OPERATIONS.md) §5)
$ ./target/release/herdr-fleet daemon status --json
{"command":"daemon status","data":{"daemon":{"pid":<pid>,...},
 "state":{"active_grants":0,"epoch":1,"schema_version":4,...}},
 "exit_code":0,"kind":"ok","schema":"hf-output/v1"}
```

**Service** — render the per-user systemd/launchd unit plan (never activates
it):

```console
$ ./target/release/herdr-fleet service install-plan --json
{"command":"service install-plan","data":{"platform":"systemd",
 "steps":["write the rendered unit to ...", ...], "unit":"# herdr-fleet ..."},
 "exit_code":0,"kind":"ok","schema":"hf-output/v1"}
```

Outputs above are trimmed and host paths are redacted; the task-ordered
runbook with full expected shapes lives in
[docs/OPERATIONS.md](docs/OPERATIONS.md).

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

- [docs/OPERATIONS.md](docs/OPERATIONS.md) — human operations runbook:
  install → configure → observe → plan → daemon/service lifecycle →
  grant-gated mutations → cleanup → recovery → upgrade, with exercised
  commands and failure→remedy tables.
- [docs/contracts/README.md](docs/contracts/README.md) — the versioned
  contract corpus (schema registry, spec-cli/config/plans/capabilities,
  spec-daemon/state/workflow, capability-map, compatibility, benchmarks) that
  machine and human outputs conform to.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — current scaffold vs planned
  modules, boundaries, dependency direction, target flow.
- [docs/contracts/spec-lifecycle.md](docs/contracts/spec-lifecycle.md) —
  lifecycle semantics: schedules, admission, cleanup archive, remote
  transport, cold-boot recovery (issue #9).
- [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) — prerequisites, canonical gate
  list, toolchain process, troubleshooting.
- [docs/WORKFLOW.md](docs/WORKFLOW.md) — contributor + maintainer workflow.
- [docs/RELEASING.md](docs/RELEASING.md) — release readiness: archive
  builder + verification machinery, version/schema policy, clean-host
  verification rows (execution human-gated).
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
the approved target architecture and delivery graph. Roadmap slices #3-#10
and polish #12 are implemented on `staging` (PRs #13-#21):
[#3](https://github.com/jirathip-dev/herdr-fleet/issues/3) contracts,
[#4](https://github.com/jirathip-dev/herdr-fleet/issues/4) read-only CLI
core, [#5](https://github.com/jirathip-dev/herdr-fleet/issues/5) daemon
foundation, [#6](https://github.com/jirathip-dev/herdr-fleet/issues/6)
workflow engine + bundled Doctrine default,
[#7](https://github.com/jirathip-dev/herdr-fleet/issues/7) harness adapters
(Hermes/Claude Code/Codex + declarative generic argv, verified with
fake-executable contract tests on public CI),
[#8](https://github.com/jirathip-dev/herdr-fleet/issues/8) control-plane
mutations (daemon-mediated plan `apply`, durable review evidence + recorded
first-write approvals, granular step kinds, grant/instance invalidation on
material edits and epoch rotation, worktree-confined harness execution),
[#9](https://github.com/jirathip-dev/herdr-fleet/issues/9) lifecycle safety
(durable recurring non-destructive schedules with single-flight/coalesced
evaluation, cold-boot recovery, fan-out admission with host-resource proofs +
monorepo-overlap refusal, cleanup archive/salvage with byte-verified
manifests, a verified system-SSH remote transport contract, and
retention-bounded backup pruning), and
[#10](https://github.com/jirathip-dev/herdr-fleet/issues/10) release
readiness (deterministic platform archives with checksums + offline SBOM +
provenance, clean-host verification scripts, compatibility probe mapping,
measured baselines, and the active in-repo release/version policy).

Remaining boundaries, unchanged: `staging` is the integration branch and
`main` changes only via dedicated human promotion; **no live workflow
execution or release execution exists yet**; no harness session runs from
public CI or fork PRs (real harness parity is a human-gated clean-host smoke,
issue #7 AC6); the mutation path is grant-gated and daemon-mediated by
design, and release execution is human-gated (docs/RELEASING.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT), at your option.
