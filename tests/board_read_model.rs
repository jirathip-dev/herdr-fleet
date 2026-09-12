//! Issue #83 acceptance tests: the bounded, deterministic board read model
//! over the daemon-owned state store.
//!
//! Every test opens a real `canter::state::State` in a per-test temp dir and
//! seeds it through the public state API (grants, instances, evidence) —
//! synthetic identities only, nothing touches the host state, a real
//! session, the network, or a subprocess. Rendered pages are run through the
//! `hf-board/v1` family validator, so the read model and the machine-checked
//! contract are pinned to each other.

use std::path::PathBuf;

use canter::board::{
    BOARD_EVIDENCE_REF_MAX, BOARD_PAGE_MAX, BoardError, BoardQuery, read_board, work_item_id,
};
use canter::state::{Retention, State};
use canter::value::{Val, string};

/// A per-test fixture directory with its own state database.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("hf-board-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        Fixture { dir }
    }

    fn db(&self) -> PathBuf {
        self.dir.join("state.db")
    }

    fn open(&self) -> State {
        State::open(&self.db(), Retention::default()).expect("open state")
    }

    /// Direct SQLite access for the two seeds the public API cannot produce:
    /// a legacy row that predates the m0002 bindings and a `done` status
    /// (declared in the state vocabulary, written only by a later slice).
    fn raw(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.db()).expect("raw connection")
    }
}

const REPO_A: &str = "example-org/widgets";
const REPO_B: &str = "example-org/gadgets";
const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
const WORKFLOW_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const POLICY_HASH: &str = "feedface01234567feedface01234567feedface01234567feedface01234567";

/// One synthetic hf-grant/v1 document for `state_epoch`.
fn grant_doc(grant_id: &str, repository: &str, issue: i64, state_epoch: i64) -> Val {
    Val::parse_json(&format!(
        r#"{{"schema":"hf-grant/v1","grant_id":"{grant_id}","repository":"{repository}",
            "issue":{{"number":{issue},"revision":"{REVISION}"}},
            "workflow_hash":"{WORKFLOW_HASH}","policy_hash":"{POLICY_HASH}",
            "phase":"review","scope":"worktrees/issues/{issue}",
            "caps":["read","worktree","spawn","review","merge"],
            "expires_at":"2999-01-01T00:00:00Z","state_epoch":{state_epoch},
            "created_at":"2026-09-06T00:00:00Z"}}"#
    ))
    .expect("grant document parses")
}

/// Seed one grant and one run bound to it; returns the run id.
fn seed_run(state: &State, repository: &str, issue: i64, run: &str) -> String {
    let epoch = state.summary().expect("summary").0;
    let grant_id = format!("gr_{:016x}", seed_counter());
    state
        .issue_grant(&grant_doc(&grant_id, repository, issue, epoch))
        .expect("issue grant");
    state
        .start_instance(run, &grant_id, "fleet-doctrine-1", "2026-09-06T00:00:00Z")
        .expect("start instance");
    run.to_string()
}

