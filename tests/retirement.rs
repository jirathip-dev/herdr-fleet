//! Issue #75 acceptance integration tests over the real binary and socket:
//! the guarded retirement of ONE checkpointed source session — the binding
//! (lane generation, source session/process identity, committed checkpoint
//! digest) checked BEFORE any effect, the immediate pre-stop quiescence
//! recheck, the ONE bounded graceful stop through the workspace (Herdr)
//! adapter path, the backend-evidence confirmation (absent process AND
//! released ownership registration — never a pane text or a `done` label),
//! the preservation of child lanes and worktree bytes, and the
//! crash-after-stop reconciliation that reconciles exact absence without
//! ever repeating a signal.
//!
//! Every test spawns `canter daemon run` as a child process with
//! isolated XDG state and an explicit socket under a per-test temp dir. The
//! workspace executable the retirement drives is a fake `herdr` recorded in a
//! per-fixture invocation log, on a PATH that contains nothing else (the
//! allowlisted adapter environment is the only channel). Nothing here touches
//! the real host state, a real session, the service manager, or the network;
//! all identities are synthetic.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use canter::client::{Connection, RpcError};
use canter::value::{Val, bool_, integer, null, object, string};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_canter")
}

/// Default per-test daemon readiness deadline.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// The fake workspace executable's two retirement rows need a shell utility
/// path of their own: the daemon passes an allowlisted environment, so the
/// script sets PATH explicitly and only ever uses /usr/bin and /bin tools.
const FAKE_WORKSPACE: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin
log="$HOME/workspace-invocations.log"
printf '%s\n' "$*" >> "$log"
mode="$(cat "$HOME/workspace-mode" 2>/dev/null || echo live)"
case "$1 $2" in
  "session interrupt")
    case "$mode" in
      stop-fail) echo '{"error":"refused"}' >&2; exit 3 ;;
      stop-sleep) exec sleep 30 ;;
      *) printf '%s\n' '{"interrupted":true}' ;;
    esac ;;
  "session show")
    case "$mode" in
      live) doc='{"session_id":"sess-0001","state":"working","process":"proc-0001","registration":{"state":"active","session":"sess-0001","generation":1}}' ;;
      retired) doc='{"session_id":"sess-0001","state":"retired","process":null,"registration":{"state":"released","session":"sess-0001","generation":1}}' ;;
      label-only) doc='{"session_id":"sess-0001","state":"done","process":"proc-0001","registration":{"state":"released","session":"sess-0001","generation":1}}' ;;
      no-process) doc='{"session_id":"sess-0001","registration":{"state":"released","session":"sess-0001","generation":1}}' ;;
      reused-process) doc='{"session_id":"sess-0001","process":"proc-0009","registration":{"state":"released","session":"sess-0001","generation":1}}' ;;
      stale-registration) doc='{"session_id":"sess-0001","process":null,"registration":{"state":"released","session":"sess-0001","generation":2}}' ;;
      malformed) doc='not-json' ;;
      *) doc='{"session_id":"sess-0001","state":"working","process":"proc-0001","registration":{"state":"active","session":"sess-0001","generation":1}}' ;;
    esac
    printf '%s\n' "$doc" ;;
  *) echo '{"error":"unknown row"}' >&2; exit 4 ;;
esac
"#;

struct Fixture {
    dir: PathBuf,
    state_dir: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("hf-rt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        Fixture {
            state_dir: dir.join("state"),
            socket: dir.join("daemon.sock"),
            dir,
        }
    }

    fn daemon_log(&self) -> PathBuf {
        self.state_dir.join("canter").join("daemon.log")
    }

    fn bin_dir(&self) -> PathBuf {
        self.dir.join("bin")
    }

    fn invocations_log(&self) -> PathBuf {
        self.dir.join("workspace-invocations.log")
    }

    /// Every workspace (fake `herdr`) invocation recorded so far, in order.
    fn invocations(&self) -> Vec<String> {
        std::fs::read_to_string(self.invocations_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Select the fake workspace behavior (and let the test switch it
    /// between a crash and the restart that reconciles it).
    fn set_mode(&self, mode: &str) {
        std::fs::write(self.dir.join("workspace-mode"), format!("{mode}\n"))
            .expect("write workspace mode");
    }

    fn write_fake_workspace(&self) {
        let bin_dir = self.bin_dir();
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let path = bin_dir.join("herdr");
        std::fs::write(&path, FAKE_WORKSPACE).expect("write fake workspace");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
        }
        std::fs::set_permissions(&path, permissions).expect("chmod");
    }

    /// Spawn the daemon with the fake workspace executable as the only
    /// `herdr` on PATH.
    fn spawn(&self, crash_point: Option<&str>) -> Child {
        self.spawn_with_path(crash_point, PathMode::FakeWorkspace)
    }

    /// Spawn the daemon with a PATH that contains nothing: the retirement
    /// stop row cannot resolve the workspace executable at all.
    fn spawn_without_tools(&self, crash_point: Option<&str>) -> Child {
        self.spawn_with_path(crash_point, PathMode::WithoutTools)
    }

    fn spawn_with_path(&self, crash_point: Option<&str>, path_mode: PathMode) -> Child {
        let mut command = Command::new(bin());
        let stderr_path = self.dir.join("daemon.stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).expect("stderr log");
        command
            .args(["daemon", "run", "--socket"])
            .arg(&self.socket)
            .env("XDG_STATE_HOME", &self.state_dir)
            .env("HOME", &self.dir)
            .env(
                "PATH",
                match path_mode {
                    PathMode::FakeWorkspace => self.bin_dir(),
                    PathMode::WithoutTools => {
                        let empty = self.dir.join("empty-bin");
                        std::fs::create_dir_all(&empty).expect("empty bin");
                        empty
                    }
                },
            )
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file));
        if let Some(point) = crash_point {
            command.env("CANTER_CRASH_POINT", point);
        }
        command.spawn().expect("spawn daemon")
    }
}

