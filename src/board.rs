//! Issue #83: the minimal authoritative board read model.
//!
//! One bounded, deterministic, paginated read that joins the recorded
//! source identity of a work item (repository identity + external issue
//! number + the acceptance revision bound when the run started) to the
//! daemon-owned workflow runs and their durable review evidence. It is a
//! pure projection over the state store: no second database, no remote
//! request per row, no terminal-text status inference, and no write path.
//!
//! Repeated reads are stable: ordering is the `(repository, issue, run)`
//! key, the cursor is an ordering key (not a pointer), and page content
//! does not depend on insertion order, restart, or out-of-order attempts.
//!
//! Two axes stay separate by construction:
//!
//! - `stage` — derived from the durable run state plus the epoch freshness
//!   of the recorded facts (`planned`, `in_progress`, `needs_attention`,
//!   `verified`), and
//! - `verification` — derived ONLY from recorded review-evidence rows
//!   (`none`, `failed`, `passed`).
//!
//! A working or idle run therefore never implies delivery: only recorded
//! review evidence can raise `verification` to `passed`, and `stage` is
//! `verified` only while that evidence is current (live epoch, run not
//! invalidated). Facts that are not recorded stay `null` (unknown) — never
//! inferred from activity.

use crate::canonical::sha256_hex;
use crate::formats::{is_evidence_id, is_hex40, is_repository_identity, is_work_item_id};
use crate::redact::redact;
use crate::state::{BoardRunRow, EvidenceRow, State};
use crate::value::{Val, bool_, integer, null, object, string};

// Closed contract sets live with the schema family (one source of truth,
// shared with the `hf-board/v1` validator); re-exported here for consumers
// of the read model.
pub use crate::schema::{
    BOARD_EVIDENCE_REF_MAX, BOARD_NEXT_ACTIONS, BOARD_RUN_STATES, BOARD_SOURCE_KINDS, BOARD_STAGES,
    BOARD_VERIFICATIONS,
};

/// Hard cap on one board page (a larger request is refused, never clamped).
pub const BOARD_PAGE_MAX: usize = 100;
/// Page size used when the caller does not choose one.
pub const BOARD_PAGE_DEFAULT: usize = 20;

/// A board read failure. Codes are stable: `board.limit` (page cap/usage),
/// `board.cursor` (malformed cursor), `board.identity`/`board.evidence`
/// (a recorded value failed its shape rule), `board.run_state` (a recorded
/// run status outside the closed set), and the wrapped `state.*` codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardError {
    /// Stable error code.
    pub code: &'static str,
    /// Human message (never contains credentials).
    pub message: String,
}

fn board_error(code: &'static str, message: impl Into<String>) -> BoardError {
    BoardError {
        code,
        message: message.into(),
    }
}

/// One requested board page: a bounded limit and an optional cursor (the
/// ordering key of the last row of the previous page).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardQuery {
    /// Rows requested (1..=[`BOARD_PAGE_MAX`]).
    pub limit: usize,
    /// Exclusive ordering-key cursor (`repository|issue|run`).
    pub after: Option<String>,
}

impl BoardQuery {
    /// Validate one request: `limit` defaults to [`BOARD_PAGE_DEFAULT`] and
    /// must not exceed [`BOARD_PAGE_MAX`]; `after` must be a well-formed
    /// cursor when present. Both are refused typed, never silently fixed.
    pub fn new(limit: Option<i64>, after: Option<&str>) -> Result<BoardQuery, BoardError> {
        let limit = match limit {
            None => BOARD_PAGE_DEFAULT,
            Some(value) if value >= 1 && value <= BOARD_PAGE_MAX as i64 => value as usize,
            Some(value) => {
                return Err(board_error(
                    "board.limit",
                    format!(
                        "page limit {value} outside 1..={BOARD_PAGE_MAX}; the page cap is never widened"
                    ),
                ));
            }
        };
        let after = match after {
            None => None,
            Some(raw) => {
                parse_cursor(raw)?;
                Some(raw.to_string())
            }
        };
        Ok(BoardQuery { limit, after })
    }
}