/// Monotone per-process counter: grant ids must be unique within one epoch.
fn seed_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Record one passing review-evidence row (all checks passed).
fn record_pass(state: &State, run: &str, reviewer: &str, feature_head: &str) {
    let checks =
        Val::parse_json(r#"[{"name":"exact-head-review","status":"passed"}]"#).expect("checks");
    state
        .record_evidence(
            run,
            REPO_A,
            feature_head,
            "89abcdef0123456789abcdef0123456789abcdef",
            WORKFLOW_HASH,
            POLICY_HASH,
            "pass",
            reviewer,
            &checks,
        )
        .expect("record pass evidence");
}

/// Record one failing review-evidence row.
fn record_fail(state: &State, run: &str) {
    let checks =
        Val::parse_json(r#"[{"name":"exact-head-review","status":"failed"}]"#).expect("checks");
    state
        .record_evidence(
            run,
            REPO_A,
            "1111111111111111111111111111111111111111",
            "89abcdef0123456789abcdef0123456789abcdef",
            WORKFLOW_HASH,
            POLICY_HASH,
            "fail",
            "reviewer-example",
            &checks,
        )
        .expect("record fail evidence");
}

fn page(state: &State, limit: i64, after: Option<&str>) -> canter::board::BoardPage {
    let query = BoardQuery::new(Some(limit), after).expect("query");
    read_board(state, &query).expect("read board")
}

/// Every rendered page must satisfy the machine-checked `hf-board/v1` rules.
fn assert_page_valid(page: &canter::board::BoardPage) {
    let bytes = canter::canonical::canonical_bytes(&page.to_doc());
    let verdict = canter::schema::validate_bytes(canter::schema::Family::Board, &bytes);
    assert!(
        verdict.is_accepted(),
        "rendered page refused by hf-board/v1: {}",
        verdict.message()
    );
}

fn row_for<'a>(page: &'a canter::board::BoardPage, run: &str) -> &'a canter::board::BoardRow {
    page.rows
        .iter()
        .find(|row| row.run == run)
        .unwrap_or_else(|| panic!("no row for {run}"))
}

#[test]
fn board_rows_keep_runs_separate_and_derive_stable_work_item_identity() {
    let fixture = Fixture::new("identity");
    let state = fixture.open();
    seed_run(&state, REPO_A, 7, "run-a-0001");
    seed_run(&state, REPO_A, 7, "run-a-0002");
    seed_run(&state, REPO_B, 7, "run-b-0003");
    seed_run(&state, REPO_A, 8, "run-a-0004");

    let page = page(&state, 20, None);
    assert_eq!(page.rows.len(), 4, "each run stays its own row");
    assert_page_valid(&page);

    let a1 = row_for(&page, "run-a-0001");
    let a2 = row_for(&page, "run-a-0002");
    let b3 = row_for(&page, "run-b-0003");
    let a4 = row_for(&page, "run-a-0004");

    // Two attempts under one issue share the stable work-item identity and
    // still render as two rows.
    assert_eq!(a1.work_item, a2.work_item);
    assert_ne!(a1.run, a2.run);
    assert_eq!(a1.work_item.as_deref(), Some(&*work_item_id(REPO_A, 7)));
    assert_eq!(
        a1.work_item.as_deref(),
        Some(&*work_item_id(REPO_A, 7)),
        "deterministic"
    );
    // Repository and issue differences cannot collide.
    assert_ne!(a1.work_item, b3.work_item);
    assert_ne!(
        work_item_id(REPO_B, 7),
        work_item_id(REPO_A, 7),
        "repository identity is part of the work-item identity"
    );
    assert_ne!(a1.work_item, a4.work_item);
    // Every row carries its own recorded source identity.
    assert_eq!((a1.repository.as_str(), a1.issue), (REPO_A, 7));
    assert_eq!((b3.repository.as_str(), b3.issue), (REPO_B, 7));
    assert_eq!(b3.work_item.as_deref(), Some(&*work_item_id(REPO_B, 7)));
}

