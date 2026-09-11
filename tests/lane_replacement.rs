//! Issue #73 acceptance integration tests over the real binary and socket:
//! request-only lane replacement records (idempotent requests, mandatory
//! identity bindings, CAS transitions, held/ambiguous outcomes, restart
//! durability of the record + history + next allowed transition).
//!
//! Every test spawns `herdr-fleet daemon run` as a child process with
//! isolated XDG state and an explicit socket under a per-test temp dir;
//! nothing here touches the real host state, the service manager, or the
//! network. All identities are synthetic.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use herdr_fleet::client::{Connection, RpcError};
use herdr_fleet::value::{Val, bool_, integer, null, object, string};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-fleet")
}

struct Fixture {
    dir: PathBuf,
    state_dir: PathBuf,
    socket: PathBuf,
}

#[derive(Clone, Copy)]
enum PathMode {
    Host,
    /// A PATH that contains nothing: proves the exercised surface needs no
    /// external tool (no spawn/shell/Git reachability).
    WithoutTools,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("hf-lr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        Fixture {
            state_dir: dir.join("state"),
            socket: dir.join("daemon.sock"),
            dir,
        }
    }

    fn spawn(&self, crash_point: Option<&str>) -> Child {
        self.spawn_with(crash_point, PathMode::Host)
    }

    fn spawn_with(&self, crash_point: Option<&str>, path_mode: PathMode) -> Child {
        let mut command = Command::new(bin());
        let stderr_path = self.dir.join("daemon.stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).expect("stderr log");
        command
            .args(["daemon", "run", "--socket"])
            .arg(&self.socket)
            .env("XDG_STATE_HOME", &self.state_dir)
            .env("HOME", &self.dir)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file));
        if let Some(point) = crash_point {
            command.env("HERDR_FLEET_CRASH_POINT", point);
        }
        if let PathMode::WithoutTools = path_mode {
            let empty = self.dir.join("empty-bin");
            std::fs::create_dir_all(&empty).expect("empty bin");
            command.env("PATH", &empty);
        }
        command.spawn().expect("spawn daemon")
    }
}

fn wait_ready(fixture: &Fixture) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if herdr_fleet::lock::socket_presence(&fixture.socket)
            == herdr_fleet::lock::SocketPresence::Active
        {
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
    let stderr_text =
        std::fs::read_to_string(fixture.dir.join("daemon.stderr.log")).unwrap_or_default();
    panic!(
        "daemon did not become ready on {}; stderr:\n{}",
        fixture.socket.display(),
        stderr_text
    );
}