/// One board row: a work item joined with one daemon-owned run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardRow {
    /// Stable local work-item id (`wi_` + 16 hex) — `None` when the run's
    /// recorded source bindings are missing (completeness `partial`).
    pub work_item: Option<String>,
    /// Source kind (`github`).
    pub source_kind: &'static str,
    /// Source repository identity (`owner/name`; empty on legacy rows).
    pub repository: String,
    /// Source work-item number (0 on legacy rows).
    pub issue: i64,
    /// Acceptance revision bound when the run started ('' on legacy rows).
    pub revision: String,
    /// `fresh` when the run's recorded facts belong to the live state
    /// epoch; `stale` after an epoch rotation (recorded evidence is no
    /// longer current for a merge).
    pub source_freshness: &'static str,
    /// `complete` when every source binding is recorded; `partial` when the
    /// run predates its bindings (never silently dropped, never fabricated).
    pub source_completeness: &'static str,
    /// The daemon-owned run identity (instance id).
    pub run: String,
    /// The exact recorded run state (`instances.status`).
    pub run_state: &'static str,
    /// Operator-facing stage derived from `run_state`, `verification`, and
    /// `source_freshness`.
    pub stage: &'static str,
    /// Verification derived ONLY from recorded review evidence.
    pub verification: &'static str,
    /// Recorded owner of the run. No durable record names one in this
    /// slice, so this stays `None` (unknown) — never fabricated.
    pub owner: Option<String>,
    /// Recorded reason text. No durable per-run reason text exists in this
    /// slice, so this stays `None` (unknown) — never inferred.
    pub reason: Option<String>,
    /// Closed next-action pointer for the states that dictate one.
    pub next_action: Option<&'static str>,
    /// Whether an explicit human decision is required to proceed.
    pub human_gate: bool,
    /// Last recorded achieved node ('' when none was recorded).
    pub milestone: Option<String>,
    /// Evidence references, newest first (at most
    /// [`BOARD_EVIDENCE_REF_MAX`]).
    pub evidence: Vec<String>,
    /// Total recorded evidence rows (overflow over `evidence`).
    pub evidence_total: i64,
    /// Recorded time of the newest evidence row.
    pub evidence_at: Option<String>,
    /// Reviewer identity of the newest evidence row (redacted), if any.
    pub reviewer: Option<String>,
    /// Last recorded run progress time (`instances.updated_at`).
    pub progress_at: Option<String>,
    /// Fresh query time of this read.
    pub observed_at: String,
}

/// One rendered board page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardPage {
    /// Fresh query time (one clock sample per page).
    pub observed_at: String,
    /// State epoch the read was taken under.
    pub epoch: i64,
    /// Highest journal seq at read time.
    pub journal_seq: i64,
    /// The page rows in deterministic order.
    pub rows: Vec<BoardRow>,
    /// Cursor for the next page; `None` exactly when no further row exists.
    pub next_cursor: Option<String>,
    /// Whether more rows exist beyond this bounded page.
    pub truncated: bool,
}

/// The stable local identity of one source work item: `wi_` + the first 16
/// hex of sha256 over `hf-work-item/v1|<repository>|<issue>`. Deterministic
/// and collision-resistant: two repositories or issue numbers cannot share
/// an id, and repeated reads (and restarts) agree.
pub fn work_item_id(repository: &str, issue: i64) -> String {
    let seed = format!("hf-work-item/v1|{repository}|{issue}");
    format!("wi_{}", &sha256_hex(seed.as_bytes())[..16])
}

/// Read one bounded, deterministic board page from the daemon state store.
///
/// The read issues at most three bounded queries (one page of runs, one
/// evidence batch for the page, one summary) and never spawns a process or
/// touches the network: the external GitHub spec is NOT re-observed per row
/// (its observation time stays `null` and never fabricates freshness).
pub fn read_board(state: &State, query: &BoardQuery) -> Result<BoardPage, BoardError> {
    let observed_at = crate::time::rfc3339_now();
    let (epoch, journal_seq) = {
        let (epoch, journal_seq, _event_seq, _schema_version) =
            state.summary().map_err(|err| BoardError {
                code: err.code,
                message: err.message,
            })?;
        (epoch, journal_seq)
    };
    // One extra row proves whether another page exists without a count scan.
    let fetch = query.limit.saturating_add(1);
    let after_bounds = match &query.after {
        Some(raw) => Some(parse_cursor(raw)?),
        None => None,
    };
    let fetched = state
        .board_page_rows(
            after_bounds
                .as_ref()
                .map(|(repository, issue, run)| (repository.as_str(), *issue, run.as_str())),
            fetch,
            BOARD_EVIDENCE_REF_MAX,
        )
        .map_err(|err| BoardError {
            code: err.code,
            message: err.message,
        })?;

    let truncated = fetched.len() > query.limit;
    let mut rows = Vec::with_capacity(fetched.len().min(query.limit));
    for joined in fetched.into_iter().take(query.limit) {
        rows.push(render_row(joined, epoch, &observed_at)?);
    }
    let next_cursor = if truncated {
        rows.last()
            .map(|row| format!("{}|{}|{}", row.repository, row.issue, row.run))
    } else {
        None
    };

    Ok(BoardPage {
        observed_at,
        epoch,
        journal_seq,
        rows,
        next_cursor,
        truncated,
    })
}

