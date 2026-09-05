# Spec: closed typed workflow DAGs and canonical hashing/serialization

Refs #3. Family: `hf-workflow/v1`. Fixtures:
[`workflow/`](../../schemas/fixtures/workflow/workflow.valid.json). Design commitment (locked
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
