# Spec: daemon request/response and local JSONL event protocols

Refs #3. Families: `hf-rpc-request/v1`, `hf-rpc-response/v1`, `hf-event/v1`.
Fixtures under [`rpc/`](../../schemas/fixtures/rpc/request.status.valid.json),
[`event/`](../../schemas/fixtures/event/events.valid.jsonl). Design commitment (locked spec:
one daemon per host on a per-user Unix socket; read-only CLI operations
remain usable without it; SQLite owns state; no network control API).

## Transport

- Per-user Unix socket only (no TCP listener, no network control API in
  1.0). Default path derives from the XDG runtime dir; explicit override
  via `daemon.socket`.
- Newline-delimited JSON-RPC-style exchanges: one `hf-rpc-request/v1`
  document per request line, one `hf-rpc-response/v1` document per response
  line. Request `id` is echoed in the response (8-64 lowercase hex,
  client-generated) and is the replay handle.

## Requests: `hf-rpc-request/v1`

```json
{"schema":"hf-rpc-request/v1","id":"0123456789abcdef0123","method":"status","params":null}
```

- `method` is a closed set: `capabilities`, `doctor`, `status`, `plan`,
  `apply`, `grants.list`, `grants.revoke`, `schedules.list`,
  `schedules.create`, `schedules.pause`, `schedules.resume`,
  `schedules.delete`, `schedules.evaluate`, `state.epoch`, `backup.create`,
  `restore.begin`, `journal.tail`, `events.subscribe`
  (issues #5/#9 add the event stream and the lifecycle methods over the
  socket; the closed set above is mirrored by the Rust schema validator and
  the fixture oracle).
- `apply` **requires** `params.idempotency_key` (`ik_` format): an apply
  without a key is refused at parse time (`rpc/request.malformed.json`).
  Replaying the same request id + idempotency key returns the recorded
  response instead of re-dispatching (daemon replay table).
- Unknown methods are refused with a typed refusal (never guessed).

## Lifecycle methods (issue #9)

- `schedules.create` accepts one canonical `hf-schedule/v1` document in
  `params.schedule` (see [spec-lifecycle.md](spec-lifecycle.md)) and
  persists it as a schedule row (`m0004` columns: the canonical doc +
  `updated_at`). Like every mutation it requires `params.idempotency_key`
  and journals its intent (`mutate.schedule.create`) before the row write.
- `schedules.pause` / `schedules.resume` durably flip the row's `enabled`
  flag (`mutate.schedule.pause` / `mutate.schedule.resume`). Paused
  schedules survive daemon/service/host restarts; the evaluation path can
  never enable one, and resume additionally clears the evaluation window so
  the next boot/evaluate tick is due again (explicit human re-arm).
- `schedules.delete` removes the row after journaling its intent
  (`mutate.schedule.delete`); deleting an absent schedule is a typed
  `state.not_found`.
- `schedules.evaluate` runs one fresh evaluation tick (see
  spec-lifecycle.md semantics): each due schedule fires at most once and
  atomically persists its next window with its `read.schedule.ran` audit +
  `schedule.ran` event; refused schedules park themselves (`enabled:false`)
  with a journaled reason and need an explicit human re-arm. Evaluations
  are not idempotency-claimed (a crash leaves at most one extra fresh
  evaluation, never a backlog replay).
- Cold boot runs the same evaluation once per due schedule before the
  socket serves (recovery with Herdr absent is covered in
  spec-lifecycle.md).

## Responses: `hf-rpc-response/v1`

```json
{"schema":"hf-rpc-response/v1","id":"0123456789abcdef0123","ok":true,
 "result":{"repository":"example-org/widgets","freshness":"fresh"},"error":null}
```

- `ok:true` ⇒ `result` object, `error:null`; `ok:false` ⇒ `result:null` and
  `error` shaped like `hf-error/v1`. Mixed responses are refused
  (`rpc/response.malformed.json`).
- Errors carry stable codes; refusal codes (`refusal.*`) are typed and
  never downgraded by clients.

## Local JSONL event protocol: `hf-event/v1` (one event per line)

The daemon appends events to a local JSONL stream for read-only consumers
(the future optional Corral-style adapter boundary, ADR-0003 — dashed and
optional; nothing requires it):

```json
{"schema":"hf-event/v1","event":"state.snapshot","seq":0,"ts":"2026-09-06T00:00:00Z",
 "data":{"repository":"example-org/widgets","epoch":3}}
```

- `event` closed set: `state.snapshot` | `agent.updated` | `plan.updated` |
  `grant.updated` | `journal.appended` | `epoch.rotated` | `schedule.ran`.
- `seq` is a monotonic per-daemon sequence (replay cursor); consumers
  resume from `Last-Seq`, and a stale cursor is answered with a fresh
  snapshot (reconnect semantics are the consumer's concern; the daemon only
  guarantees append-only, seq-ordered, redacted lines).
- `ts` RFC3339 UTC; `data` is an object; every line is self-describing with
  its schema id. Unknown event kinds are refused
  (`event/events.malformed.jsonl`); a non-JSON line fails the stream
  (`event/events.badline.jsonl` refuses at parse).
- Event content is redacted by construction (shared adapter-boundary
  redaction, [spec-cli.md](spec-cli.md)); no credentials ever ride events.

## `events.subscribe` (issue #5; AC7)

A subscriber connection sends one `events.subscribe` request (params
`{"cursor": <int>}` optional — absent means "fresh snapshot first"). The
daemon answers with the ok response, then pushes `hf-event/v1` lines:

- No cursor ⇒ one `state.snapshot` line first (current state at the latest
  journal seq), then live events.
- `cursor` inside the retained window ⇒ contiguous replay of `seq > cursor`
  (no snapshot), then live events.
- `cursor` at/behind the retained window edge or in the future ⇒ a fresh
  `state.snapshot` line (gap/resnapshot semantics).
- Lines are seq-ordered and strictly increasing; per-subscriber queues are
  bounded, and a subscriber that does not drain is disconnected (bounded
  backpressure; mutations keep succeeding).
- A subscribe connection is push-only after the response: the client never
  sends again on it.

This is the **herdr-fleet daemon's** event contract, not Herdr's workspace
socket contract. Herdr 0.9.0 changed its own new subscriptions to live-only;
that does not remove this cursor-based replay/resnapshot surface. Issue #9's
schedule lifecycle writes this daemon-owned journal and never consumes
upstream Herdr `events.subscribe`, so it has no retained-history dependency
(guarded by `tests/herdr_compatibility.rs`).

## Read-only independence

`doctor`, `status`, and `plan` never require the daemon to be running
(locked spec). When the daemon is absent, read-only commands operate on
config + direct adapter read-back and report daemon absence as part of the
observation (not as a crash).

## Fixture map

Accept: `request.status.valid.json`, `request.apply.valid.json` (with
idempotency key), `response.ok.valid.json`, `response.error.valid.json`,
`events.valid.jsonl`. Refuse: apply without key, mixed ok/error response,
unknown event kind, non-JSON event line, unknown-version variants of
request/response/event.