#[test]
fn board_never_implies_delivery_from_activity_or_reported_status() {
    let fixture = Fixture::new("delivery");
    let state = fixture.open();
    seed_run(&state, REPO_A, 12, "run-new-0001");
    seed_run(&state, REPO_A, 12, "run-working-0002");
    state
        .advance_instance(
            "run-working-0002",
            "implementer",
            0,
            0,
            false,
            0,
            "2026-09-06T00:00:01Z",
        )
        .expect("advance");
    seed_run(&state, REPO_A, 12, "run-paused-0003");
    state
        .pause_instance("run-paused-0003", &"a".repeat(64), "2026-09-06T00:00:02Z")
        .expect("pause");
    seed_run(&state, REPO_A, 12, "run-blocked-0004");
    state
        .advance_instance(
            "run-blocked-0004",
            "implementer",
            0,
            0,
            false,
            2,
            "2026-09-06T00:00:03Z",
        )
        .expect("advance blocked");
    seed_run(&state, REPO_A, 12, "run-human-0005");
    state
        .advance_instance(
            "run-human-0005",
            "reviewer",
            3,
            1,
            true,
            0,
            "2026-09-06T00:00:04Z",
        )
        .expect("advance human queue");
    // A reported-done run: the state vocabulary declares `done`, and no
    // recorded evidence backs it. No public API writes it yet.
    seed_run(&state, REPO_A, 12, "run-done-0006");
    let conn = fixture.raw();
    conn.execute(
        "UPDATE instances SET status = 'done', current_node = 'merge', updated_at = ?2
          WHERE instance_id = ?1",
        rusqlite::params!["run-done-0006", "2026-09-06T00:00:05Z"],
    )
    .expect("seed reported done");
    drop(conn);
    // Evidence-backed and evidence-failed runs.
    seed_run(&state, REPO_A, 12, "run-verified-0007");
    state
        .advance_instance(
            "run-verified-0007",
            "reviewer",
            0,
            0,
            false,
            0,
            "2026-09-06T00:00:06Z",
        )
        .expect("advance verified");
    record_pass(
        &state,
        "run-verified-0007",
        "reviewer-example",
        "2222222222222222222222222222222222222222",
    );
    seed_run(&state, REPO_A, 12, "run-failed-0008");
    state
        .advance_instance(
            "run-failed-0008",
            "reviewer",
            0,
            0,
            false,
            0,
            "2026-09-06T00:00:07Z",
        )
        .expect("advance failed");
    record_fail(&state, "run-failed-0008");

    let page = page(&state, 20, None);
    assert_page_valid(&page);
    assert_eq!(page.rows.len(), 8);

    let expected = [
        ("run-new-0001", "new", "planned", "none"),
        ("run-working-0002", "running", "in_progress", "none"),
        ("run-paused-0003", "paused", "needs_attention", "none"),
        ("run-blocked-0004", "blocked", "needs_attention", "none"),
        ("run-human-0005", "human_queue", "needs_attention", "none"),
        ("run-done-0006", "done", "needs_attention", "none"),
        ("run-verified-0007", "running", "verified", "passed"),
        ("run-failed-0008", "running", "in_progress", "failed"),
    ];
    for (run, run_state, stage, verification) in expected {
        let row = row_for(&page, run);
        assert_eq!(row.run_state, run_state, "{run} run_state");
        assert_eq!(row.stage, stage, "{run} stage");
        assert_eq!(row.verification, verification, "{run} verification");
        // Idle/working never implies delivery: only the evidence-backed run
        // is verified, and every other row states `none`/`failed`.
        if stage != "verified" {
            assert_ne!(
                row.verification, "passed",
                "{run} must not claim verification"
            );
            assert!(
                row.evidence.is_empty() || row.verification == "failed",
                "{run} must not carry unbacked evidence references"
            );
        }
    }
    // Reported done is NOT delivery: no evidence, needs attention.
    let done = row_for(&page, "run-done-0006");
    assert_eq!(done.run_state, "done");
    assert_eq!(done.stage, "needs_attention");
    assert_eq!(done.verification, "none");
    assert!(done.evidence.is_empty());
    assert_eq!(done.evidence_total, 0);
    assert!(done.evidence_at.is_none());
    // A human decision is the recorded next step for the gated rows.
    let paused = row_for(&page, "run-paused-0003");
    assert_eq!(paused.next_action, Some("resume"));
    assert!(paused.human_gate);
    let human = row_for(&page, "run-human-0005");
    assert_eq!(human.next_action, Some("human_decision"));
    assert!(human.human_gate);
    // Activity alone records no human gate.
    assert!(!row_for(&page, "run-working-0002").human_gate);
    assert!(row_for(&page, "run-working-0002").next_action.is_none());
    // The milestone is exactly the recorded achieved node.
    assert_eq!(
        row_for(&page, "run-working-0002").milestone.as_deref(),
        Some("implementer")
    );
    assert_eq!(done.milestone.as_deref(), Some("merge"));
}

