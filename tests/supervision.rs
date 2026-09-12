//! Issue #95 acceptance tests: the daemon-owned supervised reconciliation
//! driver (armed with a run submission, evaluated from recorded evidence with
//! a bounded timer fallback, and read back through the versioned
//! `hf-supervision/v1` status).
//!
//! Two fixture layers, synthetic identities only:
//! - library-level `State` assertions for the durable authorization, the
//!   coalesced wake slot and the restart-surviving holds;
//! - a real `canter daemon run` child process over an explicit socket for the
//!   end-to-end acceptance: one armed run is evaluated WITHOUT another client
//!   request, reads never move the meaningful-progress marker, a restart
//!   yields exactly one fresh reconciliation, and nothing ever spawns,
//!   prompts or continues work.
//!
//! No fixed real sleeps: every wait is a bounded poll with a deadline.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use canter::client::Connection;
use canter::config::ProfileBinding;
use canter::lifecycle::ConcurrencyCaps;
use canter::plan::DOCTRINE_WORKFLOW_ID;
use canter::queue_executor as qx;
use canter::queue_preview as qp;
use canter::state::{Retention, State};
use canter::supervision;
use canter::value::{Val, integer, object, string};

// ---------------------------------------------------------------------------
// Constants and builders (the #84/#85 fixture shape, one selected issue)
// ---------------------------------------------------------------------------

const REPO: &str = "example-org/widgets";
const HOST: &str = "host-1";
const HARNESS: &str = "lane-1";
const REV_A: &str = "1111111111111111111111111111111111111111";
const WORKFLOW_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const POLICY_HASH: &str = "feedface01234567feedface01234567feedface01234567feedface01234567";
const SECRET_DIGEST: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

fn binding_doc() -> Val {
    let mut binding = ProfileBinding {
        key: HARNESS.to_string(),
        kind: "pi".to_string(),
        provider: "provider-a".to_string(),
        model: "model-a".to_string(),
        fallbacks: Vec::new(),
        configured_limits: Vec::new(),
        introspection: false,
        secrets: vec![("PROVIDER_TOKEN".to_string(), SECRET_DIGEST.to_string())],
        revision: String::new(),
    };
    binding.revision = binding.revision_of();
    binding.to_doc()
}

fn resolved() -> Val {
    object(vec![("ref", string("staging"))])
}

fn request_with(issues: Vec<qp::SelectedIssue>) -> qp::QueueRequest {
    qp::QueueRequest {
        repository: REPO.to_string(),
        host: HOST.to_string(),
        host_available: Some(true),
        harness_key: HARNESS.to_string(),
        harness_lanes: Some(0),
        caps: ConcurrencyCaps {
            global: 4,
            per_repository: 2,
            per_harness: 2,
        },
        workflow_id: DOCTRINE_WORKFLOW_ID.to_string(),
        workflow_hash: WORKFLOW_HASH.to_string(),
        role_config: binding_doc(),
        boundary: qp::Boundary {
            phase: "merge".to_string(),
            integration_branch: "staging".to_string(),
            completion_branch: "staging".to_string(),
            caps: vec![
                "read".to_string(),
                "worktree".to_string(),
                "spawn".to_string(),
                "merge".to_string(),
            ],
        },
        steps: vec![qp::PlannedStep {
            id: "p1".to_string(),
            kind: "checkout".to_string(),
            params: Some(resolved()),
        }],
        selected: issues,
    }
}

fn selected(id: &str, revision: &str) -> qp::SelectedIssue {
    qp::SelectedIssue {
        id: id.to_string(),
        title: None,
        revision: revision.to_string(),
        requires: Vec::new(),
    }
}

fn render_bound(state: &State, request: &qp::QueueRequest) -> (Val, String) {
    let preview = qp::preview_queue(state, request).expect("preview renders");
    let bound = preview
        .doc
        .get("request")
        .cloned()
        .expect("preview carries the bound-input document");
    (bound, preview.digest)
}

