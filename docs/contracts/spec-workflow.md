# Spec: closed typed workflow DAGs and canonical hashing/serialization

Refs #3, #6. Family: `hf-workflow/v1`. Fixtures:
[`workflow/`](../../schemas/fixtures/workflow/workflow.valid.json) (+ the bundled doctrine
fixture, below). Design commitment (locked
spec: "Closed, schema-versioned typed DAG nodes only — no shell or embedded
code. Checked-in workflows require explicit config selection and hash
pinning. The bundled Doctrine workflow is versioned. Running instances pin
it; upgrades affect only new runs unless explicitly migrated.").

## Document: `hf-workflow/v1`

```json
{"schema":"hf-workflow/v1","workflow_id":"fleet-doctrine-1",
 "nodes":[{"id":"start","kind":"start","params":null}, ...],
 "edges":[{"from":"start","to":"plan"}, ...]}
```

Normative rules:

- **Closed node kinds** (no shell, no embedded code, no free-form
  executor): `start` | `plan` | `orchestrator` | `implementer` | `reviewer`
  | `gate` | `human_approval` | `merge` | `terminal`. Node `params` are
  typed data, never code. An unknown node kind is refused
  (`workflow.malformed.json`).
- Node ids are unique slugs; edges reference existing node ids only; the
  graph must be a DAG (implementation enforces acyclicity in the engine
  slice; the fixture set checks structure).
- Roles referenced by workflow content are the default model-agnostic roles
  (orchestrator/implementer/reviewer) or explicit hash-pinned custom roles
  from config (`role.<key>.hash`); workflow content cannot invent roles.
- Default roles never encode a model/provider choice (ADR-0003).
- One authoritative workflow per repository; cross-repository initiatives
  use linked dependencies, not atomic claims (locked spec) — a workflow
  document is scoped to one repository.

## Canonical serialization and digest

- Same canonical JSON rule as plans (sorted keys, compact separators, ASCII
  escapes, one trailing LF; [schema-registry.md](schema-registry.md)).
- Workflow hash = sha256 over the canonical bytes of the workflow document.
  The hash is what config selection (`workflow.<key>.hash`), route grants
  (`workflow_hash`), plans (`workflow_hash`), and review evidence
  (`workflow_hash`) bind to.
- `workflow.valid.json` is stored canonical with a pinned known-answer
  digest; `workflow.noncanonical.json` is refused with
  `REFUSE_NONCANONICAL`.

## Pinning and version lifecycle

- A checked-in workflow only becomes eligible through **explicit config
  selection** (`[workflow.<key>] id + hash`), and the selected hash must
  match the document the daemon loads.
- Running instances pin the workflow version they started with. Upgrades
  (new workflow document/hash) affect only new runs unless an explicit
  migration is reviewed and applied — no silent behavior change on old
  runs.
- The bundled Doctrine workflow ships versioned under this family; its
  machine-enforceable rules become CLI invariants and discriminating tests
  (ADR-0003), while judgment guidance lives in docs/doctrine (future
  public home).

## Effect declaration and the non-downgrade rule

Nodes declare effects (via their typed kind and params); they cannot
declare or soften a risk class. The daemon maps declared effects through the
target table ([risk-model.md](risk-model.md), AC5). There is no field in
this document for a node to mark itself "safe": production/destructive
classification cannot be downgraded by a workflow node.

## Engine semantics (issue #6, `src/engine.rs`)

Issue #6 adds the deterministic workflow engine. The family validator above
is structural; the engine applies the fail-closed semantic checks before any
workflow is pinned or advanced, and the daemon owns every transition and
durable write. No field in this document can carry policy, capabilities,
target, or approval authority.

- **Fail-closed analysis (`analyze`)** — AC1: exactly one `start` node, all
  edges reference declared nodes (`engine.unknown_node`), the graph is
  acyclic (`engine.cyclic`), and the graph is bounded: every node reachable
  from `start` and able to reach a `terminal` (`engine.unbounded`).
- **Privilege/role rules** — AC1/AC9: node `params` may not carry authority
  keys (`caps`, `capabilities`, `risk`, `effects`, `policy`,
  `policy_hash`, `approval`, `authority`, `production`, `release` →
  `engine.privilege`); a `role` reference is a default role or an explicit
  hash-pinned custom role from config (`engine.closed_role`).
- **Pinning** — AC1: a checked-in workflow is usable only when the config
  pin (`workflow.<key>` id + hash) matches the document (`engine.pin_id_mismatch`,
  `engine.hash_mismatch`); an instance that pinned a hash refuses any
  document whose digest changed in flight (`engine.changed_in_flight`).
- **Review-gate** — AC5: an integration `merge` node must have a distinct
  `reviewer` node reachable before it (`engine.unreviewed_merge`); reviewer
  and implementer identities are distinct (`engine.role_collision`).
  Adapter-level session/worktree isolation and optional extra
  harness/provider/model policy are the review-evidence contract
  ([spec-review-evidence.md](spec-review-evidence.md), child #8).
- **Review rounds** — AC4: at most `NORMAL_REVIEW_ROUNDS` (3) normal
  review/fix rounds run automatically, then one separately authorized
  recovery round may follow; exhaustion enters the human queue
  (`engine.review_exhausted`).
- **Route grants and instances** — AC2/AC8: durable grants bind exact issue
  set/revision, repository, workflow/policy hashes, phase, scope, caps,
  expiry, and epoch (`state.issue_grant`); a material issue/acceptance edit
  makes the binding stale (`engine.grant_stale`) and the grant invalidated
  before further mutation. Instances pin their grant's hashes
  (`state.start_instance`) and pause durably with a fresh authorized resume
  digest (`state.pause_instance`/`resume_instance`; `engine.mint_resume_digest`,
  `engine.authorize_resume`); the digest is single-use.
- **Blockers and follow-ups** — AC6/AC7: a blocked item never stops
  disjoint granted work; terminal blockers stay explicit and counted
  (`engine::BlockState`). New findings may produce linked proposed/filed
  follow-ups but stay unrouted without a new grant (`engine.unrouted`).
- **Advisory output** — AC9: orchestrator/LLM output is an advisory typed
  step carrying proposals/evidence only; any field attempting to change
  policy, capabilities, target, or approval state is refused
  (`engine.privilege`).
- **One authoritative workflow per repository** — a second workflow may not
  claim the same repository scope (`engine.workflow_conflict`).

## Bundled Doctrine default (AC3/AC10)

The bundled Doctrine default workflow ships as the canonical fixture
[`schemas/fixtures/workflow/workflow.doctrine.json`](../../schemas/fixtures/workflow/workflow.doctrine.json)
(`workflow_id` `fleet-doctrine-1`, `hf-workflow/v1`), embedded in
`src/engine.rs` with its sha256 pinned (`engine::DOCTRINE_DIGEST`; the
manifest pins the same digest). It models: issue -> plan -> isolated
implementation -> exact-head review -> hosted CI -> integration merge ->
post-merge verification -> closure, with production separate (no
production/destructive authority is embedded anywhere). Running instances
pin the exact version/hash they started with; upgrades affect only new runs
unless an explicit migration plan is approved. `skills/herdr-fleet/SKILL.md`
links to this canonical source rather than duplicating it.
