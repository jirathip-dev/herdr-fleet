# Spec: board read model — bounded paginated run rows (issue #83)

Refs #83 (minimal authoritative read model; the board contract the
operator surface renders). Family: `hf-board/v1` (fixtures under
[`board/board.valid.json`](../../schemas/fixtures/board/board.valid.json)).
Rust: `src/board.rs` (read model), `src/state.rs`
(`State::board_page_rows`, the bounded join), `src/schema.rs`
(`Family::Board`, closed sets, page validator). Tests:
`tests/board_read_model.rs`; fixture probes in `src/schema.rs` and
`scripts/check-contract-fixtures.py`.

The read model joins the recorded source identity of a work item to the
daemon-owned workflow runs and their durable review evidence. It is a pure
projection over the state store: no second database, no remote request per
rendered row, no terminal-text status inference, and no write path.

## 1. The page document

One read returns one bounded page:

```json
{"schema":"hf-board/v1","observed_at":"<RFC3339 Z>",
 "state":{"epoch":3,"journal_seq":9},
 "rows":[{"work_item":"wi_<16hex>","source":{...},"run":"run-widgets-0002",
          "run_state":"running","stage":"in_progress","verification":"none",
          "owner":null,"reason":null,"next_action":null,"human_gate":false,
          "milestone":"implementer","evidence":[],"evidence_total":0,
          "evidence_at":null,"reviewer":null,
          "progress_at":"2026-09-06T00:00:02Z","observed_at":"<RFC3339 Z>"}],
 "next_cursor":"example-org/widgets|12|run-widgets-0002","truncated":true}
```

`observed_at` is the fresh query time (one clock sample per page).
`state.epoch`/`state.journal_seq` name the state generation the read was
taken under.

Closed sets (unknown values are refused, never guessed):

| Field | Closed set |
| --- | --- |
| `source.kind` | `github` |
| `source.freshness` | `fresh`, `stale` |
| `source.completeness` | `complete`, `partial` |
| `run_state` | `new`, `running`, `paused`, `human_queue`, `blocked`, `done`, `invalidated` |
| `stage` | `planned`, `in_progress`, `needs_attention`, `verified` |
| `verification` | `none`, `failed`, `passed` |
| `next_action` | `null` (not recorded), `resume`, `human_decision` |

`owner` and `reason` are recorded-text fields. This slice has no durable
record naming a run owner or a per-run reason text, so both render `null`
(unknown) — they are never inferred from activity or prose. `reviewer` is
the newest evidence row's recorded reviewer identity, redacted at the read
boundary; `milestone` is the last achieved workflow node
(`instances.current_node`), and `progress_at` is the run's last recorded
update. A recorded-text field carrying an unredacted secret-shaped run is
refused by the family validator: redaction is the boundary, not a display
concern.

## 2. Identity (AC1)

- **Work item**: `wi_` + the first 16 hex of `sha256` over
  `hf-work-item/v1|<repository>|<issue>`. Deterministic, and two
  repositories or issue numbers cannot collide; repeated reads (including
  restarts) agree.
- **Run**: the daemon-owned instance id (`instances.instance_id`) — the
  durable run identity. Multiple attempts under one issue are multiple runs
  and therefore multiple rows: rows are never joined by issue, title, or
  similarity.
- **Source**: repository identity + external issue number + the acceptance
  revision bound when the run started. Rows whose recorded bindings are
  missing (legacy rows that predate the m0002 bindings) get
  `source.completeness: "partial"` and `work_item: null` — reported, never
  silently dropped and never fabricated into an identity.

## 3. Stage, verification, and delivery (AC2)

`stage` and `verification` are separate axes and are mapped by closed
rules:

- `verification: "passed"` requires the newest recorded evidence row to
  carry verdict `pass` and every recorded check `passed`. Any other
  evidence is `failed`; no evidence is `none`.
- `stage: "verified"` if and only if `verification` is `passed`, the run's
  facts are current (`source.freshness: "fresh"`), and the run is not
  `invalidated`.

An idle, working, paused, blocked, or reported-`done` run can therefore
never be presented as verified delivery: only recorded review evidence
raises verification, and only while it is current. A `done` status without
evidence is `needs_attention` with `verification: "none"` — the
reported-done/review-verified distinction the fixture corpus pins
(`board.malformed.json` is the same page with the reported-done row
relabelled `verified`, refused by the validator).

`human_gate` is exactly `run_state` ∈ {`paused`, `human_queue`}: those are
the states whose recorded facts require an explicit human decision to
proceed. `next_action` is `resume` only for a paused run and
`human_decision` only for a human-queue run; every other state carries
`null` (not recorded) — no unsupported recovery is ever recommended.

## 4. Pagination (AC3)

- Ordering is the `(repository, issue, run)` key, strictly increasing —
  independent of insertion order, out-of-order attempts, or restart.
- The page is bounded: the read fetches at most `limit + 1` rows (hard cap
  100; a larger request is refused `board.limit`, never clamped), and
  `truncated` states whether more rows exist.
- `next_cursor` is the ordering key of the last row of the page; it exists
  exactly while `truncated` is true. The cursor is an ordering key, not a
  pointer: rows deleted between pages leave no gap, and a page can be
  re-requested after a restart with the same result.
- An empty board is a valid empty page (`rows: []`, `truncated: false`,
  `next_cursor: null`).
- `source.freshness: "stale"` marks runs whose recorded facts belong to an
  earlier state epoch (an epoch rotation — restore or security rotation —
  makes their recorded evidence no longer current for a merge). Stale rows
  are reported as stale, never silently repaired.

## 5. Read-only boundaries (AC4, AC5)

- **No remote per row**: a board read performs no remote request and spawns
  no process. The GitHub spec is not re-observed by the read: the intent
  side is the recorded provenance (`source.observed_at` stays `null`).
  Freshness/completeness describe the recorded local facts, never a remote
  scrape.
- **No write path**: the read never writes. State counts (epoch, journal
  seq, table contents) are unchanged by any number of board reads.
- **Bounded reads only**: the join issues one page query plus one evidence
  batch plus one summary; the evidence references per row are capped
  (`evidence` lists at most the newest 4 ids) and overflow stays explicit
  through `evidence_total`.
- **Redaction**: recorded text (reviewer, and any future owner/reason text)
  passes the shared conservative redaction before it can become part of a
  row; the family validator refuses a page whose recorded-text fields carry
  an unredacted secret-shaped run.

## 6. Non-goals

No cache or metrics optimization (#65–#70), no web surface, no second
database, no RPC/CLI surface change in this slice, and no status inference
from pane activity, elapsed time, or transcripts.