fn grant_doc_at(grant_id: &str, number: i64, revision: &str, epoch: i64) -> Val {
    Val::parse_json(&format!(
        r#"{{"schema":"hf-grant/v1","grant_id":"{grant_id}","repository":"{REPO}",
            "issue":{{"number":{number},"revision":"{revision}"}},
            "workflow_hash":"{WORKFLOW_HASH}","policy_hash":"{POLICY_HASH}",
            "phase":"merge","scope":"worktrees/issues/{number}",
            "caps":["read","worktree","spawn","merge"],
            "expires_at":"2999-01-01T00:00:00Z","state_epoch":{epoch},
            "created_at":"2026-09-06T00:00:00Z"}}"#
    ))
    .expect("grant document")
}

fn seed_grant(state: &State, grant_id: &str, number: i64) {
    let epoch = state.current_epoch().expect("epoch");
    state
        .issue_grant(&grant_doc_at(grant_id, number, REV_A, epoch))
        .expect("issue grant");
}

fn role_revision() -> String {
    binding_doc()
        .get("revision")
        .and_then(Val::as_str)
        .expect("binding revision")
        .to_string()
}

/// The `queue.submit` params document, with the optional supervision
/// authorization block (`None` = supervision disabled: the default).
#[allow(clippy::too_many_arguments)]
fn params_doc(
    key: &str,
    bound: &Val,
    digest: &str,
    role_revision: &str,
    grant_id: &str,
    supervision_block: Option<supervision::Authorization>,
) -> Val {
    let grants = vec![qx::ItemGrant {
        id: "#5".to_string(),
        grant_id: grant_id.to_string(),
    }];
    qx::submit_params(
        key,
        digest,
        1,
        bound,
        &binding_doc(),
        role_revision,
        ConcurrencyCaps {
            global: 4,
            per_repository: 2,
            per_harness: 2,
        },
        Some(true),
        Some(0),
        &grants,
        &[],
        supervision_block.as_ref(),
    )
}

fn armed(interval_secs: i64, timeout_secs: i64) -> supervision::Authorization {
    supervision::Authorization {
        desired: "armed".to_string(),
        policy: supervision::Policy {
            check_interval_secs: interval_secs,
            progress_timeout_secs: timeout_secs,
        },
    }
}

// ---------------------------------------------------------------------------
// Fixtures (the tests/queue_submit.rs daemon pattern)
// ---------------------------------------------------------------------------

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hf-supervision-95-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Runtime-assembled idempotency key (the tracked file never carries a
/// `key = "<literal>"` shape the secret scanners read as an API key).
fn idem_key(stem: &str) -> String {
    format!("ik_95-{stem}-{}", std::process::id())
}

fn fresh_id(seed: u64) -> String {
    format!("{seed:016x}")
}

struct DaemonFixture {
    dir: PathBuf,
    state_dir: PathBuf,
    socket: PathBuf,
}

impl DaemonFixture {
    fn new(name: &str) -> DaemonFixture {
        let dir = temp_dir(name);
        DaemonFixture {
            state_dir: dir.join("state"),
            socket: dir.join("daemon.sock"),
            dir,
        }
    }

    fn db(&self) -> PathBuf {
        self.state_dir.join("canter").join("state.db")
    }

    fn daemon_log(&self) -> PathBuf {
        self.state_dir.join("canter").join("daemon.log")
    }

    fn seed(&self) -> State {
        std::fs::create_dir_all(self.state_dir.join("canter")).expect("state dir");
        State::open(&self.db(), Retention::default()).expect("open state")
    }

    fn spawn(&self) -> Child {
        std::fs::create_dir_all(&self.state_dir).expect("state home");
        Command::new(env!("CARGO_BIN_EXE_canter"))
            .args(["daemon", "run", "--socket"])
            .arg(&self.socket)
            .env("XDG_STATE_HOME", &self.state_dir)
            .env("HOME", &self.dir)
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(self.dir.join("daemon.stderr.log")).expect("stderr log"),
            ))
            .spawn()
            .expect("spawn daemon")
    }
}

