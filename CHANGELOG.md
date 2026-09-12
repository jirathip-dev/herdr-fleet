# Changelog

All notable changes to this project are documented here. This project
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) until a
release process activates (docs/RELEASING.md), then semver applies.

## [Unreleased]

### Added (bootstrap)

- Public repository foundation for canter (issue #2):
  - Single-package Rust scaffold: `canter` library + binary with
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
    public `skills/canter` skill.
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
- State layer: SQLite migrations m0001-m0008, audit/event JSONL journals
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

### Added (issue #75 — Guarded single-session retirement)

- One guarded retirement of a single checkpointed source session
  (`lane.retire` RPC, no schema change): the request binds the record's lane
  generation, source session/process identity and the committed checkpoint
  digest, and a changed identity, checkpoint or paused (`held`) state
  refuses BEFORE any effect (`refusal.retirement.binding` — nothing is
  signalled). The immediate pre-stop quiescence recheck must observe every
  child `exited` and no active external execution; unknown child activity or
  an unknown process identity holds (`refusal.retirement.held`), and a hold
  writes nothing, kills nothing and never addresses a process group.
- The graceful stop is ONE bounded request over the existing workspace
  (Herdr) session adapter row (`session interrupt <session> --json`), with
  no retry, no SIGKILL, no broad pattern, no process-group signal and no
  authority escalation in the slice; a stop that never ran refuses
  (`refusal.unavailable.harness`) and an unconfirmed delivery holds and
  parks the record `ambiguous`. Adapter profiles that do not declare the
  required `interrupt` + `observe` capabilities are unsupported and refuse
  with `unknown.capability` before the claim.
- The retirement is confirmed by backend evidence only — the process is
  absent AND the ownership/registration is released for the bound session
  and generation; a pane text or a `done` label is never read, a reused
  process/pane or a stale registration fails closed
  (`refusal.retirement.reused`), and child lanes, worker/reviewer records
  and worktree bytes are never touched. The `checkpointed` → `retired`
  transition commits atomically with its history row (the phase transition
  is the commit marker), and restart reconciliation of an interrupted claim
  reconciles exact absence without ever repeating a signal — never against a
  reused identity. Excluded, as the issue requires: successor start,
  process-tree cleanup, whole-fleet restart and real deployment activation.

### Added (issue #76 — Bounded start, verification and adoption of one successor)

- ONE bounded start of one successor for a retired replacement and its
  single adoption (`lane.start` / `lane.adopt` RPCs, `lane_successors`
  table via m0007/schema v7): the start commits exactly one successor owner
  boundary (a deterministic `su_` successor id bound to the lane
  generation, the startup nonce, the target session and the committed
  checkpoint digest) BEFORE any spawn, and reuses the existing admission
  gate and the workspace (Herdr) session adapter row
  (`session start <session> --json`) — the same logical lane and worktree,
  never a new infrastructure model.
- A booted process alone is never a successor: adoption refuses whenever
  the successor is not verifiably interactive/usable
  (`refusal.successor.held`), a process observation without a usable
  session parks `ambiguous`, and the adoption RE-QUERIES the lane and
  compares the fresh observation against the durable checkpoint snapshot
  (`refusal.successor.differs`) instead of replaying the recorded state as
  success.
- Adoption reconciles the orchestrator identity: the successor keeps the
  retired session's role, the record's worker/reviewer references and its
  pending completion events; the events are consumed at most once by a
  logged consumer (`lane.successor.consume`) and a started successor is
  fenced while the record is held (`refusal.replacement.held`) — a paused
  lane cannot activate a booted successor.
- Capacity remains the existing bounded admission path: a missing host
  proof, a missing admission claim or an exhausted global/per-repository/
  per-harness cap holds typed (`refusal.capacity.*`) with NO child, no
  state change and a bounded fresh retry only.
- A crash between the boundary commit and the spawn is reconciled on
  restart (successor read-back before any retry; non-verifiable evidence
  parks `ambiguous` and refuses the retry until reconciliation resolves
  it), so a restart never silently writes off the boundary and never
  spawns twice. Excluded, as the issue requires: scheduler-driven successor
  work, automatic retries, process-tree cleanup and real deployment
  activation.

### Added (issue #77 — Target profile identity/fingerprint bound to the successor)

- One replacement may be requested under an EXPLICIT profile-configuration
  revision (`params.profile`, the canonical `hf-profile-binding/v1`
  document a human reviewed; optional — an unbound request keeps the
  pre-#77 behavior). The document carries the target profile key/kind, the
  intended `provider`/`model` pair sourced from the supported profile
  configuration, the authorized fallback pairs, the configured limits
  (metadata overrides — reported as configured limits, never as proof of
  provider support), the declared binding-introspection support and the
  credential DIGESTS (never values; a declared-but-absent credential is
  recorded as `unset`). Its `revision` is the sha256 over that canonical
  material: the daemon recomputes it and refuses a presented revision that
  does not fingerprint its own material (`refusal.profile.revision`), so a
  revision is never a claim. Any relevant configuration or credential
  change produces a different revision, and a start that presents the
  changed revision refuses
  (`refusal.profile.revision`: a newly reviewed plan is required) with
  nothing spawned; a missing or unexpected binding refuses
  (`refusal.profile.binding`).
- The reviewed plan is durable (`lane_replacement_profiles`, m0008/schema
  v8, committed in the SAME transaction as the replacement record — a
  replacement is bound from birth or unbound, never retro-fitted) and is
  re-validated on read. The start must present the identical plan, run the
  profile the plan names, and the successor read-back is classified against
  it: the planned pair verifies (`matched`), an AUTHORIZED fallback is
  accepted and reported distinctly (`fallback`), an unexpected
  provider/model stays fenced (fail closed, parked for reconciliation), and
  a read-back that reports nothing leaves the actual binding `unknown` — a
  profile that declares binding introspection instead holds with an honest
  capability hold, and the actual binding is NEVER copied from the
  requested configuration. The binding verdict (intended vs actual from
  authoritative adapter evidence, the reviewed revision, the configured
  limits) is recorded in the successor verification/adoption evidence and
  returns on `lane.start`/`lane.adopt`; `lane.replacement.status` exposes
  the bound plan and `config show --json` previews the exact plan (and the
  credential names present/missing) the human reviews.
- Excluded, as the issue requires: live in-place provider switching,
  editing user profiles, provider benchmark/availability services and
  automatic fallback-policy expansion. The source session is untouched
  until normal quiescence/retirement; no new store, scheduler or automatic
  rotation trigger exists.

### Changed (issue #106 — product rename `herdr-fleet` → `canter`)

- The product is renamed: package + library crate `canter`, canonical binary
  `canter`, and `--help`/`--version`, docs, skills, install/archive scripts,
  CI references, and the changelog all use the new name. Schema families
  (`hf-*`), envelopes, exit codes, and the daemon wire contract are
  unchanged, and the schema/version facts (`--version`) stay accurate.
- Compatibility is normative in
  [docs/contracts/compatibility.md](docs/contracts/compatibility.md#product-rename-issue-106):
  the pre-rename `herdr-fleet` binary ships as an alias running the identical
  CLI (deprecation warning on stderr); a pre-rename state/runtime tree is
  adopted **in place** (never copied, moved, migrated, or deleted) and a
  pre-rename config is still discovered; `HERDR_FLEET_CRASH_POINT` remains
  honored; live Herdr integration ids (`custom:herdr-fleet-pi|-jcode`,
  `herdr-fleet-lane`) are retained; frozen v0.1.0 architecture renders and
  historical `.report-*.md` records keep the old name by design.
- Machine-checked by `tests/rename_sweep.rs` (no stale reference outside the
  documented legacy set) and `tests/rename_compat.rs` (alias binary, legacy
  tree adoption, legacy config discovery, legacy env var).

### Changed

- Documentation polish (issue #12, PR #13): security recipe doc line fix
  and dark-preview renderer note; `docs/` claims kept in step with the
  shipped surface.

### Fixed

- Nothing yet.

### Security

- Private vulnerability reporting via GitHub's private advisory flow
  (SECURITY.md); no secrets in public issues.