/// Parse `repository|issue|run` into its ordering key, refusing malformed
/// cursors typed (the cursor is never silently repaired).
fn parse_cursor(raw: &str) -> Result<(String, i64, String), BoardError> {
    let parts: Vec<&str> = raw.split('|').collect();
    let invalid = |why: &str| board_error("board.cursor", format!("cursor {why}"));
    if parts.len() != 3 {
        return Err(invalid("must have exactly the form repository|issue|run"));
    }
    let (repository, issue, run) = (parts[0], parts[1], parts[2]);
    if !repository.is_empty() && !is_repository_identity(repository) {
        return Err(invalid("repository part must be empty or owner/name"));
    }
    let issue: i64 = issue
        .parse()
        .map_err(|_| invalid("issue part must be a non-negative integer"))?;
    if issue < 0 {
        return Err(invalid("issue part must be a non-negative integer"));
    }
    if run.is_empty() || run.len() > 64 || !run.chars().all(is_run_char) {
        return Err(invalid("run part must be 1..=64 identifier characters"));
    }
    Ok((repository.to_string(), issue, run.to_string()))
}

/// Run-id character set: `[A-Za-z0-9._-]` (daemon slugs, and legacy ids the
/// durable state may still carry).
pub(crate) fn is_run_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Derive one rendered row from the joined read, applying the same closed
/// mappings the schema family validator enforces.
fn render_row(joined: BoardRunRow, epoch: i64, observed_at: &str) -> Result<BoardRow, BoardError> {
    let run = &joined.run;
    let repository = run.repository.clone();
    let issue = run.issue_number;
    let revision = run.issue_revision.clone();
    let run_id = run.instance_id.clone();

    let source_bound = is_repository_identity(&repository)
        && issue >= 1
        && is_hex40(&revision)
        && !run_id.is_empty();
    let completeness = if source_bound { "complete" } else { "partial" };
    let work_item = if source_bound {
        Some(work_item_id(&repository, issue))
    } else {
        None
    };
    if let Some(id) = &work_item
        && !is_work_item_id(id)
    {
        return Err(board_error(
            "board.identity",
            "derived work-item identity failed its own shape rule",
        ));
    }
    for evidence_id in &joined.evidence_ids {
        if !is_evidence_id(evidence_id) {
            return Err(board_error(
                "board.evidence",
                "recorded evidence id failed its shape rule",
            ));
        }
    }
    let freshness = if run.state_epoch == epoch {
        "fresh"
    } else {
        "stale"
    };
    let run_state = BOARD_RUN_STATES
        .iter()
        .copied()
        .find(|state| *state == run.status)
        .ok_or_else(|| {
            board_error(
                "board.run_state",
                format!(
                    "recorded run state {:?} is outside the closed set; refusing to guess",
                    run.status
                ),
            )
        })?;

    let (verification, evidence_at, reviewer) = match &joined.evidence_newest {
        None => ("none", None, None),
        Some(evidence) => (
            verification_of(evidence),
            non_empty(evidence.created_at.clone()),
            // Recorded text is redacted at this boundary (the same
            // conservative pass the adapter boundary uses) before it can
            // become part of a canonical record.
            non_empty(redact(&evidence.reviewer)),
        ),
    };
    let stage = stage_of(run_state, verification, freshness);
    let next_action = match run_state {
        "paused" => Some("resume"),
        "human_queue" => Some("human_decision"),
        _ => None,
    };
    let human_gate = matches!(run_state, "paused" | "human_queue");

    Ok(BoardRow {
        work_item,
        source_kind: "github",
        repository,
        issue,
        revision,
        source_freshness: freshness,
        source_completeness: completeness,
        run: run_id,
        run_state,
        stage,
        verification,
        owner: None,
        reason: None,
        next_action,
        human_gate,
        milestone: non_empty(run.current_node.clone()),
        evidence: joined.evidence_ids,
        evidence_total: joined.evidence_total,
        evidence_at,
        reviewer,
        progress_at: non_empty(run.updated_at.clone()),
        observed_at: observed_at.to_string(),
    })
}

