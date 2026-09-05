# Spec: deterministic plans, plan digests, route grants, state epochs, idempotency keys, typed outcomes

Refs #3. Families: `hf-plan/v1`, `hf-grant/v1`, `hf-epoch/v1`,
`hf-outcome/v1`. Fixtures under
[`plan/`](../../schemas/fixtures/plan/plan.valid.json), [`grant/`](../../schemas/fixtures/grant/grant.valid.json),
[`epoch/`](../../schemas/fixtures/epoch/epoch.initial.valid.json), [`outcome/`](../../schemas/fixtures/outcome/outcome.succeeded.valid.json).
Design commitment (locked spec: plan-first, digest-bound, freshly
revalidated, journaled before effect, idempotency-keyed, exactly read back).

## 1. Deterministic plans: `hf-plan/v1`

A plan is the complete, typed description of intended effects for one
issue-bound unit of work: repository identity, pinned workflow id + hash,
the state epoch it was computed against, the exact issue/acceptance
revision, and an ordered list of typed steps.

```json
{"schema":"hf-plan/v1","plan_id":"hf_plan_0123456789abcdef","workflow_id":"fleet-doctrine-1",
 "workflow_hash":"<64hex>","state_epoch":3,"repository":"example-org/widgets",
 "issue":{"number":123,"revision":"<40hex>"},
 "steps":[{"id":"p1","kind":"checkout","params":{"ref":"staging"}}, ...]}
```

Normative rules:

- `issue.revision` is the **exact acceptance revision** the plan is bound
  to (issue body/acceptance text hash or issue-level commit pin recorded by
  the grant issuer). Changing acceptance text invalidates plans bound to
  the old revision.
- Step `kind` is a closed set (`checkout`, `worktree_create`,
  `harness_start`, `prompt`, `collect_outcome`, `review_evidence`, `merge`,
  `cleanup`, `publish`). There is no shell/embedded-code step; a document
  carrying an unknown kind is refused (`plan.malformed.json`).
- Plans do not classify risk; risk comes from the target/effect table in
  the daemon ([risk-model.md](risk-model.md)) — nodes cannot downgrade it.

### Canonical serialization and digest

- Canonical bytes: JSON with keys sorted lexicographically, compact
  separators, ASCII escaping, single trailing LF (shared rule,
  [schema-registry.md](schema-registry.md)).
- Plan digest = lowercase hex sha256 over the canonical bytes. `plan.valid.json`
  is stored in canonical form and the manifest pins its known-answer digest;
  `plan.noncanonical.json` (same content, pretty-printed) is refused with
  `REFUSE_NONCANONICAL`.
- The digest is what grants/audit records bind to, and what apply
  revalidation re-computes immediately before effects.

## 2. Route grants: `hf-grant/v1` (AC3)

A route grant is the only authorization to start durable work:

```json
{"schema":"hf-grant/v1","grant_id":"gr_0123456789abcdef","repository":"example-org/widgets",
 "issue":{"number":123,"revision":"<40hex>"},"workflow_hash":"<64hex>","policy_hash":"<64hex>",
 "phase":"merge","scope":"worktrees/issues/123","caps":["read","worktree","spawn","prompt","review","merge"],
 "expires_at":"2026-09-13T00:00:00Z","state_epoch":3,"created_at":"2026-09-06T00:00:00Z"}
```

AC3 bindings, all required fields of the document:

- repository identity (`repository`),
- exact issue and acceptance revision (`issue.number`, `issue.revision`),
- workflow hash (`workflow_hash` — canonical digest of the pinned workflow
  document, [spec-workflow.md](spec-workflow.md)),
- policy hash (`policy_hash` — digest of the effective config+overlay),
- allowed phase (`phase`, closed set), scope (`scope` — path-scoped lanes
  and overlap checks), caps (`caps`, closed subset),
- expiry (`expires_at`),
- state epoch (`state_epoch`).

Semantics: labels/comments alone never authorize (trust model T6). Grants
are consumed/checked by the daemon, expire, and die with their epoch. A
grant missing any binding is refused (`grant.malformed.json` removes
`state_epoch`).

## 3. State epochs: `hf-epoch/v1` (AC7)

Epochs give every durable claim a generation anchor:

```json
{"schema":"hf-epoch/v1","epoch":1,"created_at":"2026-09-06T00:00:00Z",
 "reason":"initial","prior_epoch":null}
```

- The initial epoch has `prior_epoch: null`; every later epoch (reason
  `restore` or `security_rotation`) **must** reference its `prior_epoch`
  (`epoch.malformed.json` drops it).
- **Restore semantics (AC7)**: a restore creates a new state epoch;
  prior grants and digests are invalidated; interrupted work is marked
  ambiguous; external state must be reconciled by a human/operator before
  new grants issue. The `hf-epoch/v1` record with `reason:"restore"` is the
  durable marker of that rotation.

## 4. Idempotency keys

Format: `ik_` + 8-64 `[a-z0-9-]` (registry scalar table). Rules:

- Every mutating apply carries one (`hf-rpc-request/v1` `apply` requires
  `params.idempotency_key`; see [spec-daemon.md](spec-daemon.md)).
- The daemon records the key in the audit journal before the effect (AC6)
  and in the typed outcome; replaying an effect with a consumed key returns
  the recorded outcome instead of re-executing.
- Exactly-once across external systems is never claimed; the key gives
  at-most-once dispatch plus exactly-once read-back of the recorded result.

## 5. Typed outcomes: `hf-outcome/v1`

Every plan step ends in one typed outcome:

```json
{"schema":"hf-outcome/v1","plan_id":"hf_plan_0123456789abcdef","step_id":"p5",
 "status":"succeeded","idempotency_key":"ik_apply-20260906-0001",
 "observed_at":"2026-09-06T00:00:00Z","result":{"exit_code":0},"error":null}
```

- `status` closed set: `succeeded` | `failed` | `ambiguous` | `refused` |
  `superseded`.
- `failed`/`refused` outcomes must carry an `error` shaped like
  `hf-error/v1` (`outcome.malformed.json` is a failed outcome without one).
- `ambiguous` is reserved for interrupted/restored work (AC7: restore marks
  interrupted work ambiguous) and always requires external reconciliation.

## Fixture map

| Fixture | Expectation |
| --- | --- |
| `plan/plan.valid.json` | accept + pinned known-answer digest |
| `plan/plan.noncanonical.json` | refuse (non-canonical bytes) |
| `plan/plan.malformed.json` | refuse (shell step kind — closed set) |
| `plan/plan.unknown-version.json` | refuse (unknown version) |
| `grant/grant.valid.json` | accept (all AC3 bindings) |
| `grant/grant.malformed.json` | refuse (missing state epoch) |
| `grant/grant.unknown-version.json` | refuse |
| `epoch/epoch.initial.valid.json`, `epoch/epoch.restore.valid.json` | accept |
| `epoch/epoch.malformed.json` | refuse (restore without prior epoch) |
| `epoch/epoch.unknown-version.json` | refuse |
| `outcome/outcome.succeeded.valid.json`, `outcome/outcome.failed.valid.json` | accept |
| `outcome/outcome.malformed.json` | refuse (failed without error) |
| `outcome/outcome.unknown-version.json` | refuse |
