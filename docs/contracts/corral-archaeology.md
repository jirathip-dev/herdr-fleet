# Corral archaeology matrix (AC9)

Refs #3 (deliverable: Corral archaeology matrix classifying candidates as
REUSE / ADAPT / LEARN / REJECT with public commit/path/test and license
provenance) and umbrella #1 ("Corral archaeology and provenance gate"),
which names two public reference commits:

- `6dfba2249c86c3906b025b3098879ab241b691a2` — last pre-read-only
  capability implementation ("6dfba22" below).
- `cf766b343cd34c5c618df87573cc85204cf31b5c` — former bounded plugin
  engine ("cf766b3" below).

All provenance below is public material from the public Corral repository
(https://github.com/jirathip-dev/corral). Both commits carry
[`LICENSE-APACHE`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/LICENSE-APACHE)
and
[`LICENSE-MIT`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/LICENSE-MIT)
(verified identical license blobs at both SHAs), i.e. **Apache-2.0 OR MIT**
— compatible with this repository's licensing. No private repository names
or host paths appear anywhere in this matrix (AC8).

Classification rubric:

- **REUSE** — vendor/import Corral code (requires the AC9 independent
  review + provenance preservation before it lands; nothing is reused in
  this slice).
- **ADAPT** — take the artifact's shape/wire knowledge as a starting point
  and re-derive it for herdr-fleet's own contract, with re-verification.
- **LEARN** — extract the discipline/pattern into herdr-fleet's specs and
  discriminating tests; no code or wire knowledge is carried over.
- **REJECT** — the mechanism is out of 1.0 scope or contradicted by
  upstream's own trajectory; recorded as historical reference only.

Review status (AC9): **pending independent review before any REUSE/ADAPT
code lands**. This slice commits no Corral code; the rows below state what
a reviewer must verify per row.

## Candidate matrix

| # | Candidate seam (umbrella #1 wording) | Public provenance (commit · code · tests) | License | Class | Rationale and what independent review must verify |
| --- | --- | --- | --- | --- | --- |
| A1 | Typed Herdr socket RPC/event reconciliation | 6dfba22 · [`src/adapters/herdr.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/src/adapters/herdr.rs) (event-first newline-delimited JSON-RPC over the per-user Unix socket; `events.subscribe` connections become push-only; request/response work opens a fresh short-lived connection; historical replay-convergent subscribe — Herdr 0.9 consumers must instead subscribe before snapshot and never expect retained replay (issue #35); trusted freshness watchdog; `DEFAULT_SOCKET`) · tests exist under `tests/` at the same commit | Apache-2.0 OR MIT | **ADAPT** | Herdr-socket wire knowledge is the starting point for the herdr adapter spec (child #7), but herdr-fleet must re-derive and re-verify every protocol fact against live Herdr + upstream docs — Corral code is evidence, not the contract. Review must verify: protocol facts re-verified, no adapter code copied wholesale, live-socket conformance tests in #7. |
| A2 | Reconnect/backoff discipline and stream resume | 6dfba22 · [`crates/corrald-client/src/client.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/crates/corrald-client/src/client.rs) (`RetryPolicy`: max 4 attempts, 100 ms base, 2 s max backoff) · [`crates/corrald-client/src/sse.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/crates/corrald-client/src/sse.rs) (no/stale/future cursor -> snapshot; bounded backoff reconnect; drop-to-stop) | Apache-2.0 OR MIT | **LEARN** | Encoded as resume semantics in the `hf-event/v1` spec (`seq` cursor; stale cursor answered with a fresh snapshot). No code carried. Review must verify: spec text does not import Corral constants as herdr-fleet commitments. |
| A3 | Generation-safe agent↔pane mappings, stale-target retirement, cancellation safety | 6dfba22 · [`src/adapters/herdr.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/src/adapters/herdr.rs) (monotonic mapping generation; every pane/target transition advances it; late RPC results cannot retire a newer mapping; retired panes cannot resurrect through replay order; stale-agent retirement with typed `StaleAgent` drive error) | Apache-2.0 OR MIT | **LEARN** | The same hazard class is handled in herdr-fleet by state epochs + stale-observation refusal + typed refusal outcomes (spec-plans/spec-cli); no mapping code is transferable (different domain object). Review must verify: herdr-fleet's epoch/refusal rules cover late-result and resurrection hazards; Corral code read for hazard enumeration only. |
| A4 | Closed capability/payload/result enums and typed refusals | 6dfba22 · [`crates/corrald-client/src/errors.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/crates/corrald-client/src/errors.rs) (closed `DriveErrorKind`: `InFlight`, `StaleAgent`, `StaleApproval`, `Expired`, `Revoked`, `NotGranted`, `HashMismatch`, `UnknownCapability`, ...) · [`contracts/state-tokens.json`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/contracts/state-tokens.json) (closed token set: blocked/done/working/idle/unknown) · [`clients/egui/tests/conformance.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/clients/egui/tests/conformance.rs) (client↔daemon canonical byte conformance incl. `refusal_kinds_match_the_conformance_table`) | Apache-2.0 OR MIT | **LEARN** | The *discipline* — closed wire enums + refusal tables + cross-client conformance tests — is adopted by herdr-fleet's closed fixture sets and refusal fixtures (AC2) and the probe self-test's tamper discrimination. The enum values themselves are domain-specific and not copied. Review must verify: herdr-fleet's closed sets are independently derived from its own contracts. |
| A5 | Request-ID replay/idempotency and claim consumption | 6dfba22 · [`crates/corrald-client/src/client.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/crates/corrald-client/src/client.rs) (signed-drive writer with client-side replay table and idempotent retries; daemon replay table guarantees at most one dispatch; `InFlight` refusal on concurrent duplicates) · [`src/auth/registry.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/src/auth/registry.rs) (registering the same key twice is idempotent and never extends expiry; inline test `re_registration_is_idempotent_and_never_extends`) | Apache-2.0 OR MIT | **LEARN** | herdr-fleet's `ik_` idempotency keys, daemon replay, and journal-claims semantics (spec-plans/spec-state/spec-daemon) encode the same guarantees with herdr-fleet's own records. Review must verify: claim-expiry-never-extends and at-most-once wording match the spec, and no Corral code is imported. |
| A6 | Issue-linked vs issue-free worktree plans with no unsafe fallback | 6dfba22 · [`src/fleet/worktree.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/src/fleet/worktree.rs) (`FleetIdentity`, `WorktreeRequest`/`WorktreePlan`/`WorktreeOutcome`/`Handoff`, `IssueCheck`: stale/closed issue guard; any non-`OPEN` state — including the UNKNOWN sentinel — is not startable) · [`tests/worktree.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/tests/worktree.rs) (integration tests at same commit) | Apache-2.0 OR MIT | **LEARN** | The guard pattern ("issue state unknown ⇒ not startable ⇒ no unsafe fallback") informs herdr-fleet's route-grant + plan contract: grants bind exact issue/acceptance revision and stale/unknown state refuses (spec-plans AC3). Issue-free ephemeral spikes can never merge/promote/publish (locked spec). Review must verify: the no-unsafe-fallback rule is expressed through herdr-fleet's grant/plan documents, not through any copied check. |
| A7 | Bounded subprocess execution: schedule ownership, bounded concurrency, timeout, output caps, `kill_on_drop`, failure isolation | cf766b3 · [`src/plugin.rs`](https://github.com/jirathip-dev/corral/blob/cf766b343cd34c5c618df87573cc85204cf31b5c/src/plugin.rs) (configless allowlisted sidecar engine; single allowed id; commands parsed as argv arrays, never assembled from client input; env handling via allowlist-style PATH reconstruction; `kill_on_drop(true)`; 30 s command timeout; `OUTPUT_LIMIT` = 256 KiB) · 6dfba22 · [`src/tmux.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/src/tmux.rs) (`MAX_CAPTURE_BYTES` = 64 KiB bounded capture) | Apache-2.0 OR MIT | **LEARN** | The bounded-execution discipline (argv-only, env allowlist, hard timeouts, output caps, kill-on-drop, failure isolation) is normative for herdr-fleet's subprocess adapters; concrete bound values become herdr-fleet commitments only when the adapter contract tests land (child #7) — the Corral values above are reference prior art, not adopted constants. Review must verify: no bound value is copied silently; each is re-derived and tested in #7. |
| A8 | Former bounded plugin engine as a product surface | cf766b3 · [`src/plugin.rs`](https://github.com/jirathip-dev/corral/blob/cf766b343cd34c5c618df87573cc85204cf31b5c/src/plugin.rs), [`herdr-plugin.toml`](https://github.com/jirathip-dev/corral/blob/cf766b343cd34c5c618df87573cc85204cf31b5c/herdr-plugin.toml), [`tests/plugin_status_test.sh`](https://github.com/jirathip-dev/corral/blob/cf766b343cd34c5c618df87573cc85204cf31b5c/tests/plugin_status_test.sh) (offline curl-stub proof of the status endpoint/summary contract) · removed again by 6dfba22 (read-only cutover): the delta between the two named commits deletes the plugin engine and fleet cli/switch/health surfaces | Apache-2.0 OR MIT | **REJECT** | 1.0 non-goal: no dynamic plugin SDK; the native Herdr plugin is optional, late, thin, and never owns policy (locked spec). Upstream's own trajectory (engine removed before the read-only era) corroborates the rejection. Recorded as historical reference only. Review must verify: no plugin-engine structure leaks into herdr-fleet docs/specs as a future commitment. |
| A9 | One shared redaction pass before any bytes leave the machine | 6dfba22 · [`src/core/redact.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/src/core/redact.rs) (single conservative pass at the adapter boundary so every downstream serialization is redacted by construction) · [`tests/redact.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/tests/redact.rs) (test module at same commit) | Apache-2.0 OR MIT | **LEARN** | herdr-fleet's output spec states the identical boundary rule (spec-cli §6: redact at the adapter boundary before canonical records). Rule lists are re-derived from herdr-fleet's own trust model (T5). Review must verify: redaction rule coverage is herdr-fleet's own and includes no Corral-specific token set. |
| A10 | Canonical envelope byte-conformance testing (golden bytes) | 6dfba22 · [`clients/egui/tests/conformance.rs`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/clients/egui/tests/conformance.rs) (`canonical_envelope_bytes_match_the_daemon`) · [`tests/fixtures/canonical_stream_golden.json`](https://github.com/jirathip-dev/corral/blob/6dfba2249c86c3906b025b3098879ab241b691a2/tests/fixtures/canonical_stream_golden.json) (golden fixture at same commit) | Apache-2.0 OR MIT | **LEARN** | Canonical-bytes + known-answer-digest fixture testing is exactly what herdr-fleet's `hf-plan/v1`/`hf-workflow/v1` canonical rule and manifest-pinned digests implement. Review must verify: golden bytes are generated by herdr-fleet's own canonical serializer definition (registry), not imported from Corral streams. |

## What the two-commit delta shows

Comparing the public trees at cf766b3 and 6dfba22: the later commit no
longer contains the plugin engine (`src/plugin.rs`, `src/api/plugin.rs`,
`src/api/fleets.rs`, `herdr-plugin.toml`, `tests/plugin_status_test.sh`,
`clients/egui/src/ui/plugin.rs`) or the fleet cli/switch/health surfaces.
Upstream's read-only trajectory therefore corroborates two herdr-fleet
decisions: the read contract is separate from control machinery, and
extension/plugin surfaces were deliberately retired rather than evolved.

## AC9 gate

This matrix is a contract artifact only. **Before any REUSE/ADAPT code
lands** in a future slice: (1) an independent reviewer re-verifies each row
above against the cited public commits; (2) license compatibility and
attribution are confirmed for anything imported; (3) the slice records the
review outcome. Rows A1 and A7 carry the highest review burden (wire
knowledge and bound values respectively).