#[derive(Clone, Copy)]
enum PathMode {
    FakeWorkspace,
    WithoutTools,
}

fn wait_ready(fixture: &Fixture) {
    let deadline = Instant::now() + READY_TIMEOUT;
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
        canter::canonical::canonical_text(&doc)
    );
    doc.get("result").expect("result").clone()
}

fn rpc_err(socket: &Path, id: &str, method: &str, params: Option<Val>) -> (String, String) {
    let doc = rpc(socket, id, method, params);
    assert_eq!(
        doc.get("ok").and_then(Val::as_bool),
        Some(false),
        "expected refused response for {method}: {}",
        canter::canonical::canonical_text(&doc)
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

const WORKTREE: &str = "worktrees/issues/75";
const ORCH_SESSION: &str = "sess-0001";
const ORCH_PROCESS: &str = "proc-0001";

fn replacement_params(lane: &str, role: &str, key: &str, session: &str, process: &str) -> Val {
    object(vec![
        ("idempotency_key", string(key)),
        ("lane_id", string(lane)),
        ("generation", integer(1)),
        ("source_session", string(session)),
        ("source_process", string(process)),
        ("role", string(role)),
        ("worktree", string(WORKTREE)),
        ("reason", string("handoff retirement window")),
    ])
}

/// A valid synthetic checkpoint observation for one lane; `orchestration`
/// adds the orchestrator reference block when supplied.
fn observation(role: &str, orchestration: Option<Val>) -> Val {
    let mut fields = vec![
        ("role", string(role)),
        ("task", string("issue-75 retirement capture")),
        ("worktree", string(WORKTREE)),
        ("branch", string("issue-75-retire")),
        ("head", string(&"a".repeat(40))),
        ("base", string(&"b".repeat(40))),
        (
            "dirty",
            object(vec![
                ("count", integer(1)),
                ("digest", string(&"c".repeat(64))),
            ]),
        ),
        (
            "untracked",
            object(vec![
                ("count", integer(0)),
                ("digest", string(&"d".repeat(64))),
            ]),
        ),
        (
            "report",
            object(vec![
                ("round", integer(1)),
                ("reviewed_sha", string(&"e".repeat(40))),
            ]),
        ),
        (
            "gates",
            Val::Arr(vec![object(vec![
                ("name", string("focused")),
                ("status", string("pending")),
            ])]),
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
    ];
    if let Some(orchestration) = orchestration {
        fields.push(("orchestration", orchestration));
    }
    object(fields)
}

fn orchestration(workers: &[&str], reviewers: &[&str]) -> Val {
    object(vec![
        (
            "workers",
            Val::Arr(workers.iter().map(|id| string(id)).collect()),
        ),
        (
            "reviewers",
            Val::Arr(reviewers.iter().map(|id| string(id)).collect()),
        ),
        (
            "pending_events",
            Val::Arr(vec![string("worker-finished:lane-8")]),
        ),
    ])
}

fn replacement_of(result: &Val) -> &Val {
    result.get("replacement").expect("replacement")
}

fn checkpoint_of(result: &Val) -> &Val {
    result.get("checkpoint").expect("checkpoint")
}

fn retirement_of(result: &Val) -> &Val {
    result.get("retirement").expect("retirement")
}

fn field_str<'a>(val: &'a Val, key: &str) -> &'a str {
    val.get(key).and_then(Val::as_str).expect(key)
}

fn request_replacement(
    socket: &Path,
    id_seed: u32,
    lane: &str,
    role: &str,
    key: &str,
    session: &str,
    process: &str,
) -> String {
    let result = rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.request",
        Some(replacement_params(lane, role, key, session, process)),
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

fn capture_checkpoint(
    socket: &Path,
    id_seed: u32,
    replacement_id: &str,
    role: &str,
    orchestration: Option<Val>,
    key: &str,
) -> String {
    let observation = observation(role, orchestration);
    let result = rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.checkpoint.create",
        Some(object(vec![
            ("idempotency_key", string(key)),
            ("replacement_id", string(replacement_id)),
            ("generation", integer(1)),
            ("observation", observation.clone()),
            ("reobservation", observation),
        ])),
    );
    field_str(checkpoint_of(&result), "digest").to_string()
}

fn status_of(socket: &Path, id_seed: u32, replacement_id: &str) -> Val {
    rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.status",
        Some(object(vec![("replacement_id", string(replacement_id))])),
    )
}