fn rpc(socket: &Path, id: &str, method: &str, params: Option<Val>) -> Val {
    let mut connection = Connection::open(socket).expect("connect");
    connection
        .send_request(id, method, params.as_ref())
        .expect("send");
    let response = connection.read_response().expect("read response");
    if response.ok {
        object(vec![("ok", bool_(true)), ("result", response.result)])
    } else {
        let error = response.error.unwrap_or_else(|| RpcError {
            code: "missing.error".to_string(),
            message: "no error doc".to_string(),
        });
        object(vec![
            ("ok", bool_(false)),
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
        "expected ok response for {method}: {}",
        herdr_fleet::canonical::canonical_text(&doc)
    );
    doc.get("result").expect("result").clone()
}

fn rpc_err(socket: &Path, id: &str, method: &str, params: Option<Val>) -> (String, String) {
    let doc = rpc(socket, id, method, params);
    assert_eq!(
        doc.get("ok").and_then(Val::as_bool),
        Some(false),
        "expected refused response for {method}: {}",
        herdr_fleet::canonical::canonical_text(&doc)
    );
    let error = doc.get("error").expect("error doc");
    (
        error
            .get("code")
            .and_then(Val::as_str)
            .unwrap_or("")
            .to_string(),
        error
            .get("message")
            .and_then(Val::as_str)
            .unwrap_or("")
            .to_string(),
    )
}

fn fresh_id(seed: u32) -> String {
    format!("{:08x}", seed + std::process::id())
}

/// Send one request and drop the connection without reading the response
/// (used when the daemon aborts mid-handling at a crash point).
fn send_only(socket: &Path, id: &str, method: &str, params: Val) {
    let mut connection = Connection::open(socket).expect("connect");
    connection
        .send_request(id, method, Some(&params))
        .expect("send crash request");
    drop(connection);
}

fn wait_exit(mut child: Child, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            return;
        }
        assert!(Instant::now() < deadline, "{label} did not exit in time");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn shutdown(mut daemon: Child) {
    let _ = daemon.kill();
    wait_exit(daemon, "daemon");
}

// ---------------------------------------------------------------------------
// Synthetic request builders (public-data boundary: no host paths)
// ---------------------------------------------------------------------------

fn replacement_request_params_for(lane: &str, key: &str) -> Val {
    object(vec![
        ("idempotency_key", string(key)),
        ("lane_id", string(lane)),
        ("generation", integer(1)),
        ("source_session", string("sess-0001")),
        ("source_process", string("proc-0001")),
        ("role", string("implementer")),
        ("worktree", string("worktrees/issues/73")),
        ("reason", string("host rotation window")),
    ])
}

fn replacement_request_params(key: &str) -> Val {
    replacement_request_params_for("lane-7", key)
}

fn edit_params(mut params: Val, field: &str, value: Option<Val>) -> Val {
    if let Val::Obj(map) = &mut params {
        match value {
            Some(value) => {
                map.insert(field.to_string(), value);
            }
            None => {
                map.remove(field);
            }
        }
    }
    params
}

fn advance_params(replacement_id: &str, expected_phase: &str, generation: i64, key: &str) -> Val {
    object(vec![
        ("idempotency_key", string(key)),
        ("replacement_id", string(replacement_id)),
        ("expected_phase", string(expected_phase)),
        ("generation", integer(generation)),
    ])
}

fn hold_params(replacement_id: &str, key: &str) -> Val {
    object(vec![
        ("idempotency_key", string(key)),
        ("replacement_id", string(replacement_id)),
        ("reason", string("host drain window")),
    ])
}

fn cancel_params(replacement_id: &str, key: &str) -> Val {
    object(vec![
        ("idempotency_key", string(key)),
        ("replacement_id", string(replacement_id)),
    ])
}

fn status_params(replacement_id: &str) -> Val {
    object(vec![("replacement_id", string(replacement_id))])
}

fn replacement_of(result: &Val) -> &Val {
    result.get("replacement").expect("replacement")
}

fn field_str<'a>(val: &'a Val, key: &str) -> &'a str {
    val.get(key).and_then(Val::as_str).expect(key)
}

fn next_allowed(val: &Val) -> Option<&str> {
    val.get("next_allowed").and_then(Val::as_str)
}

fn request_replacement(socket: &Path, id_seed: u32, key: &str) -> String {
    request_replacement_for(socket, id_seed, "lane-7", key)
}

fn request_replacement_for(socket: &Path, id_seed: u32, lane: &str, key: &str) -> String {
    let result = rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.request",
        Some(replacement_request_params_for(lane, key)),
    );
    field_str(replacement_of(&result), "replacement_id").to_string()
}

// ---------------------------------------------------------------------------
// AC1: same lane/generation/request key is idempotent; concurrent requests
// cannot create two successor owners
// ---------------------------------------------------------------------------

