# Spec: review evidence contract (AC4)

Refs #3 (AC4: review evidence binds feature head SHA, current
integration-base SHA, workflow hash, and policy hash; any relevant change
invalidates it). Family: `hf-evidence/v1`. Fixtures:
[`evidence/`](../../schemas/fixtures/evidence/evidence.valid.json). Design commitment (locked
spec: "Integration merge requires a distinct exact-head reviewer and hosted
checks bound to feature SHA, current integration-base SHA, workflow hash,
and policy hash").

## Record: `hf-evidence/v1`

```json
{"schema":"hf-evidence/v1","evidence_id":"ev_0123456789abcdef",
 "feature_head":"<40hex>","integration_base":"<40hex>",
 "workflow_hash":"<64hex>","policy_hash":"<64hex>","verdict":"pass",
 "checks":[{"name":"hosted-ci","status":"passed"},
           {"name":"exact-head-review","status":"passed"}],
 "created_at":"2026-09-06T00:00:00Z"}
```

Normative rules:

- All four bindings are required 40/64-hex fields: `feature_head` (the
  reviewed feature branch head), `integration_base` (the exact
  integration-base SHA the review was performed against — re-basing the
  feature or advancing the base changes this field), `workflow_hash`, and
  `policy_hash`.
- `verdict` closed set: `pass` | `fail`.
- `checks` is a non-empty list of named checks with closed statuses
  `passed` | `failed` | `pending`. A check with an unknown status is
  refused (`evidence.malformed.json` uses `running`).
- A `pass` verdict is only meaningful while every binding still matches the
  live state: **any relevant change invalidates the evidence**. Relevant =
  feature head moved, integration base advanced/changed, workflow document
  hash changed, or policy hash changed (config/overlay changed). The daemon
  re-checks all four before an integration merge; stale evidence is refused
  like any other stale state (exit class 4, [spec-cli.md](spec-cli.md)).
- `feature_head` and `integration_base` must be exact 40-hex SHAs — no
  branch names, no abbreviated refs, no "current tip" indirection.

## Review procedure contract (locked spec, for child #8)

- The reviewer is a **distinct exact-head reviewer**: a different reviewer
  role/identity than the implementer, evaluating the exact committed head
  recorded in the evidence.
- Hosted CI checks must run against the recorded feature head (never a
  moved head); CI status is one of the named checks.
- Evidence records are durable (journaled) so the merge decision is
  auditable after the fact; a merge without a valid, current evidence
  record is refused.
- Storage (issue #8): the daemon keeps durable evidence rows and recorded
  first-real-write approval rows in its SQLite state (migration m0003,
  schema v3). A `review_evidence` apply writes the row under the state
  lock before the idempotency claim resolves; the merge gate then
  revalidates every binding (feature head, integration base, workflow
  hash, policy hash, verdict, named checks) against the live refs — any
  moved binding refuses with `refusal.evidence.stale`. The row is
  invalidated when the plan's workflow/policy hash set changes; latest
  per instance, append-only history is retained.

## Fixture map

Accept: `evidence.valid.json` (all four bindings + two passed checks).
Refuse: unknown check status, unknown version.
