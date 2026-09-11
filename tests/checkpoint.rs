//! Issue #74 acceptance integration tests over the real binary and socket:
//! the safe-boundary checkpoint operation — the quiescing fence, the
//! supported quiescence acknowledgment + process/child observation gate for
//! active external harness execution, the typed holds (active/ambiguous
//! side-effecting children; oversize required data), the two-observation
//! consistency rule, orchestrator references that never alter referenced
//! lanes, the atomic record+brief commit across restarts, and byte-for-byte
//! preservation of a dirty worktree.
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
        let dir = std::env::temp_dir().join(format!("hf-ck-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        Fixture {
            state_dir: dir.join("state"),
            socket: dir.join("daemon.sock"),
            dir,
        }
    }

    /// The daemon-owned checkpoint brief directory (`XDG_STATE_HOME` /
    /// `herdr-fleet` / `checkpoints`).
    fn checkpoints_dir(&self) -> PathBuf {
        self.state_dir.join("herdr-fleet").join("checkpoints")
    }

    fn daemon_log(&self) -> PathBuf {
        self.state_dir.join("herdr-fleet").join("daemon.log")
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

const WORKTREE: &str = "worktrees/issues/74";
const REPLACEMENT_WORKTREE: &str = "worktrees/issues/74";

fn replacement_params(lane: &str, role: &str, key: &str) -> Val {
    object(vec![
        ("idempotency_key", string(key)),
        ("lane_id", string(lane)),
        ("generation", integer(1)),
        ("source_session", string("sess-0001")),
        ("source_process", string("proc-0001")),
        ("role", string(role)),
        ("worktree", string(REPLACEMENT_WORKTREE)),
        ("reason", string("handoff checkpoint window")),
    ])
}

/// A valid synthetic observation for one lane: every required field present
/// (the checkpoint refuses missing evidence instead of omitting it).
fn observation(role: &str) -> Val {
    object(vec![
        ("role", string(role)),
        ("task", string("issue-74 checkpoint capture")),
        ("worktree", string(WORKTREE)),
        ("branch", string("issue-74-checkpoint")),
        ("head", string(&"a".repeat(40))),
        ("base", string(&"b".repeat(40))),
        (
            "dirty",
            object(vec![
                ("count", integer(3)),
                ("digest", string(&"c".repeat(64))),
            ]),
        ),
        (
            "untracked",
            object(vec![
                ("count", integer(2)),
                ("digest", string(&"d".repeat(64))),
            ]),
        ),
        (
            "report",
            object(vec![
                ("round", integer(2)),
                ("reviewed_sha", string(&"e".repeat(40))),
            ]),
        ),
        (
            "gates",
            Val::Arr(vec![
                object(vec![
                    ("name", string("focused")),
                    ("status", string("pending")),
                ]),
                object(vec![("name", string("full")), ("status", string("passed"))]),
            ]),
        ),
        (
            "children",
            Val::Arr(vec![object(vec![
                ("command", string("cargo test --locked")),
                ("state", string("exited")),
            ])]),
        ),
        (
            "execution",
            object(vec![("active", bool_(false)), ("ack", null())]),
        ),
    ])
}

fn session_ack(at: &str) -> Val {
    object(vec![
        ("kind", string("session-quiesced")),
        ("session", string("sess-0001")),
        ("at", string(at)),
    ])
}

fn edit_observation(mut observation: Val, key: &str, value: Option<Val>) -> Val {
    if let Val::Obj(map) = &mut observation {
        match value {
            Some(value) => {
                map.insert(key.to_string(), value);
            }
            None => {
                map.remove(key);
            }
        }
    }
    observation
}

fn edit_observation_nested(
    mut observation: Val,
    outer: &str,
    key: &str,
    value: Option<Val>,
) -> Val {
    if let Val::Obj(map) = &mut observation
        && let Some(Val::Obj(inner)) = map.get_mut(outer)
    {
        match value {
            Some(value) => {
                inner.insert(key.to_string(), value);
            }
            None => {
                inner.remove(key);
            }
        }
    }
    observation
}

fn checkpoint_params(
    replacement_id: &str,
    generation: i64,
    observation: &Val,
    reobservation: &Val,
    key: &str,
) -> Val {
    object(vec![
        ("idempotency_key", string(key)),
        ("replacement_id", string(replacement_id)),
        ("generation", integer(generation)),
        ("observation", observation.clone()),
        ("reobservation", reobservation.clone()),
    ])
}

fn checkpoint_of(result: &Val) -> &Val {
    result.get("checkpoint").expect("checkpoint")
}

fn field_str<'a>(val: &'a Val, key: &str) -> &'a str {
    val.get(key).and_then(Val::as_str).expect(key)
}