/// The delivery separation rule: `verified` requires recorded review
/// evidence that is still current (live epoch, run not invalidated). A run
/// that merely reports `done`, is busy, idle, paused, or invalidated never
/// reaches it without that evidence.
fn stage_of(run_state: &str, verification: &str, freshness: &str) -> &'static str {
    if verification == "passed" && freshness == "fresh" && run_state != "invalidated" {
        return "verified";
    }
    match run_state {
        "new" => "planned",
        "running" => "in_progress",
        _ => "needs_attention",
    }
}

/// Verification is derived from the newest recorded evidence row only (the
/// same row the merge gate reads): `passed` requires a `pass` verdict AND
/// every recorded check `passed`; any other evidence is `failed`; no
/// evidence is `none`.
fn verification_of(evidence: &EvidenceRow) -> &'static str {
    if evidence.verdict != "pass" {
        return "failed";
    }
    let checks = Val::parse_json(&evidence.checks).ok();
    let all_passed = match checks.as_ref() {
        Some(Val::Arr(items)) if !items.is_empty() => items
            .iter()
            .all(|item| matches!(item.get("status"), Some(Val::Str(status)) if status == "passed")),
        _ => false,
    };
    if all_passed { "passed" } else { "failed" }
}

/// `None` for empty recorded text (an empty record is not a value).
fn non_empty(text: String) -> Option<String> {
    if text.is_empty() { None } else { Some(text) }
}

/// Render one optional recorded-text field.
fn option_str(value: Option<&str>) -> Val {
    match value {
        Some(text) => string(text),
        None => null(),
    }
}

impl BoardPage {
    /// Render the page as a canonical `hf-board/v1` document.
    pub fn to_doc(&self) -> Val {
        object(vec![
            ("schema", string("hf-board/v1")),
            ("observed_at", string(&self.observed_at)),
            (
                "state",
                object(vec![
                    ("epoch", integer(self.epoch)),
                    ("journal_seq", integer(self.journal_seq)),
                ]),
            ),
            (
                "rows",
                Val::Arr(self.rows.iter().map(BoardRow::to_doc).collect()),
            ),
            (
                "next_cursor",
                match &self.next_cursor {
                    Some(cursor) => string(cursor),
                    None => null(),
                },
            ),
            ("truncated", bool_(self.truncated)),
        ])
    }
}

impl BoardRow {
    /// Render one row as the `hf-board/v1` row object.
    pub fn to_doc(&self) -> Val {
        object(vec![
            (
                "work_item",
                match &self.work_item {
                    Some(id) => string(id),
                    None => null(),
                },
            ),
            (
                "source",
                object(vec![
                    ("kind", string(self.source_kind)),
                    ("repository", string(&self.repository)),
                    ("issue", integer(self.issue)),
                    ("revision", string(&self.revision)),
                    ("freshness", string(self.source_freshness)),
                    ("completeness", string(self.source_completeness)),
                    // The GitHub spec is not re-observed by a board read:
                    // its observation time stays unknown, never fabricated.
                    ("observed_at", null()),
                ]),
            ),
            ("run", string(&self.run)),
            ("run_state", string(self.run_state)),
            ("stage", string(self.stage)),
            ("verification", string(self.verification)),
            ("owner", option_str(self.owner.as_deref())),
            ("reason", option_str(self.reason.as_deref())),
            (
                "next_action",
                match self.next_action {
                    Some(action) => string(action),
                    None => null(),
                },
            ),
            ("human_gate", bool_(self.human_gate)),
            (
                "milestone",
                match &self.milestone {
                    Some(node) => string(node),
                    None => null(),
                },
            ),
            (
                "evidence",
                Val::Arr(self.evidence.iter().map(|id| string(id)).collect()),
            ),
            ("evidence_total", integer(self.evidence_total)),
            (
                "evidence_at",
                match &self.evidence_at {
                    Some(at) => string(at),
                    None => null(),
                },
            ),
            ("reviewer", option_str(self.reviewer.as_deref())),
            (
                "progress_at",
                match &self.progress_at {
                    Some(at) => string(at),
                    None => null(),
                },
            ),
            ("observed_at", string(&self.observed_at)),
        ])
    }
}