#[test]
fn request_is_idempotent_and_concurrent_requests_yield_one_owner() {
    let fixture = Fixture::new("ac1-idempotent");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let key = "ik_request-00000001";

    let first = rpc_ok(
        &fixture.socket,
        &fresh_id(1),
        "lane.replacement.request",
        Some(replacement_request_params(key)),
    );
    let first = replacement_of(&first).clone();
    let replacement_id = field_str(&first, "replacement_id").to_string();
    assert_eq!(
        replacement_id,
        herdr_fleet::state::replacement_id_for("lane-7", 1),
        "the record identity is deterministic per lane generation"
    );
    assert_eq!(field_str(&first, "phase"), "requested");
    assert_eq!(field_str(&first, "outcome"), "pending");
    assert_eq!(
        first.get("next_allowed").and_then(Val::as_str),
        Some("quiescing")
    );
    // AC2: the record binds every source identity at request time.
    assert_eq!(
        first
            .get("source")
            .and_then(|s| s.get("session"))
            .and_then(Val::as_str),
        Some("sess-0001")
    );
    assert_eq!(
        first
            .get("source")
            .and_then(|s| s.get("process"))
            .and_then(Val::as_str),
        Some("proc-0001")
    );
    assert_eq!(
        first
            .get("source")
            .and_then(|s| s.get("role"))
            .and_then(Val::as_str),
        Some("implementer")
    );
    assert_eq!(
        first
            .get("source")
            .and_then(|s| s.get("worktree"))
            .and_then(Val::as_str),
        Some("worktrees/issues/73")
    );
    assert_eq!(field_str(&first, "reason"), "host rotation window");

    // Same request (same id + key) replays the recorded response: the same
    // record, no second successor.
    let replay = rpc_ok(
        &fixture.socket,
        &fresh_id(1),
        "lane.replacement.request",
        Some(replacement_request_params(key)),
    );
    assert_eq!(
        field_str(replacement_of(&replay), "replacement_id"),
        replacement_id,
        "an idempotent replay returns the same record"
    );

    // A different request (new key) for the same lane generation is refused.
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(2),
        "lane.replacement.request",
        Some(replacement_request_params("ik_request-00000002")),
    );
    assert_eq!(code, "refusal.replacement.exists", "{message}");

    // Concurrent requests from two connections for a fresh lane: exactly
    // one wins.
    let outcomes: Vec<(bool, String)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|i| {
                let socket = fixture.socket.clone();
                let id = fresh_id(10 + i);
                let key = format!("ik_race-0000000{i}");
                scope.spawn(move || {
                    let doc = rpc(
                        &socket,
                        &id,
                        "lane.replacement.request",
                        Some(replacement_request_params_for("lane-8", &key)),
                    );
                    let ok = doc.get("ok").and_then(Val::as_bool).unwrap_or(false);
                    let code = doc
                        .get("error")
                        .and_then(|error| error.get("code"))
                        .and_then(Val::as_str)
                        .unwrap_or("")
                        .to_string();
                    (ok, code)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("race thread"))
            .collect()
    });
    let ok_count = outcomes.iter().filter(|(ok, _)| *ok).count();
    let exists_count = outcomes
        .iter()
        .filter(|(_, code)| code == "refusal.replacement.exists")
        .count();
    assert_eq!(
        ok_count, 1,
        "exactly one concurrent request creates the record"
    );
    assert_eq!(exists_count, 1, "the other is a typed exists refusal");

    // The daemon still holds exactly one record for the lane generation.
    let status = rpc_ok(
        &fixture.socket,
        &fresh_id(20),
        "lane.replacement.status",
        Some(status_params(&replacement_id)),
    );
    assert_eq!(field_str(replacement_of(&status), "phase"), "requested");

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC2: missing/invalid identities refuse; no record is ever inferred
// ---------------------------------------------------------------------------

