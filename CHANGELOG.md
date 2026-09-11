# Changelog

All notable changes to this project are documented here. This project
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) until a
release process activates (docs/RELEASING.md), then semver applies.

## [Unreleased]

### Added (bootstrap)

- Public repository foundation for herdr-fleet (issue #2):
  - Single-package Rust scaffold: `herdr-fleet` library + binary with
    truthful `--help`/`--version` only; edition 2024, pinned toolchain
    1.97.1, committed `Cargo.lock`, zero external dependencies.
  - Real-binary CLI smoke tests (`tests/cli_smoke.rs`).
  - Canonical `just` developer gates (`just ci` aggregate) and strict
    `deny.toml` for cargo-deny.
  - Hosted CI (policy, rust-ubuntu, rust-macos, supply-chain, secret-scan)
    with stable job names, minimal permissions, full-SHA action pins, and
    no caching.
  - Public-tree privacy scanner (`scripts/check-public-tree.py`) with
    discriminating self-tests (`scripts/test-check-public-tree.py`).
  - Contributor tooling: issue/PR templates, CODEOWNERS (comment-only),
    Dependabot grouped weekly updates targeting `staging`.
  - Public docs: README, ARCHITECTURE, DEVELOPMENT, WORKFLOW, RELEASING,
    three ADRs, SECURITY, CONTRIBUTING, AGENTS, CODE_OF_CONDUCT, and the
    public `skills/herdr-fleet` skill.
  - Amendment-3 architecture artifact set committed under
    `docs/architecture/` (sanitized locked-target JSON/HTML + static
    light/dark previews + SHA-256 provenance README).

### Added (issue #3 — Contracts, PR #14)

- Versioned contract corpus under `docs/contracts/` with machine-checked
  fixtures under `schemas/fixtures/`: capability map and owner dependency
  order; schema registry; specs for config/policy, CLI/JSON (envelopes,
  exit codes 0-5, partial-freshness), plans/digests/grants/epochs/
  idempotency keys/typed outcomes, closed typed workflow DAGs with
  canonical hashing, daemon request/response + JSONL events, SQLite
  migration/journal/audit and backup/restore/retention, harness/forge
  capability negotiation, review evidence, trust model, daemon-owned
  fail-closed risk model, Corral archaeology matrix, tested Herdr + `gh`
  compatibility policy, and the benchmark corpus.
- Fixture oracle: `scripts/check-contract-fixtures.py` +
  `scripts/test-check-contract-fixtures.py` (accept/refuse/tamper
  discrimination, known-answer digests, manifest coherence).

### Added (issue #4 — Read-only core, PR #15)

- Typed `hf-config/v1` configuration: `config init` (stdout template),
  `config validate`, `config show` (refusals for unknown keys, foreign
  schema identifiers, unsupported versions, invalid overlays — exit 5).
- `doctor`: read-only prerequisite checks (git, herdr >= 0.8.2, gh auth +
  scopes) plus config status; exits 3 on missing/degraded prerequisites.
- `status`: bounded read-only observation of configured repositories
  (local git checkout + authenticated `gh`, herdr presence/version), with
  explicit freshness/completeness and per-surface degradation.
- `plan`: deterministic read-only `hf-plan/v1` rendering (canonical bytes,
  sha256 digest, redacted acceptance-revision binding; `--revision` offline
  mode; `forge.unavailable` refusal when gh is unavailable without one).
- `capabilities`: declared forge read capabilities
  (`read_refs`, `read_issues`, `read_checks`).
- Stable JSON contract: every `--json` invocation writes exactly one
  `hf-output/v1` envelope; closed exit-code set 0-5; adapter reads are
  env-isolated and redacted at the boundary.

### Added (issue #5 — Daemon foundation, PR #16)

- `daemon run`: single-writer state daemon in the foreground (per-user
  flock, SQLite state open/migrated, interrupted-claim reconciliation,
  per-user Unix socket `hf-rpc/v1`); a second daemon is refused
  (`daemon.busy`).
- `daemon status`: socket probe reporting live daemon + state facts; exit 1
  with `daemon.absent` when no daemon is running (read-only commands stay
  available without it).
- `service doctor` + `service install-plan|status-plan|uninstall-plan`:
  per-user launchd/systemd environment checks and unit-plan rendering —
  plans only, never installing/starting/stopping/querying the host service
  manager.
- State layer: SQLite migrations m0001-m0006, audit/event JSONL journals
  with mirror rebuild, backup/restore hooks, per-user path resolution.
- Socket RPC: `hf-rpc-request/v1`/`hf-rpc-response/v1` closed method set,
  typed refusal codes, `events.subscribe` stream.

### Added (issue #6 — Workflow engine, PR #17)

- Typed workflow engine: closed step-kind set, canonical serialization and
  hashing, instance state (`hf-workflow/v1`, m0002).
- Bundled Doctrine default workflow (`fleet-doctrine-1`, model-agnostic
  orchestrator/implementer/reviewer roles; fake/no-effect step kinds only).
- Route grants (`hf-grant/v1`), state epochs (`hf-epoch/v1`), role
  composition with hash-pinned roles.

### Added (issue #7 — Harness adapters, PR #18)

- Harness adapters for Hermes, Claude Code, and Codex plus a declarative
  generic argv adapter: closed operation set, typed refusal codes,
  identity-triple rule, capability negotiation against `hf-capability/v1`.
- Fake-executable contract tests (`tests/harness_adapters.rs`) — public CI
  and fork PRs need no credentials; real harness parity stays a
  human-gated clean-host smoke (AC6/AC7).
- Versioned compatibility matrix (`docs/contracts/compatibility.md`).

### Added (issue #8 — Control-plane mutations, PR #19)

- Daemon-mediated plan `apply` RPC: one typed, digest-bound,
  capability-gated plan step per request; digest/identity recomputation
  before journaling; live revalidation under the state lock (epoch vs
  plan/grant/instance, grant expiry, issue revision, workflow/policy
  hashes, step capabilities).
- Durable review evidence rows and recorded first-write approvals for
  real-external scopes (m0003/schema v3).
- Granular step kinds (checkout, worktree_create, harness_start, prompt,
  collect_outcome, review_evidence, merge, cleanup, hosted_check,
  post_merge_verify, branch_delete, approve), worktree-confined harness
  execution, typed `hf-outcome/v1` results (succeeded/failed/refused/
  ambiguous/superseded), idempotency-keyed dispatch with exactly-once
  read-back of recorded results.
- Grant/instance invalidation on material edits and epoch rotation;
  production-branch effects require fresh interactive TTY confirmation.

### Added (issue #9 — Lifecycle safety, PR #20)

- Durable recurring non-destructive schedules (`hf-schedule/v1`, m0004)
  with single-flight/coalesced evaluation and pause/resume that survives
  daemon/service/host restarts; explicit human re-arm after refusal.
- Cold-boot recovery: one fresh evaluation per due schedule before the
  socket serves; recovery independent of herdr/git/gh presence.
- Fan-out admission: host-resource proofs, capability bounds, and
  monorepo-overlap refusal.
- Cleanup archive/salvage with byte-verified manifests and retention-
  bounded backup pruning.
- Verified system-SSH remote transport contract.

### Added (issue #10 — Release readiness, PR #21)

- Deterministic platform archives with SHA-256 checksums, offline SPDX
  SBOM derived from `Cargo.lock`, and `release-provenance/v1` records
  (`scripts/build-archive.py` + self-tests).
- Clean-host verification scripts (`scripts/clean-host-verify.sh`,
  `clean-host-probe.py` + self-tests) for fresh macOS/Linux hosts.
- Measured baseline + regression-budget machinery
  (`scripts/measure-baseline.py`, committed Linux x86_64 table) and
  compatibility probe mapping — never a CI gate.
- Active in-repo release/version policy with release execution
  human-gated (`docs/RELEASING.md`).

### Added (issue #35 — Herdr 0.9 compatibility)

- Portable contract coverage for Herdr 0.9.0 event bootstrap ordering and
  live-only subscriptions, explicit workspace-group close, prompt wait
  activity, unscrolled recent pane reads, and issue #9's lack of an upstream
  retained-event dependency.
- Measured compatibility matrix: same-version 0.9.0/protocol 22 passed in an
  isolated scratch server; both 0.8.2/protocol-20 mixed directions refused
  normal API calls with `protocol_mismatch`, consistent with upstream's
  endpoint-generation boundary.
- The doctor minimum remains 0.8.2 because the required mixed-version matrix
  is red; documentation distinguishes that CLI floor from server protocol
  compatibility rather than claiming or working around interoperability.

### Added (issue #73 — Lane replacement records)

- Request-only lane replacement records: one durable record per logical
  lane generation (`lane.replacement.request|advance|hold|cancel|status`
  RPCs, m0005/schema v5) with explicit phases (requested → quiescing →
  checkpointed → retired → starting → adopting → adopted) and explicit
  `held` (parked: advancement refused) / `ambiguous` (interrupted
  transition: external reconciliation required) / `cancelled` (invalidated
  before retirement) outcomes.
- Records bind source session/process identity, role, worktree and reason;
  missing/invalid identities refuse instead of inferring an empty lane.
  Transitions are transactional compare-and-set (stale generation, invalid
  order and replayed expectations cannot advance) and the transition
  history is committed atomically with each record write and preserved
  across daemon restarts.
- The surface has no spawn/kill/Git effect and no authority uplift (no
  grants are required, issued, or consumed); an agent may request its own
  retirement but can never authorize its own replacement effects.
  Retirement execution, automatic triggers and successor execution remain
  out of scope.

### Added (issue #74 — Safe-boundary checkpoints)

- One safe-boundary checkpoint operation for a lane replacement at the
  `quiescing` boundary (`lane.checkpoint.create` / `lane.checkpoint.status`
  RPCs, `lane_checkpoints` table via m0006/schema v6): the capture validates
  TWO observations of the lane (canonically identical, or the checkpoint
  refuses with `refusal.checkpoint.changed`), requires a supported
  quiescence acknowledgment AND a process/child observation for active
  external harness execution (`refusal.checkpoint.ack` — daemon fencing
  alone is not claimed to stop arbitrary shell actions), holds completion
  on active/ambiguous side-effecting child commands (`refusal.checkpoint.held`
  — nothing is ever signalled, killed, or cleaned up to obtain a snapshot),
  refuses missing evidence (`refusal.checkpoint.incomplete`), and yields a
  typed hold when required data exceeds the enforced brief bound
  (`refusal.checkpoint.oversize` — required gates are never silently
  truncated).
- Quiescing fences new replacement slots for the lane
  (`refusal.replacement.fenced`) until the handoff resolves or is cancelled;
  capture is only admitted at the quiescing boundary; one replacement
  carries at most one checkpoint (`refusal.checkpoint.exists`).
- The checkpoint row and the record's `quiescing` → `checkpointed`
  transition commit in ONE transaction; the compact brief (≤ 3 KiB) is
  generated deterministically from the durable record, carries explicit
  evidence pointers only, and is regenerated (digest-verified) by restart
  reconciliation when a crash lands between the commit and the artifact
  write. An artifact without a committed record fails closed. Orchestrator
  checkpoints reference existing worker/reviewer records and pending
  completion events without altering them. No spawn/kill/Git effect, no
  grant, no scheduler, and no automatic trigger exists on the surface.

### Changed

- Documentation polish (issue #12, PR #13): security recipe doc line fix
  and dark-preview renderer note; `docs/` claims kept in step with the
  shipped surface.

### Fixed

- Nothing yet.

### Security

- Private vulnerability reporting via GitHub's private advisory flow
  (SECURITY.md); no secrets in public issues.