/// Request, advance and checkpoint one replacement through the socket;
/// returns `(replacement_id, checkpoint_digest)`.
fn checkpointed_record(
    fixture: &Fixture,
    id_seed: u32,
    lane: &str,
    role: &str,
    orchestration: Option<Val>,
    key_prefix: &str,
) -> (String, String) {
    let replacement_id = request_replacement(
        &fixture.socket,
        id_seed,
        lane,
        role,
        &format!("ik_{key_prefix}-request"),
        ORCH_SESSION,
        ORCH_PROCESS,
    );
    advance_to_quiescing(
        &fixture.socket,
        id_seed + 1,
        &replacement_id,
        &format!("ik_{key_prefix}-advance"),
    );
    let digest = capture_checkpoint(
        &fixture.socket,
        id_seed + 2,
        &replacement_id,
        role,
        orchestration,
        &format!("ik_{key_prefix}-capture"),
    );
    (replacement_id, digest)
}

fn retirement_binding(
    generation: i64,
    session: &str,
    process: &str,
    checkpoint_digest: &str,
) -> Val {
    object(vec![
        ("generation", integer(generation)),
        ("session", string(session)),
        ("process", string(process)),
        ("checkpoint_digest", string(checkpoint_digest)),
    ])
}

fn retirement_recheck(
    session: &str,
    process: Option<&str>,
    children: &[(&str, &str)],
    active: bool,
) -> Val {
    object(vec![
        ("observed_at", string("2026-09-12T00:00:00Z")),
        ("session", string(session)),
        ("process", process.map(string).unwrap_or_else(null)),
        (
            "children",
            Val::Arr(
                children
                    .iter()
                    .map(|(command, state)| {
                        object(vec![("command", string(command)), ("state", string(state))])
                    })
                    .collect(),
            ),
        ),
        ("active", bool_(active)),
    ])
}

fn harness_pi() -> Val {
    object(vec![("key", string("lane-orch-1")), ("kind", string("pi"))])
}

fn retirement_params(
    request_id: &str,
    replacement_id: &str,
    binding: Val,
    recheck: Val,
    harness: Val,
) -> Val {
    let key = format!("ik_retire-{request_id}");
    object(vec![
        ("idempotency_key", string(&key)),
        ("replacement_id", string(replacement_id)),
        ("binding", binding),
        ("recheck", recheck),
        ("harness", harness),
    ])
}

fn retire(socket: &Path, id: &str, replacement_id: &str, binding: Val, recheck: Val) -> Val {
    rpc_ok(
        socket,
        id,
        "lane.retire",
        Some(retirement_params(
            id,
            replacement_id,
            binding,
            recheck,
            harness_pi(),
        )),
    )
}

// ---------------------------------------------------------------------------
// AC1 (+ probe P1): the binding fences generation, session/process identity
// and the committed checkpoint digest BEFORE any effect.
// ---------------------------------------------------------------------------

