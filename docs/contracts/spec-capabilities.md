# Spec: harness and forge capability negotiation

Refs #3, #7. Family: `hf-capability/v1`. Fixtures:
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

## Adapter contract (issue #7)

Refs #7. Implemented in `src/adapters.rs`; verified by fake-executable
contract tests (`tests/harness_adapters.rs`) that public CI and fork PRs
can run with **no harness credentials** (AC7).

- **Official adapters**: Hermes (`hermes`), Claude Code (`claude`), Codex
  (`codex`), and Pi (`pi`, earendil-works/pi — issue #33) — adapter
  examples per ADR-0003, with declared version ranges in
  [compatibility.md](compatibility.md) (Hermes/Claude Code/Codex measured
  2026-09-06; Pi measured 2026-09-08) and the full closed harness
  capability set above.
- **Generic adapter**: the declarative `argv` kind — validated argv arrays,
  explicit capability declarations, bare executable names resolved through
  the allowlisted PATH (the verified absolute identity is what is spawned),
  bounded time/output, cancellation, redaction, typed exits. No shell
  evaluation, no command templates, no capability inference from prose, no
  dynamic plugin SDK (AC9).
- **Operations** (closed set): `start` binds a session handle; `prompt`
  delivers untrusted text as data — a single final argv element that can
  never alter adapter argv or policy (AC5); `observe`, `interrupt`,
  `outcome`, and `identity` run the workspace session protocol (session
  observation, interruption/cancellation, terminal outcome collection,
  identity read-back) through the workspace executable. The workspace
  invocation rows are a v1 candidate contract: their real-world parity is
  [awaiting-evidence] until the human-gated clean-host smokes (AC6), and
  fakes pin the exact argv shape in tests.
- **Stable agent identity (AC3)**: an agent identity binds the Herdr
  workspace session id + a stable terminal/native-session identity + a
  generation counter. A mutable pane label is not part of the identity and
  cannot substitute for any part; binding without all three parts is refused
  (`refusal.identity.incomplete`), and an identity read-back that disagrees
  with the bound triple is refused (`refusal.stale.identity`).
- **Refusal and failure codes** (typed, `hf-error/v1` shape): `unknown.harness`
  (kind outside the closed set), `unknown.capability` (capability/operation
  not in the closed harness set or not declared), `refusal.unavailable.harness`
  (executable missing/unspawnable), `refusal.credentials` (auth failure —
  credentials stay in the harness, never in herdr-fleet, AC8),
  `refusal.malformed.output` (unparsable structured output),
  `refusal.stale.identity`, `refusal.identity.incomplete`,
  `refusal.request.malformed`, `adapter.timeout` (deadline exceeded, the
  child is killed; outcome class `ambiguous`), `adapter.process_death`
  (`ambiguous`), and `adapter.exit` (ordinary non-zero exit).
- **Boundaries**: adapters pass only the explicit environment allowlist
  (`env_allow`, spec-config.md), never read the host environment
  themselves, never store tokens, and never persist raw prompts or
  transcripts (AC8; trust model T5). Unknown or unavailable harnesses fail
  per-surface and never break independent read-only operations (AC4,
  observe.rs pattern).
- **Fake-adapter doctrine (AC1/AC7)**: the same core workflow/plan
  fixtures drive fake implementations of every adapter contract (fake
  executables declaring the same closed sets); every official adapter has
  exact-version contract tests for success, missing executable/auth,
  unsupported capability, timeout, cancellation, malformed output, stale
  identity, and process death (AC2).
- **Herdr lane-lifecycle reporting (issue #33 A2, pi adapter)**: when a pi
  profile operation runs inside a Herdr pane (`HERDR_ENV=1` +
  `HERDR_PANE_ID` in the allowlisted environment), the adapter reports the
  lane through the workspace executable's `pane report-agent` row
  (`--source custom:herdr-fleet-pi --agent pi`), per Herdr's
  custom-integration contract (herdr 0.8.2). Typed-result mapping:

  | Typed result | Herdr report |
  | --- | --- |
  | `start` succeeded | `working` (lane active) |
  | terminal `prompt` — succeeded / failed / refused / ambiguous (timeout, process death, plain exit) | `idle` |
  | terminal `prompt` with `refusal.credentials` | `blocked` + static message `harness credentials required` (a provider key decision is needed; the message never carries credential text) |

  Herdr has no `done` state, so terminal lanes report `idle`. Reporting is
  a best-effort sideband that never changes the typed op result and is a
  no-op outside Herdr (no `HERDR_ENV=1`). `herdr agent start --kind pi` is
  the substrate/orchestrator path for *interactive* pi panes (it requires
  a pane at an interactive shell prompt and is not drivable by a headless
  library adapter); headless adapter runs report through the pane rows
  above. Releasing the reporting source's authority (`herdr pane
  release-agent`, same `--source`/`--agent`) is the lane owner's
  pane-closeout step for future daemon wiring. The rows are pinned by
  fake-`herdr` contract tests in `tests/harness_adapters.rs`.

## Harness neutrality consequences

- Hermes/Claude Code/Codex/Pi are 1.0 **adapter examples** (issues #7/#33)
  with their own compatibility matrices
  ([compatibility.md](compatibility.md)); the domain core contains no
  product-name branches and no model/provider names. Pi's one-shot prompt
  row carries provider/model as opaque adapter metadata in argv (measured
  example row; never persisted, never on a wire); provider keys arrive
  only through the environment allowlist.
- An unknown or unavailable harness fails clearly without degrading
  unrelated read-only operations (issue #7 AC4; observe.rs pattern).
- Herdr and `gh` themselves are negotiated the same way; see
  [compatibility.md](compatibility.md).

## Fixture map

Accept: `capability.harness.valid.json` (full harness set), `capability.forge.valid.json`
(read subset). Refuse: unknown capability, unknown version.