#[test]
fn board_pagination_is_deterministic_bounded_and_restart_stable() {
    let fixture = Fixture::new("pagination");
    let state = fixture.open();
    // Seeded in REVERSE key order: page order must follow the ordering key,
    // never insertion order (out-of-order attempts and restarts included).
    for (index, run) in [
        "run-0007", "run-0006", "run-0005", "run-0004", "run-0003", "run-0002", "run-0001",
    ]
    .iter()
    .enumerate()
    {
        let repository = if index % 2 == 0 { REPO_A } else { REPO_B };
        seed_run(&state, repository, 5, run);
    }

    let mut seen: Vec<(String, i64, String)> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = page(&state, 3, cursor.as_deref());
        assert_page_valid(&page);
        assert!(page.rows.len() <= 3, "bounded page");
        if !page.truncated {
            assert!(page.next_cursor.is_none(), "last page offers no cursor");
        }
        for row in &page.rows {
            seen.push((row.repository.clone(), row.issue, row.run.clone()));
        }
        pages += 1;
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
        assert!(pages < 10, "pagination must terminate");
    }
    assert_eq!(pages, 3, "7 rows / limit 3 = 3 bounded pages");
    assert_eq!(seen.len(), 7, "every row appears exactly once");
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(
        seen, sorted,
        "rows arrive in deterministic (repository, issue, run) order"
    );
    assert!(
        seen.windows(2).all(|pair| pair[0] < pair[1]),
        "the ordering key is strictly increasing across pages"
    );
    let unique: std::collections::BTreeSet<&(String, i64, String)> = seen.iter().collect();
    assert_eq!(unique.len(), 7, "pages never overlap");

    // Restart stability: reopening the same state returns the same first
    // page (freshness timestamps differ; facts do not).
    let first = page(&state, 3, None);
    drop(state);
    let reopened = fixture.open();
    let again = page(&reopened, 3, None);
    assert_eq!(first.rows.len(), again.rows.len());
    for (before, after) in first.rows.iter().zip(again.rows.iter()) {
        assert_eq!(before.run, after.run);
        assert_eq!(before.stage, after.stage);
        assert_eq!(before.verification, after.verification);
        assert_eq!(before.work_item, after.work_item);
    }

    // The cap is a hard bound: a larger request is refused, never clamped.
    let refused = BoardQuery::new(Some(BOARD_PAGE_MAX as i64 + 1), None);
    assert!(matches!(
        refused,
        Err(BoardError {
            code: "board.limit",
            ..
        })
    ));
    assert!(BoardQuery::new(Some(0), None).is_err());
    assert!(BoardQuery::new(None, Some("not-a-cursor")).is_err());
    assert!(BoardQuery::new(None, Some("example-org/widgets|7|run-a-0001")).is_ok());
}

