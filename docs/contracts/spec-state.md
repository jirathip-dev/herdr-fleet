# Spec: SQLite migration/journal/audit semantics, backup/restore boundaries, retention

Refs #3. Families: `hf-migration/v1`, `hf-audit/v1` (JSONL records of the
journal/audit tables). Fixtures under
[`migration/`](../../schemas/fixtures/migration/migration.valid.json),
[`audit/`](../../schemas/fixtures/audit/audit.valid.jsonl). Design commitment (locked spec:
"SQLite owns leases, idempotency claims, schedules, journals, and recovery
state"; AC6 journal-before-mutation; AC7 restore semantics).

## SQLite role and invariants

- One SQLite state database per user/daemon owns: leases, idempotency
  claims, schedules, journals, audit records, migration bookkeeping, and
  recovery state. SQLite is the durable authority for these; live
  Git/Herdr/GitHub read-back owns observed reality; issues own intent.
- Writes are versioned, locked, atomic, and recoverable. The database is
  daemon-owned; the CLI never writes it behind the daemon's back.

## Migration semantics: `hf-migration/v1`

Migrations are linear, ordered, and journaled:

```json
{"schema":"hf-migration/v1","migration_id":"m0001_create_state_v1","applies_from":0,
 "applies_to":1,"checksum":"<64hex>","description":"create leases, idempotency claims, journal, and audit tables"}
```

- `migration_id` format `mNNNN_<snake>`; `applies_to` must be exactly
  `applies_from + 1` (linear chain; `migration.malformed.json` jumps
  0 → 3 and is refused).
- `checksum` is sha256 over the migration artifact's canonical bytes; the
  checksum is recorded before the migration executes.
- Migration procedure (future implementation contract): run inside a
  transaction against the recorded `PRAGMA user_version`; apply each
  pending manifest in order; record success; on any failure the
  transaction rolls back and the daemon fails closed (never half-migrated).
- Runtime/schema/workflow/migration changes reset the 1.0 soak clock
  (locked spec release rule) — migration records make that determination
  auditable.

## Journal/audit semantics: `hf-audit/v1` (AC6)

Audit intent is durably recorded **before** any mutation effect; inability
to journal fails closed — the mutation does not start.

```json
{"schema":"hf-audit/v1","seq":41,"action":"mutate.merge",
 "target":"example-org/widgets:staging<-issues/123",
 "idempotency_key":"ik_apply-20260906-0001","plan_hash":"<64hex>",
 "grant_id":"gr_0123456789abcdef","epoch":3,
 "recorded_before_mutation":true,"at":"2026-09-06T00:00:00Z"}
```

- Every record: monotonic `seq`, closed action code (`mutate.*` for
  mutations, `read.*` for reads), target, idempotency key, optional plan
  hash/grant id, epoch, and the `recorded_before_mutation` flag.
- A `mutate.*` record with `recorded_before_mutation:false` is **refused**
  (`audit/audit.malformed.jsonl`) — the fail-closed rule is in the schema
  itself, so no implementation can journal after the fact and still produce
  valid records.
- The journal is append-only; consumers read it as JSONL
  (`journal.tail` RPC). Redaction applies before any record is written.

## Backup/restore boundaries and retention

| Boundary | Content | Owner | Retention (default; configurable via policy overlay) |
| --- | --- | --- | --- |
| Daemon-owned backup | SQLite snapshot + migration chain + journal tail, created via `backup.create` | daemon-owned state dir | keep last N point-in-time backups; prune only with a `cleanup` grant; never touches external repos |
| External repositories/worktrees | NOT backed up by herdr-fleet | Git remotes + operator | Git history is the operator's own retention domain; herdr-fleet never copies external state into its backup |
| Logs/events | JSONL event stream | daemon | bounded by retention policy; rotated without data loss of journal records |

- Backup covers daemon-owned state only; the boundary is explicit so that
  "restore" can never be mistaken for a repository restore.
- **Restore (AC7)** = restore.begin on a daemon-owned backup → rotate epoch
  (`hf-epoch/v1`, `reason:"restore"`) → invalidate prior grants/digests →
  mark interrupted work ambiguous → require external reconciliation before
  new grants. See [spec-plans.md](spec-plans.md) §3.
- Retention defaults are design commitments; actual sizes/budgets are
  [awaiting-evidence] (benchmarks, [benchmarks.md](benchmarks.md)).

## Lifecycle additions (issue #9)

- **Migration `m0004_schedules_lifecycle_v4`** (0 → 4 chain) adds two
  columns to the `schedules` table: `doc` (the canonical `hf-schedule/v1`
  document text) and `updated_at`. `SCHEMA_VERSION` is 4.
- **Schedule rows** are the durable recurring-grant cadence records
  (spec-lifecycle.md): `enabled` is a hard pause flag — the evaluation
  path never toggles it; `next_run_at` is the single-flight window guard
  (NULL means immediately due, e.g. after create/resume). Evaluation
  commits window advance + `read.schedule.ran` audit + `schedule.ran`
  event atomically; a refused evaluation (expired, policy/issue changed)
  parks the row with a journaled reason.
- **Retention of daemon-owned backups** is bounded by age, count, and
  total bytes (`backup::BackupPolicy` defaults: keep 8, 90 days, 1 GiB);
  `backup.create` prunes verified pairs outside the policy and journals
  the prune intent (`mutate.backup.prune`) before removing anything.
- **Deletion of audit records is itself an explicit destructive
  operation**: the single `DELETE FROM audit` lives inside the append-side
  retention prune (`prune_audit_locked`, always keeping the chain
  genesis); there is no purge/truncate surface anywhere
  (static probe in tests/no_network_surface.rs).

## Handoff additions (issue #73)

- **Migration `m0005_lane_replacements_v5`** (0 → 5 chain) adds the lane
  replacement record tables: `lane_replacements` (one record per logical
  lane generation, `UNIQUE (lane_id, generation)`, with the typed phases
  `requested` → `quiescing` → `checkpointed` → `retired` → `starting` →
  `adopting` → `adopted`, explicit `pending`/`held`/`ambiguous`/`cancelled`
  outcomes, and the bound source session/process/role/worktree/reason
  identity) and `lane_replacement_events` (the transactional transition
  history committed with each record write). `SCHEMA_VERSION` is 5. The
  migration is purely additive — no existing table or row is touched, so
  stored grants are never reinterpreted by the upgrade.
- **Request-only semantics**: the `lane.replacement.*` RPCs (see
  spec-daemon.md) write durable records and journal their intents through
  the same claim machinery as every daemon mutation; nothing on the surface
  spawns, kills, or touches Git, and no grant is required, issued, or
  consumed. Transitions are transactional compare-and-set (stale
  generation, invalid order and replayed expectations cannot advance); a
  `held` record refuses advancement durably across restarts; restart
  reconciliation marks an interrupted transition `ambiguous` (external
  reconciliation required); and cancellation before retirement invalidates
  a pending replacement while leaving the original lane generation
  untouched. An agent may request its own retirement but can never
  authorize its own replacement effects.

## Checkpoint additions (issue #74)

- **Migration `m0006_lane_checkpoints_v6`** (0 → 6 chain) adds the lane
  checkpoint table `lane_checkpoints`: one checkpoint per replacement
  record (`UNIQUE (replacement_id)`), holding the canonical snapshot of
  the captured lane (role/task, worktree/branch/head/base, dirty and
  untracked inventory with their bounded integrity digests, report round +
  reviewed sha, pending gates, observed child commands, execution/ack
  state, orchestrator references, daemon-observed outstanding operations),
  the two observation digests, the snapshot digest, and the generated
  brief digest. `SCHEMA_VERSION` is 6. The migration is purely additive —
  no existing table or row is touched, so stored grants and replacement
  records are never reinterpreted by the upgrade.
- **The checkpoint commit is atomic**: `commit_lane_checkpoint` writes the
  checkpoint row, the record's `quiescing` → `checkpointed` transition and
  the transition-history row in ONE transaction (the record's compare-and-
  set fence re-asserts pending/quiescing/generation). The compact brief is
  a deterministic derivation of the committed row: it is materialized
  after the commit, and restart reconciliation regenerates it (verifying
  `brief_digest`) when a crash lands between the commit and the artifact
  write. An artifact without a committed record can only come from a
  non-atomic implementation and fails closed (the record is parked
  `ambiguous`, never adopted or silently deleted) — so a restart yields
  either the previous complete checkpoint or the new complete one.
- **Quiescing fence**: while a pending replacement sits inside the
  quiescing window (`quiescing` or `checkpointed`, outcome `pending`), new
  replacement requests for the lane are refused (`refusal.replacement.fenced`)
  — a lane cannot fork into a second successor slot mid-handoff — and the
  capture itself is only admitted at the exact `quiescing` boundary.
- **Two observations + closed contract**: the capture requires two
  observations of the lane that are canonically identical (a disagreement
  refuses with `refusal.checkpoint.changed`); every required field must be
  present, bounded and valid (`refusal.checkpoint.incomplete` — missing
  evidence is never silently omitted); active external harness execution
  requires a supported quiescence acknowledgment AND a process/child
  observation (`refusal.checkpoint.ack`); active or ambiguous
  side-effecting child commands hold completion (`refusal.checkpoint.held`
  — nothing is signalled, killed, or cleaned up to obtain a snapshot); and
  required data whose generated brief exceeds the enforced 3 KiB bound is
  a typed hold (`refusal.checkpoint.oversize`), never a truncation.
- **Orchestrator checkpoints** reference existing worker/reviewer
  replacement records (existence and role are validated;
  `refusal.checkpoint.references` otherwise) plus bounded pending
  completion-event tokens, and never alter the referenced lanes.
- **Restart reconciliation**: an interrupted `lane.checkpoint.create`
  claim reconciles against its commit marker (the checkpoint row) instead
  of blindly parking the record — the record's transition commits
  atomically with the row, so the record is never in doubt.

## Retirement additions (issue #75)

- **The binding (the plan/grant unit of this slice)**: a retirement
  request carries `binding` = {lane generation, source session identity,
  source process identity, committed checkpoint digest}. `begin_lane_retirement`
  validates it against the durable record and the checkpoint that completes
  it in one read BEFORE any effect: a changed generation, session, process
  or checkpoint digest refuses with `refusal.retirement.binding` (and
  nothing is signalled), a `held` record refuses as the paused state
  (`refusal.replacement.held`), a cancelled/ambiguous record refuses
  (`refusal.replacement.invalidated` / `refusal.replacement.ambiguous`),
  and a record that has not reached the `checkpointed` boundary refuses
  with `refusal.replacement.order`. The request is journaled as a claim
  like every other daemon mutation, so the exact binding (including the
  checkpoint digest) is durable in the claim's request line and the audit
  journal.
- **Immediate pre-stop quiescence recheck**: the same request carries
  `recheck` = {observed_at, session, process, children[] (command + state
  from the closed `exited`/`active`/`ambiguous` set), active}. Every child
  must be observed `exited` and external execution must not be active
  (`refusal.retirement.held` otherwise), a process identity that is missing
  or not a process identity is an unknown identity and holds, and an
  observed identity that differs from the record refuses as a binding
  mismatch. A hold writes nothing: nothing is signalled, killed, or cleaned
  up to obtain quiescence and no process group is ever addressed.
- **Atomic retirement commit**: `commit_lane_retirement` writes the
  record's `checkpointed` → `retired` transition and its transition-history
  row in ONE transaction, fenced on the exact generation/phase/outcome and
  on the committed checkpoint digest (a missed fence is classified typed
  and changes nothing). The bounded evidence summary is recorded as the
  transition reason (never truncated). No new table is added: the phase
  transition is the commit marker.
- **Held, not escalated**: a graceful stop whose delivery cannot be
  confirmed, a confirmation that cannot prove absence, or a retirement
  that cannot commit after the stop parks the record `ambiguous` (external
  reconciliation required) with the no-repeat decision recorded; there is
  no SIGKILL, no broad pattern, no process-group signal and no authority
  escalation anywhere on the path.
- **Restart reconciliation**: an interrupted `lane.retire` claim is
  reconciled against the backend confirmation read-back — exact absence
  (process absent AND registration released for the bound session and
  generation) completes the retirement, and every other outcome parks the
  record `ambiguous`. The stop is issued at most once: reconciliation never
  repeats a signal, and never against a reused identity.

## Successor additions (issue #76)

- **The successor boundary**: `begin_lane_successor` validates one start
  request (`replacement_id`, `binding` = {generation, checkpoint_digest,
  nonce}, `successor` = {session, kickoff_receipt}, `harness`, `admission`)
  and commits exactly ONE durable succession row (`lane_successors`,
  m0007/schema v7, `UNIQUE (replacement_id)`) BEFORE any spawn. The
  successor id is deterministic (`su_` + the first 16 hex of sha256 over
  `hf-lane-successor/v1|<replacement_id>`), so a crash-then-retry, a restart
  and a bounded same-nonce retry all address the identical successor. A
  record that has not retired refuses `refusal.replacement.order`; a record
  left ambiguous or cancelled refuses `refusal.replacement.ambiguous` /
  `refusal.replacement.invalidated`; a held record refuses
  `refusal.replacement.held`. A committed boundary owned by another nonce
  refuses `refusal.successor.nonce` (one generation/nonce owns startup and a
  second nonce can never take over a started successor), a mismatched
  bound session refuses `refusal.successor.binding`, and any start after the
  boundary committed refuses `refusal.successor.exists` (the adoption path
  continues it). A missing committed checkpoint or a changed checkpoint
  digest refuses `refusal.successor.binding` and nothing is invoked.
- **Bounded startup, fresh by construction**: the start rechecks the source
  absence first (a live or unreadable source refuses
  `refusal.successor.source_live`; an unresolvable workspace executable
  refuses `refusal.unavailable.harness`), then starts the successor session
  over the workspace (Herdr) adapter row (`session start <session> --json`)
  on the SAME logical lane and the record's worktree, and verifies the
  successor from a fresh read-back — a booted process alone is never a
  usable successor (`refusal.successor.held`), and a session or process
  identity that is already owned refuses `refusal.successor.reused`. A
  refused spawn leaves the committed boundary `delivery` = `none` for a
  bounded same-nonce retry (the attempt counter refuses
  `refusal.successor.attempts` when exhausted; a delivered successor
  re-verifies instead of spawning again). Admission failures are the
  lifecycle codes (`refusal.admission.proof_missing` / `proof_stale` /
  `cap_missing` / `cap_global` / `cap_repository` / `cap_harness` /
  `monorepo_overlap`) and hold with no child and no state change.
- **Adoption**: `commit_lane_adoption` requires the fresh `observation` and
  `reobservation` of the lane to be canonically identical
  (`refusal.successor.binding` otherwise), compares the fresh observation
  against the DURABLE checkpoint snapshot (head/base/dirty/untracked/report
  scalars plus the sorted gates and children pairs) and refuses with
  `refusal.successor.differs` naming the differing fields — the recorded
  state is never replayed as success and reconciliation is required. The
  record's `starting` → `adopting` … `adopted` transitions commit
  atomically with their history row (the phase transition is the commit
  marker) and the adoption evidence is digest-bound. A successor that is
  not verifiably usable refuses `refusal.successor.held`; a verify failure
  contradicted by a process-only observation parks the record `ambiguous`.
- **Identity and completions**: the successor continues the retired
  session's role and the record's role/worktree, and the orchestrator's
  worker/reviewer references and pending completion events stay the
  record's. `lane.successor.consume` records the consumption at most once
  (`consumed_at`, `consumer`, `consumed_events`) and refuses a second
  consumption (`refusal.successor.event_consumed`), an event that is not a
  recorded pending completion of the replacement or a worker that is not a
  referenced lane (`refusal.successor.event`), and a successor that has not
  adopted (`refusal.replacement.order`).
- **Restart reconciliation**: an interrupted `lane.start` claim is
  reconciled by reading the successor back (the committed boundary is the
  commit marker) — a verifiable successor completes the boundary, every
  other read-back parks the record `ambiguous` and refuses the retry until
  reconciliation resolves it, so a restart never spawns twice and never
  silently writes off the boundary.

## Fixture map

Accept: `migration.valid.json` (0→1, checksummed), `audit.valid.jsonl`
(mutate journaled before + read record). Refuse: non-linear migration,
mutation journaled after the fact, unknown-version variants of both.
