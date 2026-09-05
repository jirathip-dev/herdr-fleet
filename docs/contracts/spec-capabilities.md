# Spec: harness and forge capability negotiation

Refs #3. Family: `hf-capability/v1`. Fixtures:
[`capability/`](../../schemas/fixtures/capability/capability.harness.valid.json). Design commitment
(ADR-0003: adapters at a small typed capability boundary; unsupported
capabilities return a typed refusal and never silently fall back to shell
guessing).

## Negotiation envelope: `hf-capability/v1`

One side of a negotiation declares what its actor can do:

```json
{"schema":"hf-capability/v1","axis":"harness","actor":"hermes",
 "capabilities":["discover","start","prompt","observe","interrupt","outcome","identity"]}
```

- `axis` closed set: `harness` | `forge`.
- `actor`: stable opaque id for the tool (harness product name, `gh`,
  adapter profile id). Actor ids are adapter metadata, never core
  branching inputs (ADR-0003).
- `capabilities`: non-empty subset of the **closed per-axis sets** below.
  An unknown capability is refused (`capability.malformed.json` adds
  `teleport`), because a client that advertises things the contract does
  not define cannot be negotiated with safely.

### Closed capability sets

| Axis | Capabilities |
| --- | --- |
| `harness` | `discover` · `start` · `prompt` · `observe` · `interrupt` · `outcome` · `identity` |
| `forge` | `read_refs` · `read_issues` · `read_checks` · `create_pr` · `comment` |

Notes: `interrupt` = interruption/cancellation; `outcome` = terminal
outcome collection; `identity` = identity/read-back of the harness actor
(ADR-0003 adapter contract list). Forge read caps (`read_refs`,
`read_issues`, `read_checks`) are what read-only status/plan use; write
caps are gated by grants and phases ([spec-plans.md](spec-plans.md)).

## Negotiation semantics

1. The daemon/CLI asks an adapter for its capability declaration before
   using it (`capabilities` RPC method).
2. A missing or refused capability means the operation is **refused with a
   typed refusal** — never emulated by shell guessing, never downgraded to
   a weaker mechanism.
3. Adapters may declare additional detail in typed params, but the
   `hf-capability/v1` document itself stays closed; anything unknown is
   refused at the boundary.
4. Core planning never branches on actor ids; it plans against declared
   capabilities, and fake adapters in tests declare the same closed sets
   (ADR-0003: fake-adapter contract tests prove core planning without any
   installed harness).

## Harness neutrality consequences

- Hermes/Claude Code/Codex are 1.0 **adapter examples** with their own
  compatibility matrices (future child #7); the domain core contains no
  product-name branches and no model/provider names.
- An unknown or unavailable harness fails clearly without degrading
  unrelated read-only operations (locked spec / ADR-0003 acceptance
  implications).
- Herdr and `gh` themselves are negotiated the same way; see
  [compatibility.md](compatibility.md).

## Fixture map

Accept: `capability.harness.valid.json` (full harness set), `capability.forge.valid.json`
(read subset). Refuse: unknown capability, unknown version.