#[test]
fn board_empty_stale_and_missing_source_cases_are_covered() {
    let fixture = Fixture::new("edges");
    let state = fixture.open();

    // Empty board: a valid empty page.
    let empty = page(&state, 20, None);
    assert!(empty.rows.is_empty());
    assert!(!empty.truncated);
    assert!(empty.next_cursor.is_none());
    assert_page_valid(&empty);

    // A full run, then an epoch rotation: its recorded facts go stale and
    // stale facts can never present the run as verified.
    seed_run(&state, REPO_A, 9, "run-stale-0001");
    state
        .advance_instance(
            "run-stale-0001",
            "reviewer",
            0,
            0,
            false,
            0,
            "2026-09-06T00:00:01Z",
        )
        .expect("advance");
    record_pass(
        &state,
        "run-stale-0001",
        "reviewer-example",
        "3333333333333333333333333333333333333333",
    );
    let before = page(&state, 20, None);
    assert_eq!(row_for(&before, "run-stale-0001").verification, "passed");
    assert_eq!(row_for(&before, "run-stale-0001").stage, "verified");
    state.rotate_epoch("security_rotation").expect("rotate");
    let after = page(&state, 20, None);
    assert_page_valid(&after);
    let stale = row_for(&after, "run-stale-0001");
    assert_eq!(stale.source_freshness, "stale");
    assert_eq!(
        stale.verification, "passed",
        "the recorded evidence fact stays visible"
    );
    assert_ne!(stale.stage, "verified", "stale facts never verify");
    assert_eq!(
        stale.stage, "in_progress",
        "the run is still working; only its verification claim is stale"
    );

    // A legacy row that predates the bindings: reported partial, never
    // dropped and never given a fabricated identity.
    let conn = fixture.raw();
    conn.execute(
        "INSERT INTO instances (instance_id, state_epoch, status, created_at)
         VALUES ('legacy-run-0002', ?1, 'new', '2026-09-06T00:00:00Z')",
        rusqlite::params![after.epoch],
    )
    .expect("seed legacy row");
    drop(conn);
    let page = page(&state, 20, None);
    assert_page_valid(&page);
    let legacy = row_for(&page, "legacy-run-0002");
    assert!(legacy.work_item.is_none(), "no fabricated identity");
    assert_eq!(legacy.source_completeness, "partial");
    assert_eq!(legacy.stage, "planned");
    // The fully-bound stale run is still complete and visible.
    assert_eq!(
        row_for(&page, "run-stale-0001").source_completeness,
        "complete"
    );
}

#[test]
fn board_rows_expose_the_required_contract_fields() {
    let fixture = Fixture::new("fields");
    let state = fixture.open();
    seed_run(&state, REPO_A, 3, "run-fields-0001");
    let page = page(&state, 20, None);
    let doc = page.to_doc();
    let row = doc
        .get("rows")
        .and_then(Val::as_array)
        .and_then(|rows| rows.first())
        .expect("one row");

    // AC2: stage, owner, reason, next action, human gate, evidence
    // references, and source freshness/completeness are all present (null
    // means not recorded — never fabricated).
    for key in [
        "work_item",
        "source",
        "run",
        "run_state",
        "stage",
        "verification",
        "owner",
        "reason",
        "next_action",
        "human_gate",
        "milestone",
        "evidence",
        "evidence_total",
        "evidence_at",
        "reviewer",
        "progress_at",
        "observed_at",
    ] {
        assert!(row.get(key).is_some(), "row must expose {key}");
    }
    assert!(
        matches!(row.get("owner"), Some(Val::Null)),
        "no recorded owner in this slice"
    );
    assert!(
        matches!(row.get("reason"), Some(Val::Null)),
        "no recorded reason in this slice"
    );
    assert!(matches!(row.get("human_gate"), Some(Val::Bool(false))));
    let source = row.get("source").expect("source");
    for key in [
        "kind",
        "repository",
        "issue",
        "revision",
        "freshness",
        "completeness",
        "observed_at",
    ] {
        assert!(source.get(key).is_some(), "source must expose {key}");
    }
    assert_eq!(source.get("freshness").and_then(Val::as_str), Some("fresh"));
    assert_eq!(
        source.get("completeness").and_then(Val::as_str),
        Some("complete")
    );
    assert!(
        matches!(source.get("observed_at"), Some(Val::Null)),
        "the GitHub spec is not re-observed by a board read"
    );
}

#[test]
fn board_page_read_has_no_process_or_remote_surface_per_row() {
    // Static surface scan (the repo's no_network_surface.rs pattern): the
    // read-model module must not be able to spawn a process or reach the
    // network per rendered row.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let board_source = std::fs::read_to_string(root.join("src/board.rs")).expect("read board.rs");
    for banned in [
        "std::process",
        "Command",
        "std::net",
        "TcpStream",
        "reqwest",
        "ureq",
        "gh_issue_text",
        "observe::",
        "adapters::",
        "remote::",
    ] {
        assert!(
            !board_source.contains(banned),
            "src/board.rs must not reference {banned}: a board read is a state-store projection"
        );
    }

    // Behavioural pin: every rendered row reports that the external spec
    // was NOT observed by this read (no per-row remote freshness claim).
    let fixture = Fixture::new("noremote");
    let state = fixture.open();
    seed_run(&state, REPO_A, 21, "run-noremote-0001");
    let page = page(&state, 20, None);
    let doc = page.to_doc();
    let rows = doc.get("rows").and_then(Val::as_array).expect("rows");
    assert!(!rows.is_empty());
    for row in rows {
        let source = row.get("source").expect("source");
        assert!(
            matches!(source.get("observed_at"), Some(Val::Null)),
            "no row may claim a fresh remote observation"
        );
    }
}