fn replacement_of(result: &Val) -> &Val {
    result.get("replacement").expect("replacement")
}

fn request_replacement(socket: &Path, id_seed: u32, lane: &str, role: &str, key: &str) -> String {
    let result = rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.request",
        Some(replacement_params(lane, role, key)),
    );
    field_str(replacement_of(&result), "replacement_id").to_string()
}

fn advance_to_quiescing(socket: &Path, id_seed: u32, replacement_id: &str, key: &str) -> Val {
    rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.advance",
        Some(object(vec![
            ("idempotency_key", string(key)),
            ("replacement_id", string(replacement_id)),
            ("expected_phase", string("requested")),
            ("generation", integer(1)),
        ])),
    )
}

fn status_of(socket: &Path, id_seed: u32, replacement_id: &str) -> Val {
    rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.status",
        Some(object(vec![("replacement_id", string(replacement_id))])),
    )
}

// ---------------------------------------------------------------------------
// AC1 (honest scope): the checkpoint surface needs no external tool and no
// authority uplift — daemon fencing is the only claim, never a claim that
// arbitrary shell actions stop.
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_surface_needs_no_external_tools_and_no_authority() {
    let fixture = Fixture::new("no-tools");
    let daemon = fixture.spawn_with(None, PathMode::WithoutTools);
    wait_ready(&fixture);

    let epoch_before = rpc_ok(&fixture.socket, &fresh_id(1), "state.epoch", None);
    let grants_before = rpc_ok(&fixture.socket, &fresh_id(2), "grants.list", None);

    let replacement_id = request_replacement(
        &fixture.socket,
        3,
        "lane-7",
        "implementer",
        "ik_ck-ro-00000001",
    );
    advance_to_quiescing(&fixture.socket, 4, &replacement_id, "ik_ck-ro-advance-0001");
    let observation = observation("implementer");
    let result = rpc_ok(
        &fixture.socket,
        &fresh_id(5),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &observation,
            &observation,
            "ik_ck-ro-capture-0001",
        )),
    );
    let checkpoint = checkpoint_of(&result);
    assert_eq!(field_str(checkpoint, "role"), "implementer");
    assert_eq!(
        field_str(checkpoint, "observation_digest"),
        field_str(checkpoint, "reobservation_digest")
    );
    let snapshot = checkpoint.get("snapshot").expect("snapshot");
    assert_eq!(
        snapshot.get("task").and_then(Val::as_str),
        Some("issue-74 checkpoint capture")
    );
    assert_eq!(
        snapshot.get("worktree").and_then(Val::as_str),
        Some(WORKTREE)
    );
    // The brief is a bounded derivation carrying explicit evidence pointers.
    let brief = result.get("brief").and_then(Val::as_str).expect("brief");
    assert!(brief.len() <= 3072, "brief is bounded");
    assert!(
        brief.contains("evidence: snapshot sha256"),
        "evidence pointer: {brief}"
    );
    assert!(brief.contains(&format!(
        "record: lane_checkpoints/{}",
        field_str(checkpoint, "checkpoint_id")
    )));
    assert!(!brief.contains("sess-0002"), "no other identities leak in");
    let brief_path = PathBuf::from(field_str(checkpoint, "brief_path"));
    assert!(brief_path.exists(), "brief artifact materialized");

    // No authority uplift: no grants required/issued/consumed, epoch intact.
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
// AC1: quiescing is the fence — checkpoint capture is only admitted at the
// quiescing boundary, and new replacement slots for the lane are fenced until
// the handoff resolves or is cancelled. No second capture can exist.
// ---------------------------------------------------------------------------

