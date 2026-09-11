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

## Fixture map

Accept: `migration.valid.json` (0→1, checksummed), `audit.valid.jsonl`
(mutate journaled before + read record). Refuse: non-linear migration,
mutation journaled after the fact, unknown-version variants of both.