#[test]
fn retirement_binds_the_record_and_refuses_changed_evidence_before_any_effect() {
    let fixture = Fixture::new("bind");
    fixture.set_mode("retired");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let (replacement_id, digest) = checkpointed_record(
        &fixture,
        1,
        "lane-orch-1",
        "implementer",
        None,
        "rt-bind-0001",
    );
    let before = status_of(&fixture.socket, 10, &replacement_id);
    assert_eq!(field_str(replacement_of(&before), "phase"), "checkpointed");

    // Every changed bound value refuses typed, and NOTHING is invoked: no
    // stop, no read-back, no state change.
    let cases: Vec<(&str, Val, Val, &str)> = vec![
        (
            "changed checkpoint digest",
            retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &"f".repeat(64)),
            retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
            "refusal.retirement.binding",
        ),
        (
            "changed source session",
            retirement_binding(1, "sess-0009", ORCH_PROCESS, &digest),
            retirement_recheck("sess-0009", Some(ORCH_PROCESS), &[], false),
            "refusal.retirement.binding",
        ),
        (
            "changed source process",
            retirement_binding(1, ORCH_SESSION, "proc-0009", &digest),
            retirement_recheck(ORCH_SESSION, Some("proc-0009"), &[], false),
            "refusal.retirement.binding",
        ),
        (
            "stale lane generation",
            retirement_binding(2, ORCH_SESSION, ORCH_PROCESS, &digest),
            retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
            "refusal.retirement.binding",
        ),
    ];
    for (index, (what, binding, recheck, expected)) in cases.into_iter().enumerate() {
        let id = fresh_id(20 + index as u32);
        let (code, message) = rpc_err(
            &fixture.socket,
            &id,
            "lane.retire",
            Some(retirement_params(
                &id,
                &replacement_id,
                binding,
                recheck,
                harness_pi(),
            )),
        );
        assert_eq!(code, *expected, "{what}: {message}");
        assert!(
            fixture.invocations().is_empty(),
            "{what}: no effect was invoked: {:?}",
            fixture.invocations()
        );
    }
    assert_eq!(
        status_of(&fixture.socket, 11, &replacement_id),
        before,
        "a refused retirement leaves the record and its history untouched"
    );

    // The exact binding retires: exactly one bounded stop and exactly one
    // confirmation read, both addressed to the record's own session.
    let id = fresh_id(30);
    let result = retire(
        &fixture.socket,
        &id,
        &replacement_id,
        retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
        retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
    );
    let retirement = retirement_of(&result);
    let replacement = retirement.get("replacement").expect("replacement");
    assert_eq!(field_str(replacement, "phase"), "retired");
    assert_eq!(field_str(replacement, "outcome"), "pending");
    assert_eq!(
        field_str(retirement, "checkpoint_digest"),
        digest,
        "the response binds the committed checkpoint digest"
    );
    assert_eq!(
        retirement.get("evidence").map(|evidence| {
            (
                field_str(evidence, "process"),
                field_str(evidence, "registration"),
            )
        }),
        Some(("absent", "released"))
    );
    assert_eq!(
        fixture.invocations(),
        vec![
            format!("session interrupt {ORCH_SESSION} --json"),
            format!("session show {ORCH_SESSION} --json"),
        ],
        "one bounded stop then one confirmation read, and nothing else"
    );

    // The stop is issued at most once: a replayed claim returns the recorded
    // response and invokes nothing.
    let replay = rpc_ok(
        &fixture.socket,
        &id,
        "lane.retire",
        Some(retirement_params(
            &id,
            &replacement_id,
            retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
            retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
            harness_pi(),
        )),
    );
    assert_eq!(replay, result, "the recorded response is replayed");
    assert_eq!(
        fixture.invocations().len(),
        2,
        "a replay never repeats the stop"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC2 (+ probe P2): the immediate pre-stop quiescence recheck holds.
// ---------------------------------------------------------------------------

#[test]
fn retirement_rechecks_quiescence_immediately_before_the_stop() {
    let fixture = Fixture::new("recheck");
    fixture.set_mode("retired");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let cases: Vec<(&str, Val, &str)> = vec![
        (
            "active child activity",
            retirement_recheck(
                ORCH_SESSION,
                Some(ORCH_PROCESS),
                &[("cargo test --locked", "active")],
                false,
            ),
            "refusal.retirement.held",
        ),
        (
            "ambiguous child activity",
            retirement_recheck(
                ORCH_SESSION,
                Some(ORCH_PROCESS),
                &[("cargo test --locked", "ambiguous")],
                false,
            ),
            "refusal.retirement.held",
        ),
        (
            "unknown process identity",
            retirement_recheck(ORCH_SESSION, None, &[], false),
            "refusal.retirement.held",
        ),
        (
            "execution still active",
            retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], true),
            "refusal.retirement.held",
        ),
    ];
    for (index, (what, recheck, expected)) in cases.iter().enumerate() {
        let lane = format!("lane-recheck-{index}");
        let (replacement_id, digest) = checkpointed_record(
            &fixture,
            100 + index as u32 * 10,
            &lane,
            "implementer",
            None,
            &format!("rt-rck-{index}"),
        );
        let id = fresh_id(200 + index as u32);
        let (code, message) = rpc_err(
            &fixture.socket,
            &id,
            "lane.retire",
            Some(retirement_params(
                &id,
                &replacement_id,
                retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
                recheck.clone(),
                harness_pi(),
            )),
        );
        assert_eq!(code, *expected, "{what}: {message}");
        assert!(
            fixture.invocations().is_empty(),
            "{what}: the hold signals nothing: {:?}",
            fixture.invocations()
        );
        let status = status_of(&fixture.socket, 300 + index as u32, &replacement_id);
        assert_eq!(
            field_str(replacement_of(&status), "phase"),
            "checkpointed",
            "{what}: the record stays at the checkpointed boundary"
        );
        assert_eq!(
            field_str(replacement_of(&status), "outcome"),
            "pending",
            "{what}: a pre-effect hold never parks the record"
        );
    }

    // A quiescent recheck retires (the hold is about the evidence, not the
    // surface).
    let (replacement_id, digest) = checkpointed_record(
        &fixture,
        400,
        "lane-recheck-ok",
        "implementer",
        None,
        "rt-rck-ok",
    );
    let result = retire(
        &fixture.socket,
        &fresh_id(450),
        &replacement_id,
        retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
        retirement_recheck(
            ORCH_SESSION,
            Some(ORCH_PROCESS),
            &[("cargo test --locked", "exited")],
            false,
        ),
    );
    assert_eq!(
        field_str(
            retirement_of(&result)
                .get("replacement")
                .expect("replacement"),
            "phase"
        ),
        "retired"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC3: the graceful stop is bounded, its failure holds explicitly, and no
// escalation exists on the path.
// ---------------------------------------------------------------------------

#[test]
fn graceful_stop_failure_holds_explicitly_without_escalation() {
    let fixture = Fixture::new("stop-fail");
    fixture.set_mode("stop-fail");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let (replacement_id, digest) = checkpointed_record(
        &fixture,
        500,
        "lane-stopfail",
        "implementer",
        None,
        "rt-stop",
    );
    let id = fresh_id(510);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.retire",
        Some(retirement_params(
            &id,
            &replacement_id,
            retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
            retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
            harness_pi(),
        )),
    );
    assert_eq!(code, "refusal.retirement.held", "{message}");
    assert!(
        message.contains("did not confirm"),
        "the failure is explicit: {message}"
    );
    // The stop was attempted exactly once, no confirmation read followed it,
    // and nothing escalates (.e.g no SIGKILL, no pattern, no process group):
    // the only invocation the daemon ever made is the one session interrupt.
    assert_eq!(
        fixture.invocations(),
        vec![format!("session interrupt {ORCH_SESSION} --json")],
        "one bounded stop attempt and no follow-up escalation"
    );
    // The record is durably parked for external reconciliation.
    let status = status_of(&fixture.socket, 520, &replacement_id);
    assert_eq!(field_str(replacement_of(&status), "phase"), "checkpointed");
    assert_eq!(field_str(replacement_of(&status), "outcome"), "ambiguous");
    let history = status
        .get("history")
        .and_then(Val::as_array)
        .expect("history");
    let last = history.last().expect("last history row");
    assert_eq!(field_str(last, "to_outcome"), "ambiguous");
    assert!(
        field_str(last, "reason").contains("NO further signal"),
        "the park records the no-repeat decision: {}",
        field_str(last, "reason")
    );

    // A parked record refuses a second retirement before any effect.
    let id = fresh_id(530);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.retire",
        Some(retirement_params(
            &id,
            &replacement_id,
            retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
            retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
            harness_pi(),
        )),
    );
    assert_eq!(code, "refusal.replacement.ambiguous", "{message}");
    assert_eq!(
        fixture.invocations().len(),
        1,
        "a parked record never signals again"
    );

    shutdown(daemon);
}