/// Replace one field of one row in a rendered page document (test tamper
/// helper).
fn set_row_field(doc: &mut Val, run: &str, key: &str, value: Val) {
    let Val::Obj(map) = doc else {
        panic!("page must be an object");
    };
    let Some(Val::Arr(rows)) = map.get_mut("rows") else {
        panic!("page must carry rows");
    };
    for row in rows.iter_mut() {
        let Val::Obj(fields) = row else { continue };
        if fields.get("run").and_then(Val::as_str) == Some(run) {
            fields.insert(key.to_string(), value);
            return;
        }
    }
    panic!("no row {run} in the page");
}

#[test]
fn board_contract_fixture_proves_verified_differs_from_reported_done() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bytes =
        std::fs::read(root.join("schemas/fixtures/board/board.valid.json")).expect("fixture bytes");
    let verdict = canter::schema::validate_bytes(canter::schema::Family::Board, &bytes);
    assert!(
        verdict.is_accepted(),
        "fixture refused: {}",
        verdict.message()
    );
    let doc = Val::parse_json(std::str::from_utf8(&bytes).expect("utf8")).expect("parse");
    let rows = doc.get("rows").and_then(Val::as_array).expect("rows");
    let reported_done = rows
        .iter()
        .find(|row| row.get("run").and_then(Val::as_str) == Some("run-widgets-0003"))
        .expect("reported-done row");
    let verified = rows
        .iter()
        .find(|row| row.get("run").and_then(Val::as_str) == Some("run-widgets-0006"))
        .expect("verified row");
    assert_eq!(
        reported_done.get("run_state").and_then(Val::as_str),
        Some("done")
    );
    assert_eq!(
        reported_done.get("stage").and_then(Val::as_str),
        Some("needs_attention")
    );
    assert_eq!(
        reported_done.get("verification").and_then(Val::as_str),
        Some("none")
    );
    assert_eq!(
        verified.get("stage").and_then(Val::as_str),
        Some("verified")
    );
    assert_eq!(
        verified.get("verification").and_then(Val::as_str),
        Some("passed")
    );
    assert!(
        !verified
            .get("evidence")
            .and_then(Val::as_array)
            .expect("evidence")
            .is_empty(),
        "verified rows carry evidence references"
    );

    // Tamper 1: relabel the reported-done row as verified delivery.
    let mut tampered = doc.clone();
    set_row_field(
        &mut tampered,
        "run-widgets-0003",
        "stage",
        string("verified"),
    );
    let bytes = canter::canonical::canonical_bytes(&tampered);
    let verdict = canter::schema::validate_bytes(canter::schema::Family::Board, &bytes);
    assert_eq!(
        verdict.refusal(),
        Some(canter::schema::Refusal::Malformed),
        "reported done relabelled verified must be refused"
    );

    // Tamper 2: claim a passed verification with no evidence records.
    let mut tampered = doc.clone();
    set_row_field(
        &mut tampered,
        "run-widgets-0003",
        "verification",
        string("passed"),
    );
    let bytes = canter::canonical::canonical_bytes(&tampered);
    let verdict = canter::schema::validate_bytes(canter::schema::Family::Board, &bytes);
    assert_eq!(verdict.refusal(), Some(canter::schema::Refusal::Malformed));
}