#[test]
fn missing_or_invalid_identities_refuse_without_creating_records() {
    let fixture = Fixture::new("ac2-identities");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let base = "ik_invalid-00000001";
    let cases: Vec<(&str, Val)> = vec![
        (
            "missing lane_id",
            edit_params(replacement_request_params(base), "lane_id", None),
        ),
        (
            "invalid lane_id",
            edit_params(
                replacement_request_params(base),
                "lane_id",
                Some(string("Bad_Lane")),
            ),
        ),
        (
            "invalid lane_id type",
            edit_params(
                replacement_request_params(base),
                "lane_id",
                Some(integer(7)),
            ),
        ),
        (
            "missing generation",
            edit_params(replacement_request_params(base), "generation", None),
        ),
        (
            "zero generation",
            edit_params(
                replacement_request_params(base),
                "generation",
                Some(integer(0)),
            ),
        ),
        (
            "missing source_session",
            edit_params(replacement_request_params(base), "source_session", None),
        ),
        (
            "empty source_session",
            edit_params(
                replacement_request_params(base),
                "source_session",
                Some(string("")),
            ),
        ),
        (
            "missing source_process",
            edit_params(replacement_request_params(base), "source_process", None),
        ),
        (
            "invalid source_process",
            edit_params(
                replacement_request_params(base),
                "source_process",
                Some(string("proc 0001")),
            ),
        ),
        (
            "missing role",
            edit_params(replacement_request_params(base), "role", None),
        ),
        (
            "unknown role",
            edit_params(
                replacement_request_params(base),
                "role",
                Some(string("admin")),
            ),
        ),
        (
            "missing worktree",
            edit_params(replacement_request_params(base), "worktree", None),
        ),
        (
            "host-absolute worktree",
            edit_params(
                replacement_request_params(base),
                "worktree",
                Some(string("/var/tmp/host-path")),
            ),
        ),
        (
            "traversal worktree",
            edit_params(
                replacement_request_params(base),
                "worktree",
                Some(string("../escape")),
            ),
        ),
        (
            "missing reason",
            edit_params(replacement_request_params(base), "reason", None),
        ),
        (
            "empty reason",
            edit_params(replacement_request_params(base), "reason", Some(string(""))),
        ),
    ];

    for (index, (label, params)) in cases.into_iter().enumerate() {
        let id = fresh_id(30 + index as u32);
        let (code, message) = rpc_err(
            &fixture.socket,
            &id,
            "lane.replacement.request",
            Some(params),
        );
        assert_eq!(
            code, "refusal.malformed",
            "case {label:?} must refuse as malformed: {message}"
        );
    }

    // No record was created — status of the would-be deterministic id is a
    // typed not-found, and the journal holds no request intent.
    let would_be = herdr_fleet::state::replacement_id_for("lane-7", 1);
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(90),
        "lane.replacement.status",
        Some(status_params(&would_be)),
    );
    assert_eq!(code, "state.not_found", "{message}");
    let tail = rpc_ok(
        &fixture.socket,
        &fresh_id(91),
        "journal.tail",
        Some(object(vec![
            ("after_seq", integer(0)),
            ("limit", integer(500)),
        ])),
    );
    let records = tail
        .get("records")
        .and_then(|value| value.as_array())
        .expect("records");
    let request_intents = records
        .iter()
        .filter(|record| {
            record.get("action").and_then(Val::as_str) == Some("mutate.lane-replacement.request")
        })
        .count();
    assert_eq!(request_intents, 0, "refused requests journal no intent");

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC3: transitions use transactional compare-and-set; invalid order, stale
// generation and replayed expectations cannot advance state
// ---------------------------------------------------------------------------

#[test]
fn advance_is_compare_and_set_fenced() {
    let fixture = Fixture::new("ac3-cas");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let replacement_id = request_replacement(&fixture.socket, 1, "ik_cas-request-00000001");
    let advance_key = "ik_cas-advance-00000001";

    // First transition: requested -> quiescing.
    let advanced = rpc_ok(
        &fixture.socket,
        &fresh_id(2),
        "lane.replacement.advance",
        Some(advance_params(&replacement_id, "requested", 1, advance_key)),
    );
    assert_eq!(field_str(replacement_of(&advanced), "phase"), "quiescing");
    assert_eq!(
        replacement_of(&advanced)
            .get("next_allowed")
            .and_then(Val::as_str),
        Some("checkpointed")
    );

    // Replaying the same request (same id + key) returns the recorded
    // response and does not advance again.
    let replay = rpc_ok(
        &fixture.socket,
        &fresh_id(2),
        "lane.replacement.advance",
        Some(advance_params(&replacement_id, "requested", 1, advance_key)),
    );
    assert_eq!(replay, advanced, "the replay returns the recorded response");

    // Stale generation is refused (P1 fence).
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(3),
        "lane.replacement.advance",
        Some(advance_params(
            &replacement_id,
            "quiescing",
            9,
            "ik_cas-stale-00000001",
        )),
    );
    assert_eq!(code, "refusal.replacement.stale", "{message}");

    // Invalid order: a forward skip and a replayed expectation both refuse.
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(4),
        "lane.replacement.advance",
        Some(advance_params(
            &replacement_id,
            "checkpointed",
            1,
            "ik_cas-skip-00000001",
        )),
    );
    assert_eq!(code, "refusal.replacement.order", "{message}");
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(5),
        "lane.replacement.advance",
        Some(advance_params(
            &replacement_id,
            "requested",
            1,
            "ik_cas-replay-00000001",
        )),
    );
    assert_eq!(code, "refusal.replacement.order", "{message}");

    // State never moved: phase, history length and next allowed are exact.
    let status = rpc_ok(
        &fixture.socket,
        &fresh_id(6),
        "lane.replacement.status",
        Some(status_params(&replacement_id)),
    );
    let replacement = replacement_of(&status);
    assert_eq!(field_str(replacement, "phase"), "quiescing");
    assert_eq!(next_allowed(replacement), Some("checkpointed"));
    let history = status
        .get("history")
        .and_then(|value| value.as_array())
        .expect("history");
    assert_eq!(history.len(), 2, "only the accepted transitions recorded");

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC4: the request-only surface has no spawn/kill/Git side effects and no
// authority uplift (no external tool is reachable; epoch/grants unchanged)
// ---------------------------------------------------------------------------