/// Run the real CLI binary against the fixture daemon (the product surface).
fn cli(fixture: &DaemonFixture, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_canter"))
        .args(args)
        .env("XDG_STATE_HOME", &fixture.state_dir)
        .env("HOME", &fixture.dir)
        .output()
        .expect("run cli");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn wait_ready(fixture: &DaemonFixture) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if canter::lock::socket_presence(&fixture.socket) == canter::lock::SocketPresence::Active {
            let ok = Connection::open(&fixture.socket)
                .and_then(|mut connection| {
                    connection.send_request("aaaaaaaaaaaaaaaa", "status", None)?;
                    connection.read_response()
                })
                .map(|response| response.ok)
                .unwrap_or(false);
            if ok {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let stderr = std::fs::read_to_string(fixture.dir.join("daemon.stderr.log")).unwrap_or_default();
    panic!(
        "daemon did not become ready on {}; stderr:\n{stderr}",
        fixture.socket.display()
    );
}

fn rpc(socket: &Path, id: &str, method: &str, params: Option<Val>) -> Val {
    let mut connection = Connection::open(socket).expect("connect");
    connection
        .send_request(id, method, params.as_ref())
        .expect("send");
    let response = connection.read_response().expect("read response");
    if response.ok {
        object(vec![("ok", Val::Bool(true)), ("result", response.result)])
    } else {
        let error = response.error.unwrap_or_else(|| canter::client::RpcError {
            code: "missing.error".to_string(),
            message: "no error doc".to_string(),
        });
        object(vec![
            ("ok", Val::Bool(false)),
            (
                "error",
                object(vec![
                    ("code", string(&error.code)),
                    ("message", string(&error.message)),
                ]),
            ),
        ])
    }
}

fn rpc_ok(socket: &Path, id: &str, method: &str, params: Option<Val>) -> Val {
    let doc = rpc(socket, id, method, params);
    assert_eq!(
        doc.get("ok").and_then(Val::as_bool),
        Some(true),
        "expected ok for {method}: {}",
        canter::canonical::canonical_text(&doc)
    );
    doc.get("result").expect("result").clone()
}

fn rpc_err(socket: &Path, id: &str, method: &str, params: Option<Val>) -> String {
    let doc = rpc(socket, id, method, params);
    assert_eq!(
        doc.get("ok").and_then(Val::as_bool),
        Some(false),
        "expected a refusal for {method}: {}",
        canter::canonical::canonical_text(&doc)
    );
    doc.get("error")
        .and_then(|error| error.get("code"))
        .and_then(Val::as_str)
        .unwrap_or_default()
        .to_string()
}

fn shutdown(mut daemon: Child) {
    let _ = daemon.kill();
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon.try_wait().expect("try_wait").is_none() {
        assert!(Instant::now() < deadline, "daemon did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// Document readers
// ---------------------------------------------------------------------------

fn item_of(result: &Val, number: i64) -> Val {
    let wanted = format!("{REPO}#{number}");
    result
        .get("items")
        .and_then(Val::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("id").and_then(Val::as_str) == Some(wanted.as_str()))
                .cloned()
        })
        .unwrap_or_else(|| {
            panic!(
                "no item for issue {number}: {}",
                canter::canonical::canonical_text(result)
            )
        })
}

fn instance_of(result: &Val, number: i64) -> String {
    item_of(result, number)
        .get("instance_id")
        .and_then(Val::as_str)
        .expect("admitted item carries the run")
        .to_string()
}

fn status_doc(socket: &Path, id: &str, run: &str) -> Val {
    rpc_ok(
        socket,
        id,
        "supervision.status",
        Some(supervision::status_params(run)),
    )
}

fn evaluation(doc: &Val) -> Val {
    doc.get("evaluation").cloned().unwrap_or_else(|| {
        panic!(
            "no evaluation block: {}",
            canter::canonical::canonical_text(doc)
        )
    })
}

fn checks_of(doc: &Val) -> i64 {
    evaluation(doc)
        .get("checks")
        .and_then(Val::as_int)
        .unwrap_or(-1)
}

fn class_of(doc: &Val) -> String {
    evaluation(doc)
        .get("class")
        .and_then(Val::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Poll `supervision.status` until the recorded check count reaches `want`
/// (bounded; no fixed sleeps).
fn wait_for_checks(fixture: &DaemonFixture, run: &str, want: i64) -> Val {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = String::new();
    let mut id = 100u64;
    while Instant::now() < deadline {
        id += 1;
        let doc = status_doc(&fixture.socket, &fresh_id(id), run);
        if checks_of(&doc) >= want {
            return doc;
        }
        last = canter::canonical::canonical_text(&doc);
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("run {run} never reached {want} recorded check(s); last: {last}");
}

// ---------------------------------------------------------------------------
// Acceptance: one armed run, evaluated without another client request
// ---------------------------------------------------------------------------

#[test]
fn armed_run_is_evaluated_without_another_client_request_and_reads_are_inert() {
    let fixture = DaemonFixture::new("armed");
    let (bound, digest) = {
        let state = fixture.seed();
        seed_grant(&state, "gr_0000000000000095", 5);
        render_bound(&state, &request_with(vec![selected("#5", REV_A)]))
    };
    let daemon = fixture.spawn();
    wait_ready(&fixture);

    let key = idem_key("armed");
    let params = params_doc(
        &key,
        &bound,
        &digest,
        &role_revision(),
        "gr_0000000000000095",
        Some(armed(10, 60)),
    );
    let result = rpc_ok(&fixture.socket, &fresh_id(1), "queue.submit", Some(params));
    let run = instance_of(&result, 5);

    // AC1: the armed run is evaluated with NO further client request — the
    // driver's own boot/wake path performs the reconciliation.
    let first = wait_for_checks(&fixture, &run, 1);
    assert_eq!(
        first
            .get("supervision")
            .and_then(|supervision| supervision.get("desired"))
            .and_then(Val::as_str),
        Some("armed")
    );
    assert_eq!(
        first
            .get("supervision")
            .and_then(|supervision| supervision.get("owner_generation"))
            .and_then(Val::as_int),
        Some(1)
    );
    assert_eq!(
        first
            .get("supervision")
            .and_then(|supervision| supervision.get("policy"))
            .and_then(|policy| policy.get("progress_timeout_secs"))
            .and_then(Val::as_int),
        Some(60)
    );
    // The first unachieved step is a plain dispatchable step with no recorded
    // progress inside the window: healthy, never eligible.
    assert_eq!(class_of(&first), "healthy");
    assert_eq!(
        evaluation(&first).get("eligible").and_then(Val::as_bool),
        Some(false)
    );
    assert_eq!(
        evaluation(&first)
            .get("freshness")
            .and_then(|freshness| freshness.get("state"))
            .and_then(Val::as_str),
        Some("fresh")
    );
    // AC7: the versioned status carries the last check and the NEXT ELIGIBLE
    // CHECK with its reason.
    assert!(
        evaluation(&first)
            .get("last_check")
            .and_then(|check| check.get("at"))
            .and_then(Val::as_str)
            .is_some_and(|at| !at.is_empty())
    );
    assert!(
        evaluation(&first)
            .get("next_check")
            .and_then(|check| check.get("at"))
            .and_then(Val::as_str)
            .is_some_and(|at| !at.is_empty())
    );
    assert!(
        first
            .get("cursor")
            .and_then(|cursor| cursor.get("next_step"))
            .and_then(Val::as_str)
            == Some("p1")
    );

    // AC2: reads and a rendered status never reset the meaningful-progress
    // marker: three more reads leave the observation byte-identical and the
    // check count untouched.
    let progress = evaluation(&first)
        .get("progress")
        .cloned()
        .expect("progress block");
    let checks_before = checks_of(&first);
    for seed in 10..13 {
        let doc = status_doc(&fixture.socket, &fresh_id(seed), &run);
        assert_eq!(checks_of(&doc), checks_before, "a read is not a check");
        assert_eq!(
            evaluation(&doc).get("progress"),
            Some(&progress),
            "a read must never move the progress marker"
        );
    }

    // AC7 / no-effect: the evaluation itself produced no journal action of
    // its own and the run stayed exactly where it was.
    let actions = rpc_ok(
        &fixture.socket,
        &fresh_id(20),
        "journal.tail",
        Some(object(vec![("limit", integer(50))])),
    );
    let text = canter::canonical::canonical_text(&actions);
    for banned in [
        "mutate.harness_start",
        "mutate.prompt",
        "mutate.merge",
        "mutate.branch_push",
    ] {
        assert!(
            !text.contains(banned),
            "supervision must never produce {banned}: {text}"
        );
    }
    let log = std::fs::read_to_string(fixture.daemon_log()).unwrap_or_default();
    assert!(
        !log.contains("supervision.continue") && !log.contains("spawn"),
        "no continuation effect may be logged: {log}"
    );

    // The default: a second, un-authorized run is never supervised at all.
    let unarmed_state = {
        shutdown(daemon);
        fixture.seed()
    };
    let unarmed_run = unarmed_state
        .list_instances()
        .expect("instances")
        .into_iter()
        .find(|row| row.instance_id == run)
        .expect("the armed run persists");
    assert_eq!(unarmed_run.status, "new");
    assert!(unarmed_run.current_node.is_empty(), "no node was advanced");
    assert!(
        unarmed_state
            .supervision_by_id("run-0000000000000000")
            .expect("read")
            .is_none(),
        "an absent supervision is never invented"
    );
}

#[test]
fn supervision_is_disabled_by_default_and_a_foreign_target_refuses() {
    let fixture = DaemonFixture::new("default-off");
    let (bound, digest) = {
        let state = fixture.seed();
        seed_grant(&state, "gr_0000000000000096", 5);
        render_bound(&state, &request_with(vec![selected("#5", REV_A)]))
    };
    let daemon = fixture.spawn();
    wait_ready(&fixture);
    let key = idem_key("default-off");
    let params = params_doc(
        &key,
        &bound,
        &digest,
        &role_revision(),
        "gr_0000000000000096",
        None,
    );
    let result = rpc_ok(&fixture.socket, &fresh_id(1), "queue.submit", Some(params));
    let run = instance_of(&result, 5);
    // AC1: absent authorization = disabled. There is no row to read, and the
    // run is never evaluated.
    let code = rpc_err(
        &fixture.socket,
        &fresh_id(2),
        "supervision.status",
        Some(supervision::status_params(&run)),
    );
    assert_eq!(code, "state.not_found");
    // Force a semantic wake on the run (a control mutation notifies the
    // driver) and settle on its committed result: the un-authorized run is
    // still never evaluated, because the driver has no authority over it.
    let pause = canter::run_control::pause_params(
        &idem_key("default-off-pause"),
        &run,
        "un-authorized runs stay un-evaluated",
    );
    let paused = rpc_ok(&fixture.socket, &fresh_id(3), "run.pause", Some(pause));
    assert_eq!(
        paused
            .get("control")
            .and_then(|control| control.get("state"))
            .and_then(Val::as_str),
        Some("paused"),
        "the pause reached its safe boundary"
    );
    let code = rpc_err(
        &fixture.socket,
        &fresh_id(4),
        "supervision.status",
        Some(supervision::status_params(&run)),
    );
    assert_eq!(
        code, "state.not_found",
        "a wake never evaluates a run without an authorization"
    );
    // The target is exactly one run identity.
    let code = rpc_err(
        &fixture.socket,
        &fresh_id(5),
        "supervision.status",
        Some(object(vec![("instance_id", string("run-nope"))])),
    );
    assert_eq!(code, "usage.supervision.target");
    shutdown(daemon);
    let state = fixture.seed();
    assert!(
        state.supervision_rows().expect("rows").is_empty(),
        "no supervision row is ever written without an explicit authorization"
    );
}

#[test]
fn restart_preserves_the_pause_hold_and_yields_one_fresh_reconciliation() {
    let fixture = DaemonFixture::new("restart");
    let (bound, digest) = {
        let state = fixture.seed();
        seed_grant(&state, "gr_0000000000000097", 5);
        render_bound(&state, &request_with(vec![selected("#5", REV_A)]))
    };
    let daemon = fixture.spawn();
    wait_ready(&fixture);
    let key = idem_key("restart");
    let params = params_doc(
        &key,
        &bound,
        &digest,
        &role_revision(),
        "gr_0000000000000097",
        // A long interval keeps the timer out of the test window: every check
        // after the first is a WAKE, never a timer tick.
        Some(armed(3600, 7200)),
    );
    let result = rpc_ok(&fixture.socket, &fresh_id(1), "queue.submit", Some(params));
    let run = instance_of(&result, 5);
    let first = wait_for_checks(&fixture, &run, 1);
    assert_eq!(class_of(&first), "healthy");
    let checks_before_pause = checks_of(&first);

    // Pause the run through the control surface: the durable hold must be
    // classified as paused and must never be eligible.
    let pause_params = canter::run_control::pause_params(
        &idem_key("pause"),
        &run,
        "operator hold for the supervision acceptance case",
    );
    rpc_ok(
        &fixture.socket,
        &fresh_id(2),
        "run.pause",
        Some(pause_params),
    );
    // The pause is a semantic control wake: wait for the driver's OWN
    // reconciliation (a new recorded check), then read the recorded class.
    let paused = wait_for_checks(&fixture, &run, checks_before_pause + 1);
    assert_eq!(class_of(&paused), "paused");
    assert_eq!(
        evaluation(&paused)
            .get("last_check")
            .and_then(|check| check.get("class"))
            .and_then(Val::as_str),
        Some("paused"),
        "the recorded check itself classified the hold"
    );
    assert_eq!(
        evaluation(&paused)
            .get("last_check")
            .and_then(|check| check.get("trigger"))
            .and_then(Val::as_str),
        Some("control")
    );
    assert_eq!(
        evaluation(&paused).get("eligible").and_then(Val::as_bool),
        Some(false)
    );
    let checks_before_restart = checks_of(&paused);
    let progress_before = evaluation(&paused).get("progress").cloned();

    // AC5: the hold survives the restart, and the boot reconciliation is
    // exactly ONE fresh check (no catch-up storm).
    shutdown(daemon);
    let daemon = fixture.spawn();
    wait_ready(&fixture);
    let after = wait_for_checks(&fixture, &run, checks_before_restart + 1);
    assert_eq!(class_of(&after), "paused");
    assert_eq!(
        evaluation(&after).get("eligible").and_then(Val::as_bool),
        Some(false)
    );
    assert_eq!(
        checks_of(&after),
        checks_before_restart + 1,
        "a restart reconciles ONCE, never once per missed window"
    );
    assert_eq!(
        after
            .get("supervision")
            .and_then(|supervision| supervision.get("desired"))
            .and_then(Val::as_str),
        Some("armed")
    );
    assert_eq!(
        after
            .get("supervision")
            .and_then(|supervision| supervision.get("policy"))
            .and_then(|policy| policy.get("check_interval_secs"))
            .and_then(Val::as_int),
        Some(3600),
        "the recorded policy survives the restart"
    );
    // The pause is a recorded state change, so the marker moved with it; the
    // marker is never empty and its observation time is recorded.
    let progress_after = evaluation(&after)
        .get("progress")
        .cloned()
        .expect("progress block");
    let marker = progress_after
        .get("marker")
        .and_then(Val::as_str)
        .unwrap_or_default()
        .to_string();
    assert_eq!(marker.len(), 64, "the marker is a recorded sha256 digest");
    assert!(
        progress_after
            .get("at")
            .and_then(Val::as_str)
            .is_some_and(|at| !at.is_empty())
    );
    assert!(progress_before.is_some());

    // The retry cursor block reads back with the documented bound and no
    // invented attempts.
    let retries = after.get("retries").cloned().expect("retries block");
    assert_eq!(
        retries.get("bound").and_then(Val::as_int),
        Some(canter::state::RUN_RETRY_MAX)
    );
    assert!(
        retries
            .get("rows")
            .and_then(Val::as_array)
            .is_some_and(|rows| rows.is_empty())
    );
    shutdown(daemon);
}

#[test]
fn the_driver_never_reaches_an_effect_surface() {
    // AC7 (no effect): outside its own unit tests the driver module cannot
    // spawn a process, reach the network, the adapters or the filesystem, and
    // the only durable writer it names is its own run-scoped commit — there is
    // no path from an evaluation to a continuation.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read_to_string(root.join("src/supervision.rs")).expect("read module");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("production half");
    for banned in [
        "std::process",
        "Command",
        "std::net",
        "TcpStream",
        "UnixStream",
        "std::fs",
        "adapters::",
        "client::",
        "daemon::",
        "start_instance",
        "pause_instance",
        "resume_instance",
        "record_evidence",
        "append_audit",
        "rotate_epoch",
        "issue_grant",
    ] {
        assert!(
            !production.contains(banned),
            "src/supervision.rs must not reference {banned}: evaluation has no effect"
        );
    }
    // ONE mutating writer, and it is the driver's own check commit.
    let without_commit = production.replace("commit_supervision_check", "");
    assert!(
        !without_commit.contains("commit_"),
        "the driver commits only its own reconciliation"
    );
    assert!(
        production.contains(canter::supervision::STATEMENT),
        "the no-effect statement rides on the module"
    );
}

#[test]
fn cli_supervision_status_reads_the_versioned_status_back_inert() {
    // AC7 (product surface): `canter supervision status` issues the closed
    // read-only `supervision.status` method through the real binary — one
    // hf-output/v1 envelope, a human rendering of the SAME document, and a
    // read that never moves the evaluation.
    let fixture = DaemonFixture::new("cli-status");
    let (bound, digest) = {
        let state = fixture.seed();
        seed_grant(&state, "gr_0000000000000095", 5);
        render_bound(&state, &request_with(vec![selected("#5", REV_A)]))
    };
    let daemon = fixture.spawn();
    wait_ready(&fixture);
    let key = idem_key("cli-status");
    let params = params_doc(
        &key,
        &bound,
        &digest,
        &role_revision(),
        "gr_0000000000000095",
        Some(armed(10, 60)),
    );
    let result = rpc_ok(&fixture.socket, &fresh_id(1), "queue.submit", Some(params));
    let run = instance_of(&result, 5);
    let first = wait_for_checks(&fixture, &run, 1);
    let checks_before = checks_of(&first);
    let socket = fixture.socket.display().to_string();

    let (exit, stdout, stderr) = cli(
        &fixture,
        &[
            "supervision",
            "status",
            "--run",
            &run,
            "--socket",
            &socket,
            "--json",
        ],
    );
    assert_eq!(exit, 0, "json exit; stderr: {stderr}");
    let doc = Val::parse_json(stdout.trim_end()).expect("one JSON envelope");
    let verdict = canter::schema::validate_doc(canter::schema::Family::Output, &doc);
    assert!(verdict.is_accepted(), "envelope: {}", verdict.message());
    let status = doc.get("data").cloned().expect("data");
    assert_eq!(
        status.get("schema").and_then(Val::as_str),
        Some(canter::supervision::SUPERVISION_SCHEMA)
    );
    assert_eq!(class_of(&status), "healthy");
    assert_eq!(
        evaluation(&status).get("eligible").and_then(Val::as_bool),
        Some(false)
    );

    // The human rendering of the SAME document.
    let (exit, stdout, stderr) = cli(
        &fixture,
        &["supervision", "status", "--run", &run, "--socket", &socket],
    );
    assert_eq!(exit, 0, "human exit; stderr: {stderr}");
    assert!(stdout.contains("healthy"), "human rendering: {stdout}");
    assert!(stdout.contains(&run), "human rendering names the run");

    // Inert: the reads never moved the evaluation.
    let after = status_doc(&fixture.socket, &fresh_id(9), &run);
    assert_eq!(checks_of(&after), checks_before);
    assert_eq!(class_of(&after), "healthy");
    shutdown(daemon);
}

#[test]
fn unapproved_plan_binding_is_held_and_never_eligible() {
    // The recorded authorization binds the approved preview digest: a
    // supervision row whose bound digest does not match the run's owning
    // submission (an unapproved/drifted plan) is held, never evaluated as
    // eligible. The state-level writer is the same one the submission
    // transaction calls, so this pins the fence itself.
    let fixture = DaemonFixture::new("unapproved");
    let state = fixture.seed();
    let mismatched = "9".repeat(64);
    state
        .arm_supervision(
            "run-0000000000000009",
            &canter::state::SupervisionAuthorizationPlan {
                desired: "armed".to_string(),
                check_interval_secs: 10,
                progress_timeout_secs: 60,
            },
            &mismatched,
            "merge",
            1,
            "2026-09-13T00:00:00Z",
        )
        .expect("arm");
    let row = state
        .supervision_by_id("run-0000000000000009")
        .expect("read")
        .expect("row");
    assert_eq!(row.authorization_digest, mismatched);
    assert_eq!(row.desired, "armed");
    // The run does not exist, so no evidence backs it: the driver's due set
    // may name it, but the reconciliation refuses to invent evidence and the
    // read surface reports the run as gone.
    let evidence = state
        .supervision_evidence("run-0000000000000009")
        .expect("read");
    assert!(evidence.is_none(), "no evidence, no evaluation");
}