#[test]
fn board_redacts_recorded_text_before_it_becomes_a_row() {
    let fixture = Fixture::new("redaction");
    let state = fixture.open();
    // Secret-shaped material is assembled at runtime (never a literal).
    let token = format!("ghp_{}", "0123456789abcdef0123456789abcdef012345");
    let reviewer = format!("reviewer {token}");
    seed_run(&state, REPO_A, 31, "run-redact-0001");
    record_pass(
        &state,
        "run-redact-0001",
        &reviewer,
        "4444444444444444444444444444444444444444",
    );

    let page = page(&state, 20, None);
    assert_page_valid(&page);
    let doc = canter::canonical::canonical_text(&page.to_doc());
    assert!(
        !doc.contains(&token),
        "the secret-shaped run must never reach a row"
    );
    assert!(doc.contains("[REDACTED]"), "redaction must be visible");
    let row = row_for(&page, "run-redact-0001");
    let reviewer_text = row.reviewer.as_deref().expect("reviewer recorded");
    assert!(reviewer_text.contains("[REDACTED]"));
    assert!(!reviewer_text.contains(&token));

    // The boundary is enforced twice: a page carrying unredacted recorded
    // text is refused by the family validator itself.
    let mut tampered = page.to_doc();
    set_row_field(
        &mut tampered,
        "run-redact-0001",
        "reviewer",
        string(&reviewer),
    );
    let bytes = canter::canonical::canonical_bytes(&tampered);
    let verdict = canter::schema::validate_bytes(canter::schema::Family::Board, &bytes);
    assert_eq!(verdict.refusal(), Some(canter::schema::Refusal::Malformed));
}

#[test]
fn board_evidence_references_are_bounded_with_explicit_overflow() {
    let fixture = Fixture::new("evidence");
    let state = fixture.open();
    seed_run(&state, REPO_A, 41, "run-evidence-0001");
    for index in 0..(BOARD_EVIDENCE_REF_MAX + 2) {
        let head = format!("{index:040x}");
        record_pass(&state, "run-evidence-0001", "reviewer-example", &head);
    }
    let page = page(&state, 20, None);
    assert_page_valid(&page);
    let row = row_for(&page, "run-evidence-0001");
    assert_eq!(row.verification, "passed");
    assert_eq!(row.stage, "verified");
    assert_eq!(
        row.evidence.len(),
        BOARD_EVIDENCE_REF_MAX,
        "reference list is capped"
    );
    assert_eq!(
        row.evidence_total,
        (BOARD_EVIDENCE_REF_MAX + 2) as i64,
        "overflow stays explicit"
    );
    let unique: std::collections::BTreeSet<&String> = row.evidence.iter().collect();
    assert_eq!(unique.len(), row.evidence.len(), "references are unique");
}

#[test]
fn board_reads_are_read_only() {
    let fixture = Fixture::new("readonly");
    let state = fixture.open();
    seed_run(&state, REPO_A, 51, "run-readonly-0001");
    seed_run(&state, REPO_B, 51, "run-readonly-0002");
    record_pass(
        &state,
        "run-readonly-0002",
        "reviewer-example",
        "5555555555555555555555555555555555555555",
    );

    let before_bytes = std::fs::read(fixture.db()).expect("db bytes");
    let before_digest = canter::canonical::sha256_hex(&before_bytes);
    let before_summary = state.summary().expect("summary before");

    // Walk every page twice; any write path would move the journal/epoch.
    for _ in 0..2 {
        let mut cursor: Option<String> = None;
        loop {
            let page = page(&state, 1, cursor.as_deref());
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
    }

    let after_summary = state.summary().expect("summary after");
    assert_eq!(
        before_summary, after_summary,
        "reads must not journal or rotate"
    );
    let after_bytes = std::fs::read(fixture.db()).expect("db bytes after");
    assert_eq!(
        before_digest,
        canter::canonical::sha256_hex(&after_bytes),
        "reads must not write to the state database"
    );
    // A page is also pure: two reads of the same bounded window agree on
    // every fact except the fresh query time.
    let first = page(&state, 20, None);
    let second = page(&state, 20, None);
    assert_eq!(first.rows, second.rows);
}