#[test]
fn graceful_stop_has_a_bounded_deadline() {
    let fixture = Fixture::new("stop-bounded");
    fixture.set_mode("stop-sleep");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let (replacement_id, digest) = checkpointed_record(
        &fixture,
        600,
        "lane-bounded",
        "implementer",
        None,
        "rt-bound",
    );
    let id = fresh_id(610);
    let started = Instant::now();
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.retire",
        Some(retirement_params(
            &id,
            &replacement_id,
            retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
            retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
            harness_pi(),
        )),
    );
    let elapsed = started.elapsed();
    assert_eq!(code, "refusal.retirement.held", "{message}");
    assert!(
        elapsed < Duration::from_secs(30),
        "the stop deadline is bounded (stopped a sleeping session in {elapsed:?})"
    );
    assert!(
        elapsed >= Duration::from_secs(5),
        "the stop deadline waited for the adapter bound, not for the sleep ({elapsed:?})"
    );
    assert_eq!(
        fixture.invocations(),
        vec![format!("session interrupt {ORCH_SESSION} --json")],
        "the bounded stop is issued once and never escalated"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC4 (+ probe P3): the confirmation reads backend process AND
// ownership/registration evidence; labels, reused panes and stale
// registrations fail closed.
// ---------------------------------------------------------------------------

#[test]
fn post_stop_confirmation_fails_closed_on_labels_reused_identities_and_stale_registrations() {
    let fixture = Fixture::new("confirm");
    fixture.write_fake_workspace();
    fixture.set_mode("retired");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let cases: Vec<(&str, &str, &str)> = vec![
        // (mode, expected error code, what)
        (
            "label-only",
            "refusal.retirement.held",
            "a `done` label with the bound process still present never retires",
        ),
        (
            "no-process",
            "refusal.retirement.held",
            "missing process evidence holds",
        ),
        (
            "malformed",
            "refusal.retirement.held",
            "an unparsable confirmation read-back holds",
        ),
        (
            "reused-process",
            "refusal.retirement.reused",
            "a different process under the bound session fails closed",
        ),
        (
            "stale-registration",
            "refusal.retirement.reused",
            "a registration of another generation fails closed",
        ),
    ];
    for (index, (mode, expected, what)) in cases.iter().enumerate() {
        let id = fresh_id(800 + index as u32);
        fixture.set_mode(mode);
        let lane = format!("lane-confirm-{index}");
        let (replacement_id, digest) = checkpointed_record(
            &fixture,
            700 + index as u32 * 10,
            &lane,
            "implementer",
            None,
            &format!("rt-cf-{index}"),
        );
        let (code, message) = rpc_err(
            &fixture.socket,
            &id,
            "lane.retire",
            Some(retirement_params(
                &id,
                &replacement_id,
                retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
                retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
                harness_pi(),
            )),
        );
        assert_eq!(code, *expected, "{what}: {message}");
        let status = status_of(&fixture.socket, 900 + index as u32, &replacement_id);
        assert_eq!(
            field_str(replacement_of(&status), "phase"),
            "checkpointed",
            "{what}: the retirement never claims an unconfirmed stop"
        );
        assert_eq!(
            field_str(replacement_of(&status), "outcome"),
            "ambiguous",
            "{what}: a stop whose confirmation cannot prove absence is parked for review"
        );
    }

    // The clean backend evidence retires the session.
    fixture.set_mode("retired");
    let (replacement_id, digest) = checkpointed_record(
        &fixture,
        950,
        "lane-confirm-ok",
        "implementer",
        None,
        "rt-cf-ok",
    );
    let result = retire(
        &fixture.socket,
        &fresh_id(960),
        &replacement_id,
        retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
        retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
    );
    assert_eq!(
        field_str(
            retirement_of(&result)
                .get("replacement")
                .expect("replacement"),
            "phase"
        ),
        "retired"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC5: retiring an orchestrator preserves its worker/reviewer lanes and every
// worktree byte.
// ---------------------------------------------------------------------------

#[test]
fn retiring_an_orchestrator_preserves_workers_reviewers_and_bytes() {
    let fixture = Fixture::new("children");
    fixture.set_mode("retired");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // Two child lanes with their own synthetic identities.
    let worker = request_replacement(
        &fixture.socket,
        1000,
        "lane-8",
        "implementer",
        "ik_rt-child-worker-01",
        "sess-0002",
        "proc-0002",
    );
    let reviewer = request_replacement(
        &fixture.socket,
        1001,
        "lane-9",
        "reviewer",
        "ik_rt-child-review-01",
        "sess-0003",
        "proc-0003",
    );
    let worker_before = status_of(&fixture.socket, 1002, &worker);
    let reviewer_before = status_of(&fixture.socket, 1003, &reviewer);

    // A dirty lane worktree with report bytes must survive unchanged.
    let worktree = fixture.dir.join("worktrees").join("issues").join("75");
    std::fs::create_dir_all(&worktree).expect("worktree dir");
    let report = worktree.join(".report-75.md");
    std::fs::write(&report, "lane report bytes: unchanged\n").expect("report");
    let fingerprint =
        |path: &Path| canter::canonical::sha256_hex(&std::fs::read(path).expect("read report"));
    let report_before = fingerprint(&report);

    let (orchestrator, digest) = checkpointed_record(
        &fixture,
        1010,
        "lane-orch-2",
        "orchestrator",
        Some(orchestration(&[&worker], &[&reviewer])),
        "rt-child-orch",
    );
    let result = retire(
        &fixture.socket,
        &fresh_id(1020),
        &orchestrator,
        retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
        retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
    );
    assert_eq!(
        field_str(
            retirement_of(&result)
                .get("replacement")
                .expect("replacement"),
            "phase"
        ),
        "retired",
        "the orchestrator is the lane that retires"
    );

    // The children were never addressed and their records are untouched.
    let invoked: Vec<String> = fixture.invocations();
    assert_eq!(
        invoked,
        vec![
            format!("session interrupt {ORCH_SESSION} --json"),
            format!("session show {ORCH_SESSION} --json"),
        ],
        "only the orchestrator's own session is addressed: {invoked:?}"
    );
    for child in ["sess-0002", "sess-0003", "proc-0002", "proc-0003"] {
        assert!(
            invoked.iter().all(|line| !line.contains(child)),
            "a child identity was addressed: {child} in {invoked:?}"
        );
    }
    assert_eq!(status_of(&fixture.socket, 1030, &worker), worker_before);
    assert_eq!(status_of(&fixture.socket, 1031, &reviewer), reviewer_before);

    // Every worktree byte survives (the retirement touches no file).
    assert_eq!(fingerprint(&report), report_before);
    assert_eq!(
        std::fs::read_to_string(&report).expect("read"),
        "lane report bytes: unchanged\n"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC6 (+ probe P4): the crash after the stop reconciles exact absence and
// never repeats a signal — not even against a reused identity.
// ---------------------------------------------------------------------------

#[test]
fn interrupted_retirement_reconciles_exact_absence_without_repeating_a_signal() {
    let fixture = Fixture::new("reconcile-absence");
    fixture.set_mode("retired");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(Some("lane-retire.after-stop"));
    wait_ready(&fixture);

    let (replacement_id, digest) = checkpointed_record(
        &fixture,
        1100,
        "lane-crash",
        "implementer",
        None,
        "rt-crash",
    );
    let id = fresh_id(1110);
    let mut connection = Connection::open(&fixture.socket).expect("connect");
    connection
        .send_request(
            &id,
            "lane.retire",
            Some(&retirement_params(
                &id,
                &replacement_id,
                retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
                retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
                harness_pi(),
            )),
        )
        .expect("send retirement");
    drop(connection);
    // The daemon aborts at the crash point: the stop was issued, its outcome
    // was never journaled.
    wait_exit(daemon, "crashed daemon");
    assert_eq!(
        fixture.invocations(),
        vec![format!("session interrupt {ORCH_SESSION} --json")],
        "the stop was issued exactly once before the crash"
    );

    // Restart: reconciliation reconciles exact absence and completes the
    // retirement — WITHOUT a second stop.
    fixture.set_mode("retired");
    let restarted = fixture.spawn(None);
    wait_ready(&fixture);
    let status = status_of(&fixture.socket, 1120, &replacement_id);
    assert_eq!(
        field_str(replacement_of(&status), "phase"),
        "retired",
        "exact absence completes the interrupted retirement"
    );
    let history = status
        .get("history")
        .and_then(Val::as_array)
        .expect("history");
    let last = history.last().expect("last history row");
    assert_eq!(field_str(last, "from_phase"), "checkpointed");
    assert_eq!(field_str(last, "to_phase"), "retired");
    assert!(
        field_str(last, "reason").contains("reconciled after an interrupted retirement"),
        "the reconciled evidence summary is durable: {}",
        field_str(last, "reason")
    );
    let invoked: Vec<String> = fixture.invocations();
    assert_eq!(
        invoked
            .iter()
            .filter(|line| line.starts_with("session interrupt"))
            .count(),
        1,
        "reconciliation never repeats the stop: {invoked:?}"
    );
    assert_eq!(
        invoked
            .iter()
            .filter(|line| line.starts_with("session show"))
            .count(),
        1,
        "reconciliation reads the backend confirmation exactly once: {invoked:?}"
    );
    let log = std::fs::read_to_string(fixture.daemon_log()).expect("daemon log");
    assert!(
        log.contains("reconcile.lane.retire"),
        "the reconciliation is journaled in the daemon log"
    );

    shutdown(restarted);
}

#[test]
fn interrupted_retirement_never_signals_a_reused_identity() {
    let fixture = Fixture::new("reconcile-reused");
    fixture.set_mode("retired");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(Some("lane-retire.after-stop"));
    wait_ready(&fixture);

    let (replacement_id, digest) = checkpointed_record(
        &fixture,
        1200,
        "lane-crash-reuse",
        "implementer",
        None,
        "rt-reuse",
    );
    let id = fresh_id(1210);
    let mut connection = Connection::open(&fixture.socket).expect("connect");
    connection
        .send_request(
            &id,
            "lane.retire",
            Some(&retirement_params(
                &id,
                &replacement_id,
                retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
                retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
                harness_pi(),
            )),
        )
        .expect("send retirement");
    drop(connection);
    wait_exit(daemon, "crashed daemon");

    // The restart observes a REUSED identity: the record is parked
    // ambiguous and the stop is never repeated against the reused process.
    fixture.set_mode("reused-process");
    let restarted = fixture.spawn(None);
    wait_ready(&fixture);
    let status = status_of(&fixture.socket, 1220, &replacement_id);
    assert_eq!(field_str(replacement_of(&status), "phase"), "checkpointed");
    assert_eq!(field_str(replacement_of(&status), "outcome"), "ambiguous");
    let history = status
        .get("history")
        .and_then(Val::as_array)
        .expect("history");
    let last = history.last().expect("last history row");
    assert!(
        field_str(last, "reason").contains("reused identity")
            && field_str(last, "reason").contains("no signal was repeated"),
        "the park records the reused identity and the no-repeat decision: {}",
        field_str(last, "reason")
    );
    let invoked: Vec<String> = fixture.invocations();
    assert_eq!(
        invoked
            .iter()
            .filter(|line| line.starts_with("session interrupt"))
            .count(),
        1,
        "the reused identity was never signalled: {invoked:?}"
    );
    assert_eq!(
        invoked
            .iter()
            .filter(|line| line.starts_with("session show"))
            .count(),
        1,
        "only the read-back ran on the restart: {invoked:?}"
    );

    shutdown(restarted);
}

// ---------------------------------------------------------------------------
// The adapter path: unsupported adapters refuse with a capability refusal and
// an unavailable workspace never signals.
// ---------------------------------------------------------------------------

#[test]
fn retirement_refuses_unsupported_adapters_and_an_unavailable_workspace() {
    let fixture = Fixture::new("adapter");
    fixture.set_mode("retired");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let (replacement_id, digest) = checkpointed_record(
        &fixture,
        1300,
        "lane-adapter",
        "implementer",
        None,
        "rt-adapt",
    );

    // An argv profile declaring only the read-back capability is an
    // unsupported adapter for the retirement path: capability refusal BEFORE
    // any claim or effect.
    let id = fresh_id(1310);
    let partial_harness = object(vec![
        ("key", string("lane-adapter-1")),
        ("kind", string("argv")),
        ("executable", string("hf-lane-stub")),
        ("capabilities", Val::Arr(vec![string("observe")])),
    ]);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.retire",
        Some(retirement_params(
            &id,
            &replacement_id,
            retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
            retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
            partial_harness,
        )),
    );
    assert_eq!(code, "unknown.capability", "{message}");
    // An unknown adapter kind refuses typed.
    let id = fresh_id(1311);
    let unknown_kind = object(vec![
        ("key", string("lane-adapter-2")),
        ("kind", string("teleport")),
    ]);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.retire",
        Some(retirement_params(
            &id,
            &replacement_id,
            retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
            retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
            unknown_kind,
        )),
    );
    assert_eq!(code, "unknown.harness", "{message}");
    assert!(
        fixture.invocations().is_empty(),
        "an unsupported adapter never signals"
    );
    // The refused attempts left the record exactly at its checkpointed
    // boundary (they refused before the claim: nothing was journaled).
    assert_eq!(
        field_str(
            replacement_of(&status_of(&fixture.socket, 1320, &replacement_id)),
            "phase"
        ),
        "checkpointed"
    );

    shutdown(daemon);

    // An unavailable workspace executable (empty PATH): the stop row cannot
    // run, so nothing is signalled and the record is not parked — the
    // retirement is a typed refusal with a retryable record.
    let no_tools = Fixture::new("adapter-no-tools");
    no_tools.set_mode("retired");
    no_tools.write_fake_workspace();
    let daemon = no_tools.spawn_without_tools(None);
    wait_ready(&no_tools);
    let (replacement_id, digest) = checkpointed_record(
        &no_tools,
        1400,
        "lane-no-tools",
        "implementer",
        None,
        "rt-nt",
    );
    let id = fresh_id(1410);
    let (code, message) = rpc_err(
        &no_tools.socket,
        &id,
        "lane.retire",
        Some(retirement_params(
            &id,
            &replacement_id,
            retirement_binding(1, ORCH_SESSION, ORCH_PROCESS, &digest),
            retirement_recheck(ORCH_SESSION, Some(ORCH_PROCESS), &[], false),
            harness_pi(),
        )),
    );
    assert_eq!(code, "refusal.unavailable.harness", "{message}");
    let status = status_of(&no_tools.socket, 1420, &replacement_id);
    assert_eq!(field_str(replacement_of(&status), "phase"), "checkpointed");
    assert_eq!(
        field_str(replacement_of(&status), "outcome"),
        "pending",
        "a stop that never ran does not park the record"
    );
    assert!(no_tools.invocations().is_empty());

    shutdown(daemon);
}
