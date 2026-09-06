# Architecture

Current bootstrap vs approved target, boundaries, and dependency direction.
The locked target is documented in the committed architecture artifacts in
[`architecture/`](architecture/README.md) and in
[ADR-0001](decisions/0001-public-core-private-overlays.md),
[ADR-0002](decisions/0002-staging-integration-main-release.md), and
[ADR-0003](decisions/0003-companion-boundary-and-harness-neutrality.md).

## Current scaffold (this bootstrap)

One Cargo package (library + `herdr-fleet` binary), stdlib only, zero
external dependencies:

- `src/lib.rs` — public identity surface (`PACKAGE_NAME`, `PACKAGE_VERSION`,
  `about()`); `#![forbid(unsafe_code)]`.
- `src/main.rs` — stdlib argument handling: `--help`/`-h`, `--version`/`-V`,
  usage-on-error otherwise. No command claims unimplemented functionality.
- `tests/cli_smoke.rs` — exercises the real binary (exit codes asserted).
- `scripts/` — public-tree privacy scanner + self-tests.
- `.github/workflows/` — policy, rust (ubuntu + macOS), supply-chain, and
  secret-scan gates.
- `docs/architecture/` — the approved locked-target artifacts (target, not
  current state).

There is **no** daemon, workflow engine, adapter, mutation, migration,
release, or plugin machinery — deliberately (YAGNI; see below).

## Planned module split (approved target — not implemented)

The locked target keeps a harness-neutral core with adapters at the edge:

- **Plan/domain core** — typed workflow + plan engine, grants/digests,
  leases, idempotency, recovery. No product-name branching in domain types.
- **Adapters (edge, typed, small)** — Herdr (workspace/terminal/process
  execution), Git/GitHub (branches/PRs/checks/read-back), and **agent-harness
  adapters**. Hermes, Claude Code, Codex, and OpenCode may appear **only as
  adapter examples** in this repository; they are never core assumptions and
  none is promised to ship in any particular slice. Unsupported capabilities
  return typed refusals.
- **Separate axes**: harness, model/provider, role, skills, workflow, policy,
  repository, and execution substrate compose via configuration; none is
  inferred from another, and no model/provider names are compiled into core.

Dependency direction is strictly inward: adapters depend on the core; the
core depends on nothing external. Side-effect boundaries are explicit:
mutations are plan-first, digest-bound, journaled, and read back exactly;
production/destructive actions require fresh interactive TTY confirmation and
can never be scheduled; remote data is never shell-evaluated.

## Locked target core flow (for context)

```text
Operator + Route Grant -> CLI -> local daemon (sole state/transition
authority) -> typed workflow + plan engine -> typed effects + adapters
    -> Herdr · Git/GitHub · agent harnesses
```

- **XDG config**: one canonical TOML config plus one explicit optional
  private downstream policy overlay; no implicit profile/repository merge
  stack. The overlay is private/deployment-owned and never required by the
  public core.
- **SQLite state + audit**: leases, idempotency claims, schedules, journals,
  and recovery state; issues own intent, live read-back owns observed
  reality.
- **Trust boundary**: same-user TTY approval is an *accidental-safety*
  boundary, not cryptographic isolation; hardened isolation requires a
  separate OS principal/container plus external branch controls.
- **One daemon per host** on a per-user Unix socket; read-only CLI operations
  remain usable without the daemon.

## Scheduler-cutover truth

herdr-fleet does **not** replace Hermes Agent, the Hermes scheduler as a
platform, Corral's daemon, Herdr's server, or unrelated research/office/host-
maintenance automation. Its planned scope is **portable fleet scheduling**
(pause/resume/rearm/supervision/recovery/cleanup for software-repository
fleets). A private host's cron/service/caller census and any legacy-scheduler
disposition are out of scope for this public project and remain private
concerns tracked elsewhere. **Issue #2 changes no live state**: no cron,
launchd, service, caller, daemon, live config, or runtime state is touched by
this repository's content.

## Corral's actual current path (separate product)

```text
Herdr per-user Unix socket -> corrald -> FleetNotifier (iOS)
```

Corral is an independent, optional, **read-only** observability product. It
has no runtime dependency on herdr-fleet, and herdr-fleet has none on Corral.
If a concrete need ever justifies it, a future Corral adapter may consume the
versioned herdr-fleet local read/event contract — shown **dashed** and
labeled **optional future adapter** in the committed diagram. Nothing in this
repository implies a required herdr-fleet → Corral edge.

## YAGNI rule

No speculative structure is added before its first real owner exists. The
bootstrap therefore has no `domain/`, `adapters/`, `commands/`, `schemas/`,
`examples/`, plugin, or TUI directories, no async runtime, no HTTP/GitHub
client, no tracing stack, no config parser, and no dependency on clap. A
future slice may introduce each only with a demonstrated need and its own
review.

## Corral reuse/provenance process

Corral's public evidence and discipline (see its issue trail) inform this
repository's gates — fmt-first CI, real RED/GREEN scanner proofs, no
`pull_request_target`, no `producer | grep -q` pass/fail, platform coverage
on both host families, blocking license/advisory checks. This repository
reuses that *discipline*, not Corral product code, app machinery, launchd
installers, or UI/demo tooling. Any future code reuse follows normal open-
source provenance (license-compatible, attributed), reviewed in the slice
that needs it.

## Contract artifacts (issue #3)

The locked 1.0 decisions are decomposed into machine-checked contract
artifacts under [`contracts/`](contracts/README.md): capability map,
schema registry, per-surface specifications (config, CLI output,
plans/grants/epochs/outcomes, workflow DAGs, daemon protocol, state and
journal, capability negotiation, review evidence), trust and risk models,
the Corral archaeology/provenance matrix, compatibility and benchmark
policy. Synthetic fixtures live under `schemas/fixtures/` and are verified
by `scripts/check-contract-fixtures.py` + its self-test. The registry and
specs bind the *target* only; no `src/`, daemon, adapter, or migration
behavior exists yet (bootstrap rule).

## Links

- Committed target artifacts: [`architecture/`](architecture/README.md)
- ADR-0001 (public core / private overlays), ADR-0002 (branch model),
  ADR-0003 (companion boundary + harness neutrality)
- 1.0 contracts (issue #3): [`contracts/`](contracts/README.md)
- [README](../README.md) · [DEVELOPMENT](DEVELOPMENT.md) ·
  [WORKFLOW](WORKFLOW.md) · [RELEASING](RELEASING.md)