#[test]
fn quiescing_boundary_gates_capture_and_fences_new_generations() {
    let fixture = Fixture::new("fence");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let replacement_id = request_replacement(
        &fixture.socket,
        1,
        "lane-7",
        "implementer",
        "ik_ck-fence-req-00001",
    );
    let observation = observation("implementer");

    // Before quiescing the boundary is not up: the capture refuses (the
    // fence is what admits it).
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(2),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &observation,
            &observation,
            "ik_ck-fence-early-0001",
        )),
    );
    assert_eq!(code, "refusal.replacement.order", "{message}");

    advance_to_quiescing(&fixture.socket, 3, &replacement_id, "ik_ck-fence-adv-0001");

    // Inside the window: a new generation slot for the lane is fenced...
    let gen2_params = replacement_params("lane-7", "implementer", "ik_ck-fence-gen2-0001");
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(4),
        "lane.replacement.request",
        Some(edit_observation(
            gen2_params,
            "generation",
            Some(integer(2)),
        )),
    );
    assert_eq!(code, "refusal.replacement.fenced", "{message}");
    assert!(
        message.contains(&replacement_id),
        "the fence names the holding record: {message}"
    );
    // ...while other lanes are unaffected.
    request_replacement(
        &fixture.socket,
        5,
        "lane-8",
        "implementer",
        "ik_ck-fence-l8-00001",
    );

    // A stale generation on the capture refuses; the exact generation and
    // the quiescing phase commit.
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(6),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            9,
            &observation,
            &observation,
            "ik_ck-fence-stale-0001",
        )),
    );
    assert_eq!(code, "refusal.replacement.stale", "{message}");
    let result = rpc_ok(
        &fixture.socket,
        &fresh_id(7),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &observation,
            &observation,
            "ik_ck-fence-capture-001",
        )),
    );
    assert_eq!(field_str(replacement_of(&result), "phase"), "checkpointed");
    assert_eq!(
        replacement_of(&result)
            .get("next_allowed")
            .and_then(Val::as_str),
        Some("retired")
    );

    // One replacement carries at most one capture; the replay of the same
    // request (same id + key) returns the recorded response byte-identically.
    let replay = rpc_ok(
        &fixture.socket,
        &fresh_id(7),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &observation,
            &observation,
            "ik_ck-fence-capture-001",
        )),
    );
    assert_eq!(
        replay, result,
        "the idempotent replay returns the recorded response"
    );
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(8),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &observation,
            &observation,
            "ik_ck-fence-capture-002",
        )),
    );
    assert_eq!(code, "refusal.checkpoint.exists", "{message}");

    // The window stays up through `checkpointed` and lifts on cancellation.
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(9),
        "lane.replacement.request",
        Some(edit_observation(
            replacement_params("lane-7", "implementer", "ik_ck-fence-gen2-0002"),
            "generation",
            Some(integer(2)),
        )),
    );
    assert_eq!(code, "refusal.replacement.fenced", "{message}");
    rpc_ok(
        &fixture.socket,
        &fresh_id(10),
        "lane.replacement.cancel",
        Some(object(vec![
            ("idempotency_key", string("ik_ck-fence-cancel-001")),
            ("replacement_id", string(&replacement_id)),
        ])),
    );
    let gen2_params = replacement_params("lane-7", "implementer", "ik_ck-fence-gen2-0003");
    let successor = rpc_ok(
        &fixture.socket,
        &fresh_id(11),
        "lane.replacement.request",
        Some(edit_observation(
            gen2_params,
            "generation",
            Some(integer(2)),
        )),
    );
    assert_eq!(
        replacement_of(&successor)
            .get("generation")
            .and_then(Val::as_int),
        Some(2),
        "the fence lifted after cancellation"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC1/AC2: the acknowledgment + child observation gate (active external
// harness execution) and the typed holds for side-effecting children.
// ---------------------------------------------------------------------------

#[test]
fn acknowledgment_and_child_observation_gate_active_execution() {
    let fixture = Fixture::new("ack");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let replacement_id = request_replacement(
        &fixture.socket,
        1,
        "lane-7",
        "implementer",
        "ik_ck-ack-req-000001",
    );
    advance_to_quiescing(&fixture.socket, 2, &replacement_id, "ik_ck-ack-adv-000001");

    let base = observation("implementer");
    let cases: Vec<(&str, Val, &str)> = vec![
        (
            "active execution without an acknowledgment",
            edit_observation(
                base.clone(),
                "execution",
                Some(object(vec![("active", bool_(true)), ("ack", null())])),
            ),
            "refusal.checkpoint.ack",
        ),
        (
            "unsupported acknowledgment kind",
            edit_observation(
                base.clone(),
                "execution",
                Some(object(vec![
                    ("active", bool_(true)),
                    (
                        "ack",
                        object(vec![
                            ("kind", string("pane-scraped")),
                            ("session", string("sess-0001")),
                            ("at", string("2026-09-12T00:00:00Z")),
                        ]),
                    ),
                ])),
            ),
            "refusal.checkpoint.ack",
        ),
        (
            "acknowledgment while execution is inactive",
            edit_observation(
                base.clone(),
                "execution",
                Some(object(vec![
                    ("active", bool_(false)),
                    ("ack", session_ack("2026-09-12T00:00:00Z")),
                ])),
            ),
            "refusal.checkpoint.ack",
        ),
        (
            "active side-effecting child command",
            edit_observation(
                base.clone(),
                "children",
                Some(Val::Arr(vec![object(vec![
                    ("command", string("git push origin issue-74-checkpoint")),
                    ("state", string("active")),
                ])])),
            ),
            "refusal.checkpoint.held",
        ),
        (
            "ambiguous side-effecting child command",
            edit_observation(
                base.clone(),
                "children",
                Some(Val::Arr(vec![object(vec![
                    ("command", string("cargo build --release")),
                    ("state", string("ambiguous")),
                ])])),
            ),
            "refusal.checkpoint.held",
        ),
    ];
    for (index, (label, observation, expected)) in cases.into_iter().enumerate() {
        let key = format!("ik_ck-ack-case-{index:06}");
        let (code, message) = rpc_err(
            &fixture.socket,
            &fresh_id(10 + index as u32),
            "lane.checkpoint.create",
            Some(checkpoint_params(
                &replacement_id,
                1,
                &observation,
                &observation,
                &key,
            )),
        );
        assert_eq!(code, expected, "{label}: {message}");
    }
    // Held/refused captures commit nothing: the record is untouched.
    let status = status_of(&fixture.socket, 30, &replacement_id);
    assert_eq!(field_str(replacement_of(&status), "phase"), "quiescing");
    assert_eq!(field_str(replacement_of(&status), "outcome"), "pending");

    // A supported acknowledgment with an observed (exited) child commits and
    // records the acknowledgment in the durable snapshot.
    let supported = edit_observation(
        base,
        "execution",
        Some(object(vec![
            ("active", bool_(true)),
            ("ack", session_ack("2026-09-12T00:00:00Z")),
        ])),
    );
    let result = rpc_ok(
        &fixture.socket,
        &fresh_id(31),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &supported,
            &supported,
            "ik_ck-ack-valid-00001",
        )),
    );
    let snapshot = checkpoint_of(&result).get("snapshot").expect("snapshot");
    assert_eq!(
        snapshot
            .get("execution")
            .and_then(|value| value.get("active"))
            .and_then(Val::as_bool),
        Some(true)
    );
    assert_eq!(
        snapshot
            .get("execution")
            .and_then(|value| value.get("ack"))
            .and_then(|ack| ack.get("kind"))
            .and_then(Val::as_str),
        Some("session-quiesced")
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC5/AC6: two observations must agree; missing evidence refuses; oversize
// required data yields a typed hold (never a truncation).
// ---------------------------------------------------------------------------

#[test]
fn changed_views_missing_evidence_and_oversize_refuse_the_capture() {
    let fixture = Fixture::new("two-obs");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let replacement_id = request_replacement(
        &fixture.socket,
        1,
        "lane-7",
        "implementer",
        "ik_ck-two-req-000001",
    );
    advance_to_quiescing(&fixture.socket, 2, &replacement_id, "ik_ck-two-adv-000001");

    let base = observation("implementer");
    // The second observation disagrees (the lane changed during capture).
    let changed_reobs = edit_observation(base.clone(), "head", Some(string(&"f".repeat(40))));
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(3),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &base,
            &changed_reobs,
            "ik_ck-two-changed-0001",
        )),
    );
    assert_eq!(code, "refusal.checkpoint.changed", "{message}");

    // Missing evidence refuses: the dirty-inventory integrity digest is
    // required — it is never silently omitted.
    let missing_digest = edit_observation_nested(base.clone(), "dirty", "digest", None);
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(4),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &missing_digest,
            &missing_digest,
            "ik_ck-two-missing-0001",
        )),
    );
    assert_eq!(code, "refusal.checkpoint.incomplete", "{message}");
    assert!(message.contains("digest"), "{message}");

    // Oversize required data (the maximal recorded child list) is a typed
    // hold: nothing is truncated and nothing commits.
    let long_children: Vec<Val> = (0..16)
        .map(|_| {
            object(vec![
                ("command", string(&"x".repeat(160))),
                ("state", string("exited")),
            ])
        })
        .collect();
    let oversize = edit_observation(
        edit_observation(base.clone(), "task", Some(string(&"t".repeat(200)))),
        "children",
        Some(Val::Arr(long_children)),
    );
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(5),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &oversize,
            &oversize,
            "ik_ck-two-oversize-001",
        )),
    );
    assert_eq!(code, "refusal.checkpoint.oversize", "{message}");
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(6),
        "lane.checkpoint.status",
        Some(object(vec![("replacement_id", string(&replacement_id))])),
    );
    assert_eq!(code, "state.not_found", "{message}");

    // A consistent, complete, bounded observation still commits afterwards.
    let result = rpc_ok(
        &fixture.socket,
        &fresh_id(7),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &base,
            &base,
            "ik_ck-two-valid-000001",
        )),
    );
    assert_eq!(
        field_str(checkpoint_of(&result), "observation_digest"),
        field_str(checkpoint_of(&result), "reobservation_digest")
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC3/AC4: the snapshot binds the lane facts; orchestrator checkpoints
// reference existing worker/reviewer identities and pending completion
// events without altering those lanes.
// ---------------------------------------------------------------------------

#[test]
fn orchestrator_checkpoint_references_lanes_without_altering_them() {
    let fixture = Fixture::new("orchestrator");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let orchestrator = request_replacement(
        &fixture.socket,
        1,
        "lane-orch",
        "orchestrator",
        "ik_ck-orch-req-00001",
    );
    advance_to_quiescing(&fixture.socket, 2, &orchestrator, "ik_ck-orch-adv-00001");
    let worker = request_replacement(
        &fixture.socket,
        3,
        "lane-8",
        "implementer",
        "ik_ck-orch-worker-001",
    );
    let reviewer = request_replacement(
        &fixture.socket,
        4,
        "lane-9",
        "reviewer",
        "ik_ck-orch-review-001",
    );

    let worker_before = status_of(&fixture.socket, 5, &worker);
    let reviewer_before = status_of(&fixture.socket, 6, &reviewer);

    let mut orchestrator_observation = observation("orchestrator");
    if let Val::Obj(map) = &mut orchestrator_observation {
        map.insert(
            "orchestration".to_string(),
            object(vec![
                ("workers", Val::Arr(vec![string(&worker)])),
                ("reviewers", Val::Arr(vec![string(&reviewer)])),
                (
                    "pending_events",
                    Val::Arr(vec![string("worker-finished:lane-8")]),
                ),
            ]),
        );
    }
    let result = rpc_ok(
        &fixture.socket,
        &fresh_id(7),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &orchestrator,
            1,
            &orchestrator_observation,
            &orchestrator_observation,
            "ik_ck-orch-capture-01",
        )),
    );
    let snapshot = checkpoint_of(&result).get("snapshot").expect("snapshot");
    let orchestration = snapshot.get("orchestration").expect("orchestration");
    assert_eq!(
        orchestration
            .get("workers")
            .and_then(Val::as_array)
            .and_then(|items| items.first())
            .and_then(Val::as_str),
        Some(worker.as_str())
    );
    assert_eq!(
        orchestration
            .get("pending_events")
            .and_then(Val::as_array)
            .and_then(|items| items.first())
            .and_then(Val::as_str),
        Some("worker-finished:lane-8")
    );

    // Referencing lanes never alters them: the full status documents
    // (record + history) are byte-identical before and after.
    assert_eq!(status_of(&fixture.socket, 8, &worker), worker_before);
    assert_eq!(status_of(&fixture.socket, 9, &reviewer), reviewer_before);

    // Bogus references refuse typed; so does a role-mismatched reference.
    let reviewer_as_worker = {
        let mut bogus = observation("orchestrator");
        if let Val::Obj(map) = &mut bogus {
            map.insert(
                "orchestration".to_string(),
                object(vec![
                    ("workers", Val::Arr(vec![string(&reviewer)])),
                    ("reviewers", Val::Arr(vec![])),
                    ("pending_events", Val::Arr(vec![])),
                ]),
            );
        }
        bogus
    };
    let second_orch = request_replacement(
        &fixture.socket,
        10,
        "lane-orch-2",
        "orchestrator",
        "ik_ck-orch2-req-0001",
    );
    advance_to_quiescing(&fixture.socket, 11, &second_orch, "ik_ck-orch2-adv-0002");
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(12),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &second_orch,
            1,
            &reviewer_as_worker,
            &reviewer_as_worker,
            "ik_ck-orch2-bogus-001",
        )),
    );
    assert_eq!(code, "refusal.checkpoint.references", "{message}");

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC7: the checkpoint commit is atomic across restarts — a restart yields
// either the previous complete checkpoint or the new complete one; a dirty
// worktree is preserved byte-for-byte.
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_commit_is_atomic_across_restarts_and_preserves_dirty_bytes() {
    let fixture = Fixture::new("atomic");

    // A synthetic dirty worktree (with untracked files): its bytes must not
    // change across the whole capture/restart cycle.
    let worktree = fixture.dir.join("lane-worktree");
    std::fs::create_dir_all(&worktree).expect("worktree");
    std::fs::write(worktree.join("dirty.txt"), b"uncommitted change\n").expect("dirty");
    std::fs::write(worktree.join("untracked.txt"), b"untracked file\n").expect("untracked");
    let fingerprint = |dir: &Path| -> Vec<String> {
        let mut entries: Vec<String> = std::fs::read_dir(dir)
            .expect("read worktree")
            .map(|entry| {
                let entry = entry.expect("entry");
                let bytes = std::fs::read(entry.path()).expect("bytes");
                format!(
                    "{}:{}",
                    entry.file_name().to_string_lossy(),
                    herdr_fleet::canonical::sha256_hex(&bytes)
                )
            })
            .collect();
        entries.sort();
        entries
    };
    let before = fingerprint(&worktree);

    // Scenario A: crash after the checkpoint intent is journaled, before the
    // commit — the restart yields the PREVIOUS complete state (no checkpoint,
    // no brief artifact, the record still at the quiescing boundary) and the
    // capture retries cleanly with a fresh key.
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let replacement_id = request_replacement(
        &fixture.socket,
        1,
        "lane-7",
        "implementer",
        "ik_ck-at-req-000001",
    );
    shutdown(daemon);

    let crashed = fixture.spawn(Some("lane-checkpoint.after-intent"));
    wait_ready(&fixture);
    let observation = observation("implementer");
    send_only(
        &fixture.socket,
        &fresh_id(2),
        "lane.checkpoint.create",
        checkpoint_params(
            &replacement_id,
            1,
            &observation,
            &observation,
            "ik_ck-at-capture-0001",
        ),
    );
    wait_exit(crashed, "crash at lane-checkpoint.after-intent");

    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(3),
        "lane.checkpoint.status",
        Some(object(vec![("replacement_id", string(&replacement_id))])),
    );
    assert_eq!(code, "state.not_found", "{message}");
    assert_eq!(
        std::fs::read_dir(fixture.checkpoints_dir())
            .expect("checkpoints dir")
            .count(),
        0,
        "no brief artifact exists after the pre-commit crash"
    );
    let status = status_of(&fixture.socket, 4, &replacement_id);
    assert_eq!(field_str(replacement_of(&status), "phase"), "requested");
    assert_eq!(field_str(replacement_of(&status), "outcome"), "pending");
    // The interrupted claim requires a fresh key; the retry completes.
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(2),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &observation,
            &observation,
            "ik_ck-at-capture-0001",
        )),
    );
    assert_eq!(code, "state.ambiguous_claim", "{message}");
    advance_to_quiescing(&fixture.socket, 5, &replacement_id, "ik_ck-at-adv-000001");
    rpc_ok(
        &fixture.socket,
        &fresh_id(6),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &observation,
            &observation,
            "ik_ck-at-capture-0002",
        )),
    );
    shutdown(daemon);

    // Scenario B: crash after the commit, before the brief artifact is
    // materialized — the restart yields the NEW COMPLETE checkpoint: the
    // record is checkpointed, the brief is regenerated from the durable row,
    // and its bytes verify against the digest bound at commit time.
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let second = request_replacement(
        &fixture.socket,
        7,
        "lane-8",
        "implementer",
        "ik_ck-at-req-000002",
    );
    advance_to_quiescing(&fixture.socket, 8, &second, "ik_ck-at-adv-000002");
    shutdown(daemon);

    let crashed = fixture.spawn(Some("lane-checkpoint.after-record"));
    wait_ready(&fixture);
    send_only(
        &fixture.socket,
        &fresh_id(9),
        "lane.checkpoint.create",
        checkpoint_params(
            &second,
            1,
            &observation,
            &observation,
            "ik_ck-at-capture-0003",
        ),
    );
    wait_exit(crashed, "crash at lane-checkpoint.after-record");

    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let status = status_of(&fixture.socket, 10, &second);
    assert_eq!(field_str(replacement_of(&status), "phase"), "checkpointed");
    let record = rpc_ok(
        &fixture.socket,
        &fresh_id(11),
        "lane.checkpoint.status",
        Some(object(vec![("replacement_id", string(&second))])),
    );
    let checkpoint = checkpoint_of(&record);
    let brief_path = PathBuf::from(field_str(checkpoint, "brief_path"));
    let brief_bytes = std::fs::read(&brief_path).expect("regenerated brief artifact");
    assert_eq!(
        herdr_fleet::canonical::sha256_hex(&brief_bytes),
        field_str(checkpoint, "brief_digest"),
        "the regenerated brief verifies against the digest bound at commit time"
    );
    let log_text = std::fs::read_to_string(fixture.daemon_log()).expect("daemon log");
    assert!(
        log_text.contains("reconcile.lane.checkpoint"),
        "the restart reconciliation is logged"
    );

    // Scenario C: an artifact without a committed record (only a non-atomic
    // implementation produces this) fails closed on restart: the record is
    // parked ambiguous instead of adopting or silently deleting it.
    let third = request_replacement(
        &fixture.socket,
        12,
        "lane-9",
        "implementer",
        "ik_ck-at-req-000003",
    );
    advance_to_quiescing(&fixture.socket, 13, &third, "ik_ck-at-adv-000003");
    shutdown(daemon);

    let crashed = fixture.spawn(Some("lane-checkpoint.after-intent"));
    wait_ready(&fixture);
    send_only(
        &fixture.socket,
        &fresh_id(14),
        "lane.checkpoint.create",
        checkpoint_params(
            &third,
            1,
            &observation,
            &observation,
            "ik_ck-at-capture-0004",
        ),
    );
    wait_exit(crashed, "crash at lane-checkpoint.after-intent");
    // Plant an orphan artifact at the deterministic path before restart.
    let orphan = fixture.checkpoints_dir().join(format!(
        "{}.brief",
        herdr_fleet::state::checkpoint_id_for(&third)
    ));
    std::fs::write(&orphan, b"orphan artifact without a committed record\n").expect("plant orphan");

    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let status = status_of(&fixture.socket, 15, &third);
    assert_eq!(
        field_str(replacement_of(&status), "outcome"),
        "ambiguous",
        "an artifact without a commit fails closed"
    );
    assert_eq!(
        replacement_of(&status).get("next_allowed"),
        Some(&null()),
        "the parked record cannot advance"
    );
    let log_text = std::fs::read_to_string(fixture.daemon_log()).expect("daemon log");
    assert!(
        log_text.contains("without a committed"),
        "the inconsistency is logged"
    );

    // The dirty worktree is byte-for-byte preserved through all of it.
    assert_eq!(
        fingerprint(&worktree),
        before,
        "dirty worktree bytes preserved"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// Checkpoint status is read-only (no claim, no key) and durable across a
// restart, byte-identical.
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_status_is_read_only_and_durable() {
    let fixture = Fixture::new("status");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let replacement_id = request_replacement(
        &fixture.socket,
        1,
        "lane-7",
        "implementer",
        "ik_ck-status-req-001",
    );
    advance_to_quiescing(&fixture.socket, 2, &replacement_id, "ik_ck-status-adv-001");
    let observation = observation("implementer");
    rpc_ok(
        &fixture.socket,
        &fresh_id(3),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            1,
            &observation,
            &observation,
            "ik_ck-status-cap-001",
        )),
    );
    // Read-only: no idempotency key is required or consumed.
    let first = rpc_ok(
        &fixture.socket,
        &fresh_id(4),
        "lane.checkpoint.status",
        Some(object(vec![("replacement_id", string(&replacement_id))])),
    );
    let again = rpc_ok(
        &fixture.socket,
        &fresh_id(5),
        "lane.checkpoint.status",
        Some(object(vec![("replacement_id", string(&replacement_id))])),
    );
    assert_eq!(first, again, "status reads are deterministic");
    shutdown(daemon);

    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let after_restart = rpc_ok(
        &fixture.socket,
        &fresh_id(6),
        "lane.checkpoint.status",
        Some(object(vec![("replacement_id", string(&replacement_id))])),
    );
    assert_eq!(
        after_restart, first,
        "the checkpoint is durable across restarts"
    );

    shutdown(daemon);
}