#[test]
fn request_surface_needs_no_external_tools_and_no_authority() {
    let fixture = Fixture::new("ac4-request-only");
    let daemon = fixture.spawn_with(None, PathMode::WithoutTools);
    wait_ready(&fixture);

    let epoch_before = rpc_ok(&fixture.socket, &fresh_id(1), "state.epoch", None);
    let grants_before = rpc_ok(&fixture.socket, &fresh_id(2), "grants.list", None);

    // The whole surface works with an empty PATH: no spawn, shell, or Git
    // invocation is reachable, so none can be happening.
    let replacement_id = request_replacement(&fixture.socket, 3, "ik_ro-request-00000001");
    let advanced = rpc_ok(
        &fixture.socket,
        &fresh_id(4),
        "lane.replacement.advance",
        Some(advance_params(
            &replacement_id,
            "requested",
            1,
            "ik_ro-advance-00000001",
        )),
    );
    assert_eq!(field_str(replacement_of(&advanced), "phase"), "quiescing");
    let status = rpc_ok(
        &fixture.socket,
        &fresh_id(5),
        "lane.replacement.status",
        Some(status_params(&replacement_id)),
    );
    assert_eq!(field_str(replacement_of(&status), "phase"), "quiescing");

    // No authority uplift: no grants are required/issued/consumed and the
    // epoch is untouched by the request-only surface.
    let epoch_after = rpc_ok(&fixture.socket, &fresh_id(6), "state.epoch", None);
    assert_eq!(epoch_after, epoch_before, "epoch unchanged");
    let grants_after = rpc_ok(&fixture.socket, &fresh_id(7), "grants.list", None);
    assert_eq!(
        grants_after, grants_before,
        "grants unchanged (none required)"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC5: PAUSED (held) refuses advancement and survives restart
// ---------------------------------------------------------------------------

#[test]
fn held_refuses_advancement_and_survives_restart() {
    let fixture = Fixture::new("ac5-held");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let replacement_id = request_replacement(&fixture.socket, 1, "ik_held-request-00000001");
    rpc_ok(
        &fixture.socket,
        &fresh_id(2),
        "lane.replacement.advance",
        Some(advance_params(
            &replacement_id,
            "requested",
            1,
            "ik_held-advance-00000001",
        )),
    );
    let held = rpc_ok(
        &fixture.socket,
        &fresh_id(3),
        "lane.replacement.hold",
        Some(hold_params(&replacement_id, "ik_held-hold-00000001")),
    );
    let held = replacement_of(&held).clone();
    assert_eq!(field_str(&held, "outcome"), "held");
    assert_eq!(field_str(&held, "phase"), "quiescing");
    assert_eq!(held.get("next_allowed"), Some(&null()));
    assert_eq!(field_str(&held, "outcome_reason"), "host drain window");

    // PAUSED refuses advancement with the typed park refusal.
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(4),
        "lane.replacement.advance",
        Some(advance_params(
            &replacement_id,
            "quiescing",
            1,
            "ik_held-advance-00000002",
        )),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");
    assert!(
        message.contains("host drain window"),
        "the park reason rides the refusal: {message}"
    );

    shutdown(daemon);

    // Restart: the held state is durable and still refuses advancement.
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let status = rpc_ok(
        &fixture.socket,
        &fresh_id(5),
        "lane.replacement.status",
        Some(status_params(&replacement_id)),
    );
    let replacement = replacement_of(&status);
    assert_eq!(field_str(replacement, "outcome"), "held");
    assert_eq!(
        field_str(replacement, "outcome_reason"),
        "host drain window"
    );
    assert_eq!(field_str(replacement, "phase"), "quiescing");
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(6),
        "lane.replacement.advance",
        Some(advance_params(
            &replacement_id,
            "quiescing",
            1,
            "ik_held-advance-00000003",
        )),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC5: cancellation before retirement preserves the original lane and
// invalidates the pending replacement
// ---------------------------------------------------------------------------

#[test]
fn cancellation_before_retirement_preserves_lane_and_invalidates() {
    let fixture = Fixture::new("ac5-cancel");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let replacement_id = request_replacement(&fixture.socket, 1, "ik_cancel-request-00000001");
    rpc_ok(
        &fixture.socket,
        &fresh_id(2),
        "lane.replacement.advance",
        Some(advance_params(
            &replacement_id,
            "requested",
            1,
            "ik_cancel-advance-00000001",
        )),
    );
    let before = rpc_ok(
        &fixture.socket,
        &fresh_id(3),
        "lane.replacement.status",
        Some(status_params(&replacement_id)),
    );
    let before = replacement_of(&before).clone();

    let cancelled = rpc_ok(
        &fixture.socket,
        &fresh_id(4),
        "lane.replacement.cancel",
        Some(cancel_params(&replacement_id, "ik_cancel-cancel-00000001")),
    );
    let cancelled = replacement_of(&cancelled);
    assert_eq!(field_str(cancelled, "outcome"), "cancelled");
    assert_eq!(
        field_str(cancelled, "phase"),
        "quiescing",
        "phase untouched"
    );
    // The original lane binding is preserved exactly.
    assert_eq!(
        cancelled
            .get("source")
            .and_then(|s| s.get("session"))
            .and_then(Val::as_str),
        Some("sess-0001")
    );
    assert_eq!(
        cancelled.get("lane_id").and_then(Val::as_str),
        before.get("lane_id").and_then(Val::as_str)
    );
    assert_eq!(
        cancelled.get("generation").and_then(Val::as_int),
        before.get("generation").and_then(Val::as_int)
    );
    assert_eq!(
        cancelled.get("successor_generation").and_then(Val::as_int),
        before.get("successor_generation").and_then(Val::as_int)
    );
    assert_eq!(cancelled.get("next_allowed"), Some(&null()));

    // The invalidated replacement can never advance.
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(5),
        "lane.replacement.advance",
        Some(advance_params(
            &replacement_id,
            "quiescing",
            1,
            "ik_cancel-advance-00000002",
        )),
    );
    assert_eq!(code, "refusal.replacement.invalidated", "{message}");

    // From retirement onward cancellation is too late: a second lane walks
    // to retired, then refuses to cancel (state unchanged).
    let second =
        request_replacement_for(&fixture.socket, 6, "lane-8", "ik_cancel-request-00000002");
    for (index, expected) in ["requested", "quiescing", "checkpointed"]
        .into_iter()
        .enumerate()
    {
        let key = format!("ik_cancel-chain-0000000{index}");
        rpc_ok(
            &fixture.socket,
            &fresh_id(7 + index as u32),
            "lane.replacement.advance",
            Some(advance_params(&second, expected, 1, &key)),
        );
    }
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(11),
        "lane.replacement.cancel",
        Some(cancel_params(&second, "ik_cancel-cancel-00000002")),
    );
    assert_eq!(code, "refusal.replacement.retired", "{message}");
    let status = rpc_ok(
        &fixture.socket,
        &fresh_id(12),
        "lane.replacement.status",
        Some(status_params(&second)),
    );
    let replacement = replacement_of(&status);
    assert_eq!(field_str(replacement, "phase"), "retired");
    assert_eq!(field_str(replacement, "outcome"), "pending");

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC6: daemon restart preserves the record, its event history and the
// precise next allowed transition
// ---------------------------------------------------------------------------

#[test]
fn restart_preserves_record_history_and_next_transition() {
    let fixture = Fixture::new("ac6-restart");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let replacement_id = request_replacement(&fixture.socket, 1, "ik_restart-request-00000001");
    for (index, expected) in ["requested", "quiescing"].into_iter().enumerate() {
        let key = format!("ik_restart-advance-0000000{index}");
        rpc_ok(
            &fixture.socket,
            &fresh_id(2 + index as u32),
            "lane.replacement.advance",
            Some(advance_params(&replacement_id, expected, 1, &key)),
        );
    }
    shutdown(daemon);

    // Restart on the same state directory: record, history and next allowed
    // transition are exactly preserved.
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let status = rpc_ok(
        &fixture.socket,
        &fresh_id(10),
        "lane.replacement.status",
        Some(status_params(&replacement_id)),
    );
    let replacement = replacement_of(&status);
    assert_eq!(field_str(replacement, "phase"), "checkpointed");
    assert_eq!(field_str(replacement, "outcome"), "pending");
    assert_eq!(next_allowed(replacement), Some("retired"));
    let history = status
        .get("history")
        .and_then(|value| value.as_array())
        .expect("history");
    let transitions: Vec<(Option<&str>, &str)> = history
        .iter()
        .map(|event| {
            (
                event.get("from_phase").and_then(Val::as_str),
                field_str(event, "to_phase"),
            )
        })
        .collect();
    assert_eq!(
        transitions,
        vec![
            (None, "requested"),
            (Some("requested"), "quiescing"),
            (Some("quiescing"), "checkpointed"),
        ],
        "history is preserved in order"
    );

    // The preserved next transition still works after the restart.
    let advanced = rpc_ok(
        &fixture.socket,
        &fresh_id(11),
        "lane.replacement.advance",
        Some(advance_params(
            &replacement_id,
            "checkpointed",
            1,
            "ik_restart-advance-00000002",
        )),
    );
    assert_eq!(field_str(replacement_of(&advanced), "phase"), "retired");

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// Interrupted transition: restart reconciliation writes the explicit
// `ambiguous` outcome and advancement refuses until external reconciliation
// ---------------------------------------------------------------------------

#[test]
fn interrupted_advance_reconciles_ambiguous_and_refuses() {
    let fixture = Fixture::new("ambiguous");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let replacement_id = request_replacement(&fixture.socket, 1, "ik_amb-request-00000001");
    shutdown(daemon);

    // Crash the daemon after the advance intent is journaled but before the
    // record transition lands.
    let request_id = fresh_id(20);
    let advance_key = "ik_amb-advance-00000001";
    let crashed = fixture.spawn(Some("lane-replacement.after-intent"));
    wait_ready(&fixture);
    send_only(
        &fixture.socket,
        &request_id,
        "lane.replacement.advance",
        advance_params(&replacement_id, "requested", 1, advance_key),
    );
    wait_exit(crashed, "crash at lane-replacement.after-intent");

    // Restart: the claim is reconciled ambiguous before any retry works...
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let (code, message) = rpc_err(
        &fixture.socket,
        &request_id,
        "lane.replacement.advance",
        Some(advance_params(&replacement_id, "requested", 1, advance_key)),
    );
    assert_eq!(code, "state.ambiguous_claim", "{message}");

    // ... and the record itself carries the explicit ambiguous outcome.
    let status = rpc_ok(
        &fixture.socket,
        &fresh_id(21),
        "lane.replacement.status",
        Some(status_params(&replacement_id)),
    );
    let replacement = replacement_of(&status);
    assert_eq!(field_str(replacement, "outcome"), "ambiguous");
    assert_eq!(field_str(replacement, "phase"), "requested");
    assert_eq!(replacement.get("next_allowed"), Some(&null()));
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(22),
        "lane.replacement.advance",
        Some(advance_params(
            &replacement_id,
            "requested",
            1,
            "ik_amb-advance-00000002",
        )),
    );
    assert_eq!(code, "refusal.replacement.ambiguous", "{message}");

    // Reconciliation is journaled.
    let tail = rpc_ok(
        &fixture.socket,
        &fresh_id(23),
        "journal.tail",
        Some(object(vec![
            ("after_seq", integer(0)),
            ("limit", integer(500)),
        ])),
    );
    let actions: Vec<&str> = tail
        .get("records")
        .and_then(|value| value.as_array())
        .expect("records")
        .iter()
        .filter_map(|record| record.get("action").and_then(Val::as_str))
        .collect();
    assert!(
        actions.contains(&"reconcile.lane.replacement.advance"),
        "restart reconciliation journaled: {actions:?}"
    );

    shutdown(daemon);
}
