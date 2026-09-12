//! Issue #79 acceptance driver — the merged #73–#78 lane-handoff path under
//! interruption: no lost work, no duplicated owners, no destructive recovery.
//!
//! One reproducible fixture-level driver over the REAL `canter` binary, the
//! real daemon (Unix socket, SQLite state, crash-point hook) and a
//! deterministic local harness fixture: a fake `herdr` shell script that
//! records every invocation and whose read-backs the tests switch per phase.
//! Nothing here touches a live fleet, a real session, the network or any
//! provider: every identity is synthetic and the only workspace executable
//! is the fake in the per-test temp dir.
//!
//! Acceptance map (issue #79):
//! - `full_chain_keeps_the_worker_alive_and_consumes_the_gap_completion_once`
//!   — fresh-session handoff + adoption + the one permitted post-adoption
//!   fixture action; the original worker survives the orchestrator
//!   retirement; a completion during the gap is consumed exactly once; the
//!   worktree bytes, untracked inventory, exact-head review references and
//!   pending human gates captured by the checkpoint survive the handoff.
//! - `crash_at_checkpoint_persistence_reconciles_the_same_generation`,
//!   `crash_after_retirement_reconciles_without_repeating_a_signal`,
//!   `crash_after_spawn_before_acknowledgment_reconciles_without_a_second_spawn`,
//!   `crash_during_adoption_commits_nothing_and_a_fresh_retry_adopts_once`
//!   — interrupt at each transition, restart the daemon, reconcile the same
//!   handoff/generation without a duplicate spawn or effect.
//! - `source_overlap_and_reused_identities_stop_safely` — source/successor
//!   overlap and PID/pane reuse stop safely before any effect.
//! - `corrupt_or_missing_checkpoint_artifacts_stay_non_destructive` — a
//!   corrupt/missing committed brief artifact and an orphan artifact fail
//!   closed or regenerate; nothing is ever destructively recovered.
//! - `stale_generation_evidence_refuses_before_any_effect` and
//!   `changed_checkpoint_evidence_refuses_start_and_retire` — the generation
//!   and checkpoint-integrity fences refuse before any effect.
//! - `pause_injected_at_each_transition_is_sticky_across_restart` — a pause
//!   injected at every transition stays sticky across a restart; no
//!   successor starts or begins work while paused (disposable fixture only).
//! - `profile_switch_fixture_distinguishes_requested_actual_and_permitted_fallback`
//!   — requested model, actual binding and permitted fallback are distinct
//!   fields; no network credentials are required at any point.
//! - `expired_authorization_and_unverified_successors_hold_without_effect`
//!   — an expired (stale) authorization and an unverifiable successor hold
//!   with no effect.
//!
//! Fixture vs real: everything exercised here drives the real daemon/state
//! code over the real socket. The ADAPTER side is a synthetic shell fixture
//! — a real `herdr` binary, a real provider binding read-back and real
//! credential handling are NOT certified by this driver (remaining gates,
//! stated in `.report-79.md`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use canter::canonical::sha256_hex;
use canter::client::{Connection, RpcError};
use canter::time::{rfc3339_from_unix, unix_now};
use canter::value::{Val, bool_, integer, null, object, string};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_canter")
}

/// Default per-test daemon readiness deadline.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// The fake workspace executable. `$HOME/workspace-mode` selects the
/// successor read-back; `$HOME/source-mode` selects the source-session
/// read-back. The script records every invocation in
/// `$HOME/workspace-invocations.log` so the tests can assert exactly which
/// effects were and were not issued. The daemon hands the adapter an
/// allowlisted environment, so the script sets its own utility PATH first
/// and only ever uses /usr/bin and /bin tools.
const FAKE_WORKSPACE: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin
log="$HOME/workspace-invocations.log"
printf '%s\n' "$*" >> "$log"
mode="$(cat "$HOME/workspace-mode" 2>/dev/null || echo ready)"
case "$1 $2" in
  "session interrupt")
    printf '%s\n' '{"interrupted":true}' ;;
  "session start")
    case "$mode" in
      start-fail) echo '{"error":"refused"}' >&2; exit 3 ;;
      *) printf '%s\n' '{"started":true}' ;;
    esac ;;
  "session show")
    if [ "$3" = "sess-0001" ]; then
      src="$(cat "$HOME/source-mode" 2>/dev/null || echo retired)"
      case "$src" in
        live) doc='{"session_id":"sess-0001","state":"working","process":"proc-0001","registration":{"state":"active","session":"sess-0001","generation":1}}' ;;
        reused-source) doc='{"session_id":"sess-0001","state":"working","process":"proc-0009","registration":{"state":"released","session":"sess-0001","generation":1}}' ;;
        *) doc='{"session_id":"sess-0001","state":"retired","process":null,"registration":{"state":"released","session":"sess-0001","generation":1}}' ;;
      esac
    else
      case "$mode" in
        orchestrator) role='orchestrator' ;;
        *) role='implementer' ;;
      esac
      case "$mode" in
        no-process) processpart='' ;;
        reused-process) processpart=',"process":"proc-0001"' ;;
        *) processpart=',"process":"proc-0002"' ;;
      esac
      case "$mode" in
        binding-match) binding=',"binding":{"provider":"example-provider","model":"example-model"}' ;;
        binding-fallback) binding=',"binding":{"provider":"example-fallback-provider","model":"example-fallback-model"}' ;;
        binding-unexpected) binding=',"binding":{"provider":"example-rogue-provider","model":"example-rogue-model"}' ;;
        *) binding='' ;;
      esac
      case "$mode" in
        malformed) doc='not-json' ;;
        *) doc="{\"session_id\":\"sess-0002\",\"state\":\"working\"${processpart},\"role\":\"${role}\",\"profile\":{\"key\":\"lane-orch-1\",\"kind\":\"pi\"},\"cwd\":\"worktrees/issues/79\",\"kickoff_receipt\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"readiness\":\"ready\"${binding}}" ;;
      esac
    fi
    printf '%s\n' "$doc" ;;
  *) echo '{"error":"unknown row"}' >&2; exit 4 ;;
esac
"#;

/// The reviewed synthetic profile configuration: fictional provider and
/// model names only, no live policy, no real credential.
const PROFILE_CONFIG: &str = r#"schema = "hf-config/v1"
[repository.widgets]
origin = "https://github.com/example-org/widgets"
branch = "staging"
[harness.lane-orch-1]
kind = "pi"
executable = "pi-example"
env_allow = ["PATH", "EXAMPLE_PROVIDER_KEY"]
provider = "example-provider"
model = "example-model"
fallback = ["example-fallback-provider/example-fallback-model"]
secret_env = ["EXAMPLE_PROVIDER_KEY"]
binding_introspection = true
[harness.lane-orch-1.limits]
context_tokens = 131072
"#;

struct Fixture {
    dir: PathBuf,
    state_dir: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("hf-79-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("fixture dir");
        Fixture {
            state_dir: dir.join("state"),
            socket: dir.join("daemon.sock"),
            dir,
        }
    }

    fn daemon_log(&self) -> PathBuf {
        self.state_dir.join("canter").join("daemon.log")
    }

    /// The daemon-owned checkpoint brief directory (`XDG_STATE_HOME` /
    /// `canter` / `checkpoints`).
    fn checkpoints_dir(&self) -> PathBuf {
        self.state_dir.join("canter").join("checkpoints")
    }

    fn bin_dir(&self) -> PathBuf {
        self.dir.join("bin")
    }

    fn invocations_log(&self) -> PathBuf {
        self.dir.join("workspace-invocations.log")
    }

    /// Every workspace (fake `herdr`) invocation recorded so far, in order.
    fn invocations(&self) -> Vec<String> {
        fs::read_to_string(self.invocations_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Select the fake workspace's successor read-back behavior (and let the
    /// test switch it between a crash and the restart that reconciles it).
    fn set_mode(&self, mode: &str) {
        fs::write(self.dir.join("workspace-mode"), format!("{mode}\n")).expect("write mode");
    }

    /// Select the fake workspace's source-session read-back behavior.
    fn set_source_mode(&self, mode: &str) {
        fs::write(self.dir.join("source-mode"), format!("{mode}\n")).expect("write source mode");
    }

    fn write_fake_workspace(&self) {
        let bin_dir = self.bin_dir();
        fs::create_dir_all(&bin_dir).expect("bin dir");
        let path = bin_dir.join("herdr");
        fs::write(&path, FAKE_WORKSPACE).expect("write fake workspace");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
        }
        fs::set_permissions(&path, permissions).expect("chmod");
    }

    /// Write the reviewed config at the XDG default path under the fixture
    /// HOME (the exact path `canter config show` discovers).
    fn write_config(&self, text: &str) {
        let path = self.dir.join(".config").join("canter").join("config.toml");
        fs::create_dir_all(path.parent().expect("config dir")).expect("config dir");
        fs::write(&path, text).expect("write config");
    }

    /// Run the real `config show --json` preview under an environment that
    /// carries NO credential at all: the profile-switch fixture must work
    /// without network credentials. Returns `(plan document, credentials
    /// status, raw stdout)`.
    fn config_show(&self) -> (Val, Val, String) {
        let out = Command::new(bin())
            .args(["config", "show", "--json"])
            .env_clear()
            .env("PATH", self.bin_dir())
            .env("HOME", &self.dir)
            .env("LANG", "C")
            .output()
            .expect("run config show");
        let raw = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(
            out.status.code(),
            Some(0),
            "config show must succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let envelope = Val::parse_json(raw.trim())
            .unwrap_or_else(|err| panic!("config show envelope parse: {err}\n{raw}"));
        let data = envelope.get("data").expect("data");
        let Val::Arr(harnesses) = data.get("harnesses").expect("harnesses") else {
            panic!("harnesses must be an array: {raw}");
        };
        let row = harnesses.first().expect("one configured harness");
        (
            row.get("profile").expect("profile field").clone(),
            row.get("credentials").expect("credentials").clone(),
            raw,
        )
    }

    /// Spawn the daemon with the fake workspace executable as the only
    /// `herdr` on PATH.
    fn spawn(&self, crash_point: Option<&str>) -> Child {
        let mut command = Command::new(bin());
        let stderr_path = self.dir.join("daemon.stderr.log");
        let stderr_file = fs::File::create(&stderr_path).expect("stderr log");
        command
            .args(["daemon", "run", "--socket"])
            .arg(&self.socket)
            .env("XDG_STATE_HOME", &self.state_dir)
            .env("HOME", &self.dir)
            .env("PATH", self.bin_dir())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file));
        if let Some(point) = crash_point {
            command.env("CANTER_CRASH_POINT", point);
        }
        command.spawn().expect("spawn daemon")
    }
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
    let stderr_text = fs::read_to_string(fixture.dir.join("daemon.stderr.log")).unwrap_or_default();
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

/// Issue one request that may be cut short by an injected daemon crash:
/// connection failures are tolerated (the crash point aborts the daemon
/// before a response).
fn rpc_unchecked(socket: &Path, id: &str, method: &str, params: Option<Val>) {
    let Ok(mut connection) = Connection::open(socket) else {
        return;
    };
    if connection
        .send_request(id, method, params.as_ref())
        .is_err()
    {
        return;
    }
    let _ = connection.read_response();
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
        field_str(error, "code").to_string(),
        field_str(error, "message").to_string(),
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

/// Wait until the daemon process exits (the crash point abort).
fn wait_crash(daemon: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if daemon.try_wait().expect("try_wait").is_some() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not reach the crash point in time"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn count_invocations(fixture: &Fixture, prefix: &str) -> usize {
    fixture
        .invocations()
        .iter()
        .filter(|line| line.starts_with(prefix))
        .count()
}

fn interrupt_count(fixture: &Fixture) -> usize {
    count_invocations(fixture, "session interrupt")
}

fn spawn_count(fixture: &Fixture) -> usize {
    count_invocations(fixture, "session start")
}

// ---------------------------------------------------------------------------
// Synthetic request builders (public-data boundary: no host paths/identities)
// ---------------------------------------------------------------------------

const WORKTREE: &str = "worktrees/issues/79";
const SOURCE_SESSION: &str = "sess-0001";
const SOURCE_PROCESS: &str = "proc-0001";
const SUCCESSOR_SESSION: &str = "sess-0002";
const HEAD: &str = "1111111111111111111111111111111111111111";
const BASE: &str = "2222222222222222222222222222222222222222";
const REVIEWED_SHA: &str = "5555555555555555555555555555555555555555";

fn kickoff_receipt() -> String {
    "a".repeat(64)
}

fn harness_pi() -> Val {
    object(vec![("key", string("lane-orch-1")), ("kind", string("pi"))])
}

fn replacement_params(
    lane: &str,
    role: &str,
    session: &str,
    process: &str,
    profile: Option<Val>,
    key: &str,
) -> Val {
    let mut fields = vec![
        ("idempotency_key", string(key)),
        ("lane_id", string(lane)),
        ("generation", integer(1)),
        ("source_session", string(session)),
        ("source_process", string(process)),
        ("role", string(role)),
        ("worktree", string(WORKTREE)),
        ("reason", string("crash acceptance handoff window")),
    ];
    if let Some(profile) = profile {
        fields.push(("profile", profile));
    }
    object(fields)
}

/// A valid synthetic checkpoint observation for one lane; `orchestration`
/// adds the orchestrator reference block when supplied.
fn observation(role: &str, orchestration: Option<Val>) -> Val {
    let mut fields = vec![
        ("role", string(role)),
        ("task", string("issue-79 crash acceptance capture")),
        ("worktree", string(WORKTREE)),
        ("branch", string("issue-79-crash-acceptance")),
        ("head", string(HEAD)),
        ("base", string(BASE)),
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
                ("count", integer(2)),
                ("digest", string(&"d".repeat(64))),
            ]),
        ),
        (
            "report",
            object(vec![
                ("round", integer(1)),
                ("reviewed_sha", string(REVIEWED_SHA)),
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

/// The fresh adoption re-query: the closed comparison contract (the
/// capture-only `execution`/`orchestration` blocks are not part of it).
fn adoption_observation(role: &str) -> Val {
    let capture = observation(role, None);
    let mut fields: Vec<(&'static str, Val)> = vec![];
    for key in [
        "role",
        "task",
        "worktree",
        "branch",
        "head",
        "base",
        "dirty",
        "untracked",
        "report",
        "gates",
        "children",
    ] {
        fields.push((key, capture.get(key).expect("field").clone()));
    }
    object(fields)
}

fn orchestration(workers: &[&str], reviewers: &[&str], pending: &[&str]) -> Val {
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
            Val::Arr(pending.iter().map(|id| string(id)).collect()),
        ),
    ])
}

fn field_str<'a>(val: &'a Val, key: &str) -> &'a str {
    val.get(key).and_then(Val::as_str).expect(key)
}

fn field_int(val: &Val, key: &str) -> i64 {
    val.get(key).and_then(Val::as_int).expect(key)
}

/// The array-of-strings at `key` (empty when absent/malformed).
fn string_array<'a>(val: &'a Val, key: &str) -> Vec<&'a str> {
    val.get(key)
        .and_then(Val::as_array)
        .map(|items| items.iter().filter_map(Val::as_str).collect())
        .unwrap_or_default()
}

fn replacement_of(result: &Val) -> &Val {
    result.get("replacement").expect("replacement")
}

fn retirement_of(result: &Val) -> &Val {
    result.get("retirement").expect("retirement")
}

fn start_of(result: &Val) -> &Val {
    result.get("start").expect("start")
}

fn adoption_of(result: &Val) -> &Val {
    result.get("adoption").expect("adoption")
}

fn successor_of(result: &Val) -> &Val {
    result.get("successor").expect("successor")
}

fn checkpoint_of(result: &Val) -> &Val {
    result.get("checkpoint").expect("checkpoint")
}

fn status_of(socket: &Path, id_seed: u32, replacement_id: &str) -> Val {
    rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.status",
        Some(object(vec![("replacement_id", string(replacement_id))])),
    )
}

fn checkpoint_status(socket: &Path, id_seed: u32, replacement_id: &str) -> Val {
    rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.checkpoint.status",
        Some(object(vec![("replacement_id", string(replacement_id))])),
    )
}

fn request_replacement(fixture: &Fixture, id_seed: u32, lane: &str, role: &str, key: &str) -> Val {
    request_replacement_for(
        fixture,
        id_seed,
        lane,
        role,
        SOURCE_SESSION,
        SOURCE_PROCESS,
        None,
        key,
    )
}

// One explicit builder for the full synthetic identity set; the per-test
// wrappers below narrow it (why-comment for the repo-wide allow).
#[allow(clippy::too_many_arguments)]
fn request_replacement_for(
    fixture: &Fixture,
    id_seed: u32,
    lane: &str,
    role: &str,
    session: &str,
    process: &str,
    profile: Option<Val>,
    key: &str,
) -> Val {
    rpc_ok(
        &fixture.socket,
        &fresh_id(id_seed),
        "lane.replacement.request",
        Some(replacement_params(
            lane, role, session, process, profile, key,
        )),
    )
}

fn advance_to_quiescing(socket: &Path, id_seed: u32, replacement_id: &str, key: &str) {
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
    );
}

fn checkpoint_params(replacement_id: &str, observation: &Val, key: &str) -> Val {
    object(vec![
        ("idempotency_key", string(key)),
        ("replacement_id", string(replacement_id)),
        ("generation", integer(1)),
        ("observation", observation.clone()),
        ("reobservation", observation.clone()),
    ])
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
        Some(checkpoint_params(replacement_id, &observation, key)),
    );
    field_str(checkpoint_of(&result), "digest").to_string()
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
    let requested = request_replacement(
        fixture,
        id_seed,
        lane,
        role,
        &format!("ik_{key_prefix}-request"),
    );
    let replacement_id = field_str(replacement_of(&requested), "replacement_id").to_string();
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

fn retirement_recheck(session: &str, process: &str) -> Val {
    object(vec![
        ("observed_at", string("2026-09-12T00:00:00Z")),
        ("session", string(session)),
        ("process", string(process)),
        ("children", Val::Arr(vec![])),
        ("active", bool_(false)),
    ])
}

fn retire_params(
    id: &str,
    replacement_id: &str,
    generation: i64,
    session: &str,
    process: &str,
    digest: &str,
) -> Val {
    object(vec![
        ("idempotency_key", string(&format!("ik_retire-{id}"))),
        ("replacement_id", string(replacement_id)),
        (
            "binding",
            retirement_binding(generation, session, process, digest),
        ),
        ("recheck", retirement_recheck(session, process)),
        ("harness", harness_pi()),
    ])
}

/// Drive the retirement transition; returns the `retirement` result doc.
fn retire_ok(
    socket: &Path,
    id: &str,
    replacement_id: &str,
    generation: i64,
    session: &str,
    process: &str,
    digest: &str,
) -> Val {
    let result = rpc_ok(
        socket,
        id,
        "lane.retire",
        Some(retire_params(
            id,
            replacement_id,
            generation,
            session,
            process,
            digest,
        )),
    );
    let retirement = retirement_of(&result).clone();
    assert_eq!(
        field_str(retirement.get("replacement").expect("replacement"), "phase"),
        "retired",
        "the source retirement must commit"
    );
    retirement
}

/// Drive the full retirement transition: replacement → advance → checkpoint
/// → retire. Returns `(replacement_id, checkpoint_digest)` at `retired`.
fn retired_record(
    fixture: &Fixture,
    id_seed: u32,
    lane: &str,
    role: &str,
    orchestration: Option<Val>,
    key_prefix: &str,
) -> (String, String) {
    let (replacement_id, digest) =
        checkpointed_record(fixture, id_seed, lane, role, orchestration, key_prefix);
    retire_ok(
        &fixture.socket,
        &fresh_id(id_seed + 3),
        &replacement_id,
        1,
        SOURCE_SESSION,
        SOURCE_PROCESS,
        &digest,
    );
    (replacement_id, digest)
}

fn admission(caps: (i64, i64, i64), measured_at: &str) -> Val {
    object(vec![
        ("repository", string("example-org/widgets")),
        (
            "caps",
            object(vec![
                ("global", integer(caps.0)),
                ("repository", integer(caps.1)),
                ("harness", integer(caps.2)),
            ]),
        ),
        (
            "host_proof",
            object(vec![("measured_at", string(measured_at))]),
        ),
        ("running", Val::Arr(vec![])),
    ])
}

fn live_admission() -> Val {
    admission((8, 4, 4), &canter::time::rfc3339_now())
}

fn start_params(
    id: &str,
    replacement_id: &str,
    digest: &str,
    nonce: &str,
    session: &str,
    admission: Val,
    profile: Option<Val>,
) -> Val {
    let mut fields = vec![
        ("idempotency_key", string(&format!("ik_start-{id}"))),
        ("replacement_id", string(replacement_id)),
        (
            "binding",
            object(vec![
                ("generation", integer(1)),
                ("checkpoint_digest", string(digest)),
                ("nonce", string(nonce)),
            ]),
        ),
        (
            "successor",
            object(vec![
                ("session", string(session)),
                ("kickoff_receipt", string(&kickoff_receipt())),
            ]),
        ),
        ("harness", harness_pi()),
        ("admission", admission),
    ];
    if let Some(profile) = profile {
        fields.push(("profile", profile));
    }
    object(fields)
}

/// The plain start: live admission, no profile plan.
fn start_ok(socket: &Path, id: &str, replacement_id: &str, digest: &str, nonce: &str) -> Val {
    rpc_ok(
        socket,
        id,
        "lane.start",
        Some(start_params(
            id,
            replacement_id,
            digest,
            nonce,
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    )
}

fn adopt_params(id: &str, replacement_id: &str, successor_id: &str, role: &str) -> Val {
    let observation = adoption_observation(role);
    object(vec![
        ("idempotency_key", string(&format!("ik_adopt-{id}"))),
        ("replacement_id", string(replacement_id)),
        (
            "binding",
            object(vec![
                ("generation", integer(1)),
                ("successor_id", string(successor_id)),
                ("session", string(SUCCESSOR_SESSION)),
            ]),
        ),
        ("observation", observation.clone()),
        ("reobservation", observation),
        ("harness", harness_pi()),
    ])
}

fn adopt_ok(socket: &Path, id: &str, replacement_id: &str, successor_id: &str, role: &str) -> Val {
    rpc_ok(
        socket,
        id,
        "lane.adopt",
        Some(adopt_params(id, replacement_id, successor_id, role)),
    )
}

/// Start one verified successor and return `(replacement_id, digest,
/// successor_id)` at the committed `adopting` boundary.
fn started_record(
    fixture: &Fixture,
    id_seed: u32,
    lane: &str,
    role: &str,
    orchestration: Option<Val>,
    key_prefix: &str,
) -> (String, String, String) {
    let (replacement_id, digest) =
        retired_record(fixture, id_seed, lane, role, orchestration, key_prefix);
    let id = fresh_id(id_seed + 10);
    let result = start_ok(&fixture.socket, &id, &replacement_id, &digest, "nonce-0001");
    assert_eq!(
        field_str(replacement_of(start_of(&result)), "phase"),
        "adopting",
        "the verified start commits the `adopting` boundary"
    );
    let successor_id = field_str(successor_of(start_of(&result)), "successor_id").to_string();
    (replacement_id, digest, successor_id)
}

fn hold(fixture: &Fixture, id_seed: u32, replacement_id: &str, reason: &str) -> Val {
    rpc_ok(
        &fixture.socket,
        &fresh_id(id_seed),
        "lane.replacement.hold",
        Some(object(vec![
            (
                "idempotency_key",
                string(&format!("ik_hold-{}", fresh_id(id_seed))),
            ),
            ("replacement_id", string(replacement_id)),
            ("reason", string(reason)),
        ])),
    )
}

// ---------------------------------------------------------------------------
// AC: the driver runs the fresh-session handoff + adoption + one permitted
// post-adoption fixture action; the original worker survives the
// orchestrator retirement; the gap completion is consumed exactly once; the
// worktree bytes, untracked inventory, exact-head references and pending
// human gates survive the handoff.
// ---------------------------------------------------------------------------

#[test]
fn full_chain_keeps_the_worker_alive_and_consumes_the_gap_completion_once() {
    let fixture = Fixture::new("full-chain");
    fixture.set_mode("orchestrator");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // Two referenced child lanes (one live worker, one reviewer) exist and
    // stay untouched through the whole orchestrator handoff.
    let worker = request_replacement_for(
        &fixture,
        1,
        "lane-2",
        "implementer",
        "sess-0003",
        "proc-0003",
        None,
        "ik_79-worker-request",
    );
    let worker_id = field_str(replacement_of(&worker), "replacement_id").to_string();
    let reviewer = request_replacement_for(
        &fixture,
        2,
        "lane-3",
        "reviewer",
        "sess-0004",
        "proc-0004",
        None,
        "ik_79-reviewer-request",
    );
    let reviewer_id = field_str(replacement_of(&reviewer), "replacement_id").to_string();
    let worker_before = status_of(&fixture.socket, 3, &worker_id);
    let reviewer_before = status_of(&fixture.socket, 4, &reviewer_id);

    // The orchestrator replacement runs the FULL public chain: request →
    // advance → checkpoint (with the retained references) → retire.
    let requested = request_replacement(
        &fixture,
        10,
        "lane-orch-9",
        "orchestrator",
        "ik_79-orch-request",
    );
    let replacement_id = field_str(replacement_of(&requested), "replacement_id").to_string();
    assert_eq!(field_str(replacement_of(&requested), "phase"), "requested");
    advance_to_quiescing(&fixture.socket, 11, &replacement_id, "ik_79-orch-advance");
    let digest = capture_checkpoint(
        &fixture.socket,
        12,
        &replacement_id,
        "orchestrator",
        Some(orchestration(
            &[&worker_id],
            &[&reviewer_id],
            &["worker-finished:lane-2"],
        )),
        "ik_79-orch-capture",
    );

    // The committed checkpoint preserves the worktree bytes, untracked
    // inventory, exact-head review references and pending human gates.
    let checkpoint =
        checkpoint_of(&checkpoint_status(&fixture.socket, 13, &replacement_id)).clone();
    assert_eq!(field_int(&checkpoint, "generation"), 1);
    assert_eq!(
        field_str(&checkpoint, "digest"),
        digest,
        "the committed checkpoint digest"
    );
    let snapshot = checkpoint.get("snapshot").expect("snapshot");
    assert_eq!(
        field_str(snapshot, "head"),
        HEAD,
        "the exact-head reference"
    );
    assert_eq!(field_str(snapshot, "base"), BASE);
    assert_eq!(
        field_str(snapshot.get("report").expect("report"), "reviewed_sha"),
        REVIEWED_SHA,
        "the exact-head review reference is preserved"
    );
    assert_eq!(
        field_str(snapshot.get("dirty").expect("dirty"), "digest"),
        "c".repeat(64),
        "the tracked dirty bytes are digest-bound"
    );
    assert_eq!(
        field_int(snapshot.get("untracked").expect("untracked"), "count"),
        2
    );
    assert_eq!(
        field_str(snapshot.get("untracked").expect("untracked"), "digest"),
        "d".repeat(64),
        "the untracked file inventory is preserved"
    );
    let gates = snapshot
        .get("gates")
        .and_then(Val::as_array)
        .expect("gates");
    assert_eq!(
        field_str(&gates[0], "status"),
        "pending",
        "the pending human gate is preserved"
    );
    let children = snapshot
        .get("children")
        .and_then(Val::as_array)
        .expect("children");
    assert_eq!(field_str(&children[0], "state"), "exited");

    // The one bounded retirement addresses ONLY the bound source session:
    // the live worker and the reviewer are never touched.
    let retirement = retire_ok(
        &fixture.socket,
        &fresh_id(14),
        &replacement_id,
        1,
        SOURCE_SESSION,
        SOURCE_PROCESS,
        &digest,
    );
    assert_eq!(
        field_str(&retirement, "session"),
        SOURCE_SESSION,
        "the retirement names the bound source session"
    );
    let logged = fixture.invocations().join("\n");
    for absent in ["sess-0003", "sess-0004", "proc-0003", "proc-0004"] {
        assert!(
            !logged.contains(absent),
            "the handoff must never address the referenced child identity {absent}: {logged}"
        );
    }
    assert_eq!(
        status_of(&fixture.socket, 15, &worker_id),
        worker_before,
        "the worker record is byte-unchanged by the orchestrator retirement"
    );
    assert_eq!(
        status_of(&fixture.socket, 16, &reviewer_id),
        reviewer_before,
        "the reviewer record is byte-unchanged by the orchestrator retirement"
    );

    // The successor starts FRESH on the same logical lane/worktree and is
    // verified through the adapter read-back before anything is adopted.
    let id = fresh_id(20);
    let started = start_ok(&fixture.socket, &id, &replacement_id, &digest, "nonce-0001");
    let successor = successor_of(start_of(&started)).clone();
    assert_eq!(field_str(&successor, "session"), SUCCESSOR_SESSION);
    assert_eq!(field_str(&successor, "worktree"), WORKTREE);
    assert_eq!(field_str(&successor, "role"), "orchestrator");
    assert_eq!(field_str(&successor, "delivery"), "delivered");
    assert_eq!(field_int(&successor, "attempts"), 1);
    assert_eq!(field_str(&successor, "process"), "proc-0002");
    assert_eq!(spawn_count(&fixture), 1, "exactly one fresh-session spawn");

    // Adoption re-queries the lane and compares against the committed
    // snapshot; the retained orchestration block is preserved.
    let successor_id = field_str(&successor, "successor_id").to_string();
    let adopted = adopt_ok(
        &fixture.socket,
        &fresh_id(30),
        &replacement_id,
        &successor_id,
        "orchestrator",
    );
    assert_eq!(
        field_str(replacement_of(adoption_of(&adopted)), "phase"),
        "adopted"
    );
    let adopted_successor = successor_of(adoption_of(&adopted)).clone();
    let preserved = adopted_successor
        .get("orchestration")
        .expect("orchestration");
    assert_eq!(string_array(preserved, "workers"), vec![worker_id.as_str()]);
    assert_eq!(
        string_array(preserved, "reviewers"),
        vec![reviewer_id.as_str()]
    );
    assert_eq!(
        string_array(preserved, "pending_events"),
        vec!["worker-finished:lane-2"],
        "the worker completion observed during the gap is recorded"
    );

    // The one permitted post-adoption fixture action: the gap completion is
    // consumed exactly once and dispatches nothing.
    let invocations_before = fixture.invocations().len();
    let id = fresh_id(40);
    let consumed_params = object(vec![
        ("idempotency_key", string(&format!("ik_consume-{id}"))),
        ("replacement_id", string(&replacement_id)),
        ("successor_id", string(&successor_id)),
        (
            "completions",
            Val::Arr(vec![object(vec![
                ("event", string("worker-finished:lane-2")),
                ("worker", string(&worker_id)),
            ])]),
        ),
    ]);
    let consumed = rpc_ok(
        &fixture.socket,
        &id,
        "lane.successor.consume",
        Some(consumed_params.clone()),
    );
    assert_eq!(
        string_array(&consumed, "consumed"),
        vec!["worker-finished:lane-2"],
        "the completion is durably consumed once"
    );
    let replay = rpc_ok(
        &fixture.socket,
        &id,
        "lane.successor.consume",
        Some(consumed_params),
    );
    assert_eq!(
        replay, consumed,
        "the recorded response is replayed verbatim"
    );
    let id = fresh_id(41);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.successor.consume",
        Some(object(vec![
            ("idempotency_key", string(&format!("ik_consume-{id}"))),
            ("replacement_id", string(&replacement_id)),
            ("successor_id", string(&successor_id)),
            (
                "completions",
                Val::Arr(vec![object(vec![
                    ("event", string("worker-finished:lane-2")),
                    ("worker", string(&worker_id)),
                ])]),
            ),
        ])),
    );
    assert_eq!(code, "refusal.successor.event_consumed", "{message}");
    let id = fresh_id(42);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.successor.consume",
        Some(object(vec![
            ("idempotency_key", string(&format!("ik_consume-{id}"))),
            ("replacement_id", string(&replacement_id)),
            ("successor_id", string(&successor_id)),
            (
                "completions",
                Val::Arr(vec![object(vec![
                    ("event", string("worker-finished:lane-99")),
                    ("worker", string(&worker_id)),
                ])]),
            ),
        ])),
    );
    assert_eq!(code, "refusal.successor.event", "{message}");
    assert_eq!(
        fixture.invocations().len(),
        invocations_before,
        "the consumption surface invokes nothing (no duplicate reviewer dispatch)"
    );
    let parked = status_of(&fixture.socket, 43, &replacement_id);
    assert_eq!(
        string_array(successor_of(&parked), "consumed"),
        vec!["worker-finished:lane-2"],
        "the consumption is durable on the successor row"
    );
    assert_eq!(status_of(&fixture.socket, 44, &worker_id), worker_before);
    assert_eq!(
        status_of(&fixture.socket, 45, &reviewer_id),
        reviewer_before
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC: interrupt at checkpoint persistence; restart; reconcile the same
// generation without a duplicate effect.
// ---------------------------------------------------------------------------

#[test]
fn crash_at_checkpoint_persistence_reconciles_the_same_generation() {
    // Case A: the crash lands right after the commit (before the derived
    // brief artifact is materialized). The restart yields the COMPLETE
    // checkpoint — the derived artifact is regenerated and digest-verified
    // from the durable row — and the same generation continues.
    let fixture = Fixture::new("crash-checkpoint-a");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let mut daemon = fixture.spawn(Some("lane-checkpoint.after-record"));
    wait_ready(&fixture);

    let requested = request_replacement(
        &fixture,
        1,
        "lane-79-ck-a",
        "implementer",
        "ik_79-ck-a-request",
    );
    let replacement_id = field_str(replacement_of(&requested), "replacement_id").to_string();
    advance_to_quiescing(&fixture.socket, 2, &replacement_id, "ik_79-ck-a-advance");
    let capture = observation("implementer", None);
    rpc_unchecked(
        &fixture.socket,
        &fresh_id(3),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            &capture,
            "ik_79-ck-a-capture",
        )),
    );
    wait_crash(&mut daemon);

    let restarted = fixture.spawn(None);
    wait_ready(&fixture);
    let status = status_of(&fixture.socket, 4, &replacement_id);
    assert_eq!(
        field_str(replacement_of(&status), "phase"),
        "checkpointed",
        "the commit landed before the crash and survives the restart"
    );
    let checkpoint = checkpoint_of(&checkpoint_status(&fixture.socket, 5, &replacement_id)).clone();
    let digest = field_str(&checkpoint, "digest").to_string();
    let snapshot = checkpoint.get("snapshot").expect("snapshot");
    assert_eq!(
        field_str(snapshot, "head"),
        HEAD,
        "the preserved references survive the crash"
    );
    assert_eq!(
        field_str(snapshot.get("report").expect("report"), "reviewed_sha"),
        REVIEWED_SHA
    );
    let brief_path = PathBuf::from(field_str(&checkpoint, "brief_path"));
    let brief_bytes = fs::read(&brief_path).expect("regenerated brief artifact");
    assert_eq!(
        sha256_hex(&brief_bytes),
        field_str(&checkpoint, "brief_digest"),
        "the regenerated brief verifies against the digest bound at commit time"
    );
    let log_text = fs::read_to_string(fixture.daemon_log()).expect("daemon log");
    assert!(
        log_text.contains("reconcile.lane.checkpoint"),
        "the restart reconciliation is logged"
    );

    // The same handoff/generation continues: retire → start → adopt with
    // exactly one spawn.
    retire_ok(
        &fixture.socket,
        &fresh_id(6),
        &replacement_id,
        1,
        SOURCE_SESSION,
        SOURCE_PROCESS,
        &digest,
    );
    let started = start_ok(
        &fixture.socket,
        &fresh_id(7),
        &replacement_id,
        &digest,
        "nonce-0001",
    );
    let successor_id = field_str(successor_of(start_of(&started)), "successor_id").to_string();
    let adopted = adopt_ok(
        &fixture.socket,
        &fresh_id(8),
        &replacement_id,
        &successor_id,
        "implementer",
    );
    assert_eq!(
        field_str(replacement_of(adoption_of(&adopted)), "phase"),
        "adopted"
    );
    assert_eq!(
        field_int(successor_of(adoption_of(&adopted)), "generation"),
        2,
        "the successor row carries the lane's next generation slot (record generation 1)"
    );
    assert_eq!(
        spawn_count(&fixture),
        1,
        "no duplicate spawn across the crash"
    );
    shutdown(restarted);

    // Case B: the crash lands BEFORE the commit. Nothing was committed, no
    // artifact exists, and a fresh capture commits exactly once.
    let fixture = Fixture::new("crash-checkpoint-b");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let mut daemon = fixture.spawn(Some("lane-checkpoint.after-intent"));
    wait_ready(&fixture);
    let requested = request_replacement(
        &fixture,
        10,
        "lane-79-ck-b",
        "implementer",
        "ik_79-ck-b-request",
    );
    let replacement_id = field_str(replacement_of(&requested), "replacement_id").to_string();
    advance_to_quiescing(&fixture.socket, 11, &replacement_id, "ik_79-ck-b-advance");
    let capture = observation("implementer", None);
    rpc_unchecked(
        &fixture.socket,
        &fresh_id(12),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            &capture,
            "ik_79-ck-b-capture",
        )),
    );
    wait_crash(&mut daemon);

    let restarted = fixture.spawn(None);
    wait_ready(&fixture);
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(13),
        "lane.checkpoint.status",
        Some(object(vec![("replacement_id", string(&replacement_id))])),
    );
    assert_eq!(code, "state.not_found", "{message}");
    assert_eq!(
        fs::read_dir(fixture.checkpoints_dir())
            .expect("checkpoints dir")
            .count(),
        0,
        "no brief artifact exists after the pre-commit crash"
    );
    let status = status_of(&fixture.socket, 14, &replacement_id);
    assert_eq!(field_str(replacement_of(&status), "phase"), "quiescing");
    assert_eq!(field_str(replacement_of(&status), "outcome"), "pending");
    let digest = capture_checkpoint(
        &fixture.socket,
        15,
        &replacement_id,
        "implementer",
        None,
        "ik_79-ck-b-retry",
    );
    let status = status_of(&fixture.socket, 16, &replacement_id);
    assert_eq!(
        field_str(replacement_of(&status), "phase"),
        "checkpointed",
        "the fresh capture commits exactly once"
    );
    assert_eq!(
        field_str(
            checkpoint_of(&checkpoint_status(&fixture.socket, 17, &replacement_id)),
            "digest"
        ),
        digest
    );
    assert_eq!(
        spawn_count(&fixture),
        0,
        "the checkpoint path spawns nothing"
    );
    shutdown(restarted);
}

// ---------------------------------------------------------------------------
// AC: interrupt after the retirement; restart; reconcile without repeating
// a signal.
// ---------------------------------------------------------------------------

#[test]
fn crash_after_retirement_reconciles_without_repeating_a_signal() {
    // Case A: the bounded graceful stop was delivered; the restart proves
    // exact absence and completes the SAME generation. The stop is issued
    // AT MOST ONCE, ever.
    let fixture = Fixture::new("crash-retire-a");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let mut daemon = fixture.spawn(Some("lane-retire.after-stop"));
    wait_ready(&fixture);

    let (replacement_id, digest) =
        checkpointed_record(&fixture, 1, "lane-79-retire-a", "implementer", None, "ra");
    rpc_unchecked(
        &fixture.socket,
        &fresh_id(10),
        "lane.retire",
        Some(retire_params(
            &fresh_id(10),
            &replacement_id,
            1,
            SOURCE_SESSION,
            SOURCE_PROCESS,
            &digest,
        )),
    );
    wait_crash(&mut daemon);
    assert_eq!(interrupt_count(&fixture), 1, "the one bounded stop");

    let restarted = fixture.spawn(None);
    wait_ready(&fixture);
    let status = status_of(&fixture.socket, 11, &replacement_id);
    assert_eq!(
        field_str(replacement_of(&status), "phase"),
        "retired",
        "the restart completes the interrupted retirement from exact absence"
    );
    assert_eq!(
        interrupt_count(&fixture),
        1,
        "no signal is ever repeated across the crash/restart"
    );
    // The same generation continues: start → adopt with exactly one spawn.
    let started = start_ok(
        &fixture.socket,
        &fresh_id(12),
        &replacement_id,
        &digest,
        "nonce-0001",
    );
    let successor_id = field_str(successor_of(start_of(&started)), "successor_id").to_string();
    adopt_ok(
        &fixture.socket,
        &fresh_id(13),
        &replacement_id,
        &successor_id,
        "implementer",
    );
    assert_eq!(spawn_count(&fixture), 1);
    shutdown(restarted);

    // Case B: absence cannot be proven after the restart: the record is
    // parked for external reconciliation and STILL no second signal is
    // issued against the live source.
    let fixture = Fixture::new("crash-retire-b");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let mut daemon = fixture.spawn(Some("lane-retire.after-stop"));
    wait_ready(&fixture);

    let (replacement_id, digest) =
        checkpointed_record(&fixture, 20, "lane-79-retire-b", "implementer", None, "rb");
    rpc_unchecked(
        &fixture.socket,
        &fresh_id(30),
        "lane.retire",
        Some(retire_params(
            &fresh_id(30),
            &replacement_id,
            1,
            SOURCE_SESSION,
            SOURCE_PROCESS,
            &digest,
        )),
    );
    wait_crash(&mut daemon);
    // The source turns out to still be present (the retirement did not take).
    fixture.set_source_mode("live");

    let restarted = fixture.spawn(None);
    wait_ready(&fixture);
    let status = status_of(&fixture.socket, 31, &replacement_id);
    assert_eq!(
        field_str(replacement_of(&status), "outcome"),
        "ambiguous",
        "unproven absence parks the record for external reconciliation"
    );
    assert_eq!(
        interrupt_count(&fixture),
        1,
        "the interrupted retirement never signals a second time"
    );
    let id = fresh_id(32);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    );
    assert_eq!(code, "refusal.replacement.ambiguous", "{message}");
    assert_eq!(interrupt_count(&fixture), 1);
    shutdown(restarted);
}

// ---------------------------------------------------------------------------
// AC: interrupt after the spawn, before its acknowledgment; restart;
// reconcile without a second spawn.
// ---------------------------------------------------------------------------

#[test]
fn crash_after_spawn_before_acknowledgment_reconciles_without_a_second_spawn() {
    let fixture = Fixture::new("crash-spawn");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let mut daemon = fixture.spawn(Some("lane-start.after-spawn"));
    wait_ready(&fixture);

    let (replacement_id, digest) =
        retired_record(&fixture, 1, "lane-79-spawn", "implementer", None, "sp");
    let id = fresh_id(10);
    rpc_unchecked(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    );
    wait_crash(&mut daemon);
    assert_eq!(spawn_count(&fixture), 1, "the spawn was issued");

    let restarted = fixture.spawn(None);
    wait_ready(&fixture);
    let status = status_of(&fixture.socket, 11, &replacement_id);
    assert_eq!(
        field_str(replacement_of(&status), "phase"),
        "adopting",
        "the restart reconciles the committed boundary from the read-back"
    );
    let successor = successor_of(&status).clone();
    assert_eq!(field_str(&successor, "process"), "proc-0002");
    assert_eq!(field_str(&successor, "nonce"), "nonce-0001");
    assert_eq!(
        spawn_count(&fixture),
        1,
        "the restart never repeats the spawn"
    );
    let successor_id = field_str(&successor, "successor_id").to_string();
    let adopted = adopt_ok(
        &fixture.socket,
        &fresh_id(12),
        &replacement_id,
        &successor_id,
        "implementer",
    );
    assert_eq!(
        field_str(replacement_of(adoption_of(&adopted)), "phase"),
        "adopted"
    );
    assert_eq!(
        spawn_count(&fixture),
        1,
        "the adoption re-verifies and never spawns again"
    );
    shutdown(restarted);
}

// ---------------------------------------------------------------------------
// AC: interrupt during adoption; restart; a fresh request adopts exactly
// once.
// ---------------------------------------------------------------------------

#[test]
fn crash_during_adoption_commits_nothing_and_a_fresh_retry_adopts_once() {
    let fixture = Fixture::new("crash-adopt");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let mut daemon = fixture.spawn(Some("lane-adopt.after-intent"));
    wait_ready(&fixture);

    let (replacement_id, _digest, successor_id) =
        started_record(&fixture, 1, "lane-79-adopt", "implementer", None, "ad");
    let invocations_before = fixture.invocations();
    let id = fresh_id(20);
    rpc_unchecked(
        &fixture.socket,
        &id,
        "lane.adopt",
        Some(adopt_params(
            &id,
            &replacement_id,
            &successor_id,
            "implementer",
        )),
    );
    wait_crash(&mut daemon);

    let restarted = fixture.spawn(None);
    wait_ready(&fixture);
    assert_eq!(
        fixture.invocations(),
        invocations_before,
        "the interrupted adoption invoked nothing"
    );
    let parked = status_of(&fixture.socket, 21, &replacement_id);
    assert_eq!(
        field_str(replacement_of(&parked), "phase"),
        "adopting",
        "the interrupted adoption committed no phase transition"
    );
    assert_eq!(
        field_str(successor_of(&parked), "adopted_at"),
        "",
        "the successor is never adopted by a crashed claim"
    );
    // A fresh request reconciles the same evidence and adopts once.
    let adopted = adopt_ok(
        &fixture.socket,
        &fresh_id(22),
        &replacement_id,
        &successor_id,
        "implementer",
    );
    assert_eq!(
        field_str(replacement_of(adoption_of(&adopted)), "phase"),
        "adopted"
    );
    assert_eq!(
        spawn_count(&fixture),
        1,
        "the fresh adoption re-verifies and never spawns a second session"
    );
    let added: Vec<String> = fixture.invocations()[invocations_before.len()..].to_vec();
    assert_eq!(
        added.len(),
        2,
        "the fresh adoption only re-reads: {added:?}"
    );
    assert!(
        added.iter().all(|line| line.starts_with("session show")),
        "the fresh adoption re-reads the source absence and the successor: {added:?}"
    );
    shutdown(restarted);
}

// ---------------------------------------------------------------------------
// AC: source/successor overlap and PID/pane reuse stop safely with no
// destructive recovery.
// ---------------------------------------------------------------------------

#[test]
fn source_overlap_and_reused_identities_stop_safely() {
    let fixture = Fixture::new("overlap-reuse");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // Case A: the source reappeared before the successor started. Source
    // and successor may never both be live: the start refuses BEFORE any
    // effect, and the record is untouched (the refusal is a hold, not a
    // poison: the bounded retry below still succeeds).
    let (replacement_id, digest) =
        retired_record(&fixture, 1, "lane-79-overlap", "implementer", None, "ov");
    let before = status_of(&fixture.socket, 10, &replacement_id);
    fixture.set_source_mode("live");
    let id = fresh_id(11);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    );
    assert_eq!(code, "refusal.successor.source_live", "{message}");
    assert_eq!(spawn_count(&fixture), 0, "an overlap spawns nothing");
    assert_eq!(
        status_of(&fixture.socket, 12, &replacement_id),
        before,
        "the overlap refusal leaves the record untouched"
    );
    fixture.set_source_mode("retired");
    let started = start_ok(
        &fixture.socket,
        &fresh_id(13),
        &replacement_id,
        &digest,
        "nonce-0001",
    );
    assert_eq!(
        field_str(replacement_of(start_of(&started)), "phase"),
        "adopting",
        "the bounded retry after the overlap resolves"
    );

    // Case B: the source identity was reused by another process. The
    // source-absence recheck refuses typed and nothing is signalled.
    let (replacement_id, digest) = retired_record(
        &fixture,
        20,
        "lane-79-reused-source",
        "implementer",
        None,
        "rs",
    );
    let before = status_of(&fixture.socket, 30, &replacement_id);
    fixture.set_source_mode("reused-source");
    let id = fresh_id(31);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    );
    assert_eq!(code, "refusal.retirement.reused", "{message}");
    assert_eq!(
        spawn_count(&fixture),
        1,
        "the reused source spawned nothing new"
    );
    assert_eq!(
        status_of(&fixture.socket, 32, &replacement_id),
        before,
        "the reused-identity refusal leaves the record untouched"
    );
    fixture.set_source_mode("retired");

    // Case C: the successor read-back reuses the SOURCE process. The
    // identity contradiction fails closed: the record is parked ambiguous,
    // nothing is adopted, and a retry never spawns again.
    let (replacement_id, digest) = retired_record(
        &fixture,
        40,
        "lane-79-reused-process",
        "implementer",
        None,
        "rp",
    );
    fixture.set_mode("reused-process");
    let id = fresh_id(50);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    );
    assert_eq!(code, "refusal.successor.reused", "{message}");
    let parked = status_of(&fixture.socket, 51, &replacement_id);
    assert_eq!(field_str(replacement_of(&parked), "outcome"), "ambiguous");
    let successor = successor_of(&parked);
    assert_eq!(
        successor.get("evidence"),
        Some(&Val::Null),
        "no verification evidence is committed under a reused identity"
    );
    assert_eq!(field_str(successor, "adopted_at"), "");
    let spawns_after_c = spawn_count(&fixture);
    let id = fresh_id(52);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    );
    assert_eq!(code, "refusal.replacement.ambiguous", "{message}");
    assert_eq!(
        spawn_count(&fixture),
        spawns_after_c,
        "a parked record never spawns again"
    );

    // Case D: the post-stop confirmation observes a reused identity. The
    // retirement fails closed: parked, typed, and NO further signal. (The
    // record must be at `checkpointed`: it is the retirement being probed,
    // not a re-retirement of an already retired record.)
    let (replacement_id, digest) = checkpointed_record(
        &fixture,
        60,
        "lane-79-reused-retire",
        "implementer",
        None,
        "rr",
    );
    fixture.set_source_mode("reused-source");
    let interrupts_before = interrupt_count(&fixture);
    let id = fresh_id(70);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.retire",
        Some(retire_params(
            &id,
            &replacement_id,
            1,
            SOURCE_SESSION,
            SOURCE_PROCESS,
            &digest,
        )),
    );
    assert_eq!(code, "refusal.retirement.reused", "{message}");
    assert_eq!(
        interrupt_count(&fixture),
        interrupts_before + 1,
        "the one bounded stop was issued to the bound identity"
    );
    let parked = status_of(&fixture.socket, 71, &replacement_id);
    assert_eq!(field_str(replacement_of(&parked), "outcome"), "ambiguous");
    let id = fresh_id(72);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    );
    assert_eq!(code, "refusal.replacement.ambiguous", "{message}");
    assert_eq!(
        interrupt_count(&fixture),
        interrupts_before + 1,
        "no signal is ever repeated against a reused identity"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC: corrupt/missing checkpoint artifacts stay non-destructive.
// ---------------------------------------------------------------------------

#[test]
fn corrupt_or_missing_checkpoint_artifacts_stay_non_destructive() {
    // Case A: the capture committed, the interrupt landed before the derived
    // brief artifact was materialized, and the deterministic artifact path
    // holds CORRUPT bytes when the daemon restarts. The restart
    // reconciliation regenerates the artifact from the durable row and
    // digest-verifies it — a corrupt file is never trusted.
    let fixture = Fixture::new("checkpoint-corrupt");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let mut crashed = fixture.spawn(Some("lane-checkpoint.after-record"));
    wait_ready(&fixture);
    let requested = request_replacement(
        &fixture,
        1,
        "lane-79-ck-corrupt",
        "implementer",
        "ik_79-ck-corrupt-request",
    );
    let replacement_id = field_str(replacement_of(&requested), "replacement_id").to_string();
    advance_to_quiescing(
        &fixture.socket,
        2,
        &replacement_id,
        "ik_79-ck-corrupt-advance",
    );
    let capture = observation("implementer", None);
    rpc_unchecked(
        &fixture.socket,
        &fresh_id(3),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            &capture,
            "ik_79-ck-corrupt-capture",
        )),
    );
    wait_crash(&mut crashed);
    let brief_path = fixture.checkpoints_dir().join(format!(
        "{}.brief",
        canter::state::checkpoint_id_for(&replacement_id)
    ));
    fs::write(&brief_path, b"corrupted brief bytes\n").expect("corrupt artifact");

    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let checkpoint = checkpoint_of(&checkpoint_status(&fixture.socket, 4, &replacement_id)).clone();
    let expected = field_str(&checkpoint, "brief_digest").to_string();
    let bytes = fs::read(&brief_path).expect("regenerated artifact");
    assert_eq!(
        sha256_hex(&bytes),
        expected,
        "the corrupt artifact is regenerated byte-exact, never trusted"
    );
    assert_eq!(
        field_str(
            replacement_of(&status_of(&fixture.socket, 5, &replacement_id)),
            "phase"
        ),
        "checkpointed",
        "the durable record stays checkpointed (no destructive recovery)"
    );
    let log_text = fs::read_to_string(fixture.daemon_log()).expect("daemon log");
    assert!(
        log_text.contains("reconcile.lane.checkpoint"),
        "the regeneration is logged"
    );
    shutdown(daemon);

    // Case B: the same interrupt, but the artifact is entirely MISSING at
    // the restart: it is regenerated byte-exact from the durable row.
    let fixture = Fixture::new("checkpoint-missing");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let mut crashed = fixture.spawn(Some("lane-checkpoint.after-record"));
    wait_ready(&fixture);
    let requested = request_replacement(
        &fixture,
        10,
        "lane-79-ck-missing",
        "implementer",
        "ik_79-ck-missing-request",
    );
    let replacement_id = field_str(replacement_of(&requested), "replacement_id").to_string();
    advance_to_quiescing(
        &fixture.socket,
        11,
        &replacement_id,
        "ik_79-ck-missing-advance",
    );
    let capture = observation("implementer", None);
    rpc_unchecked(
        &fixture.socket,
        &fresh_id(12),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            &capture,
            "ik_79-ck-missing-capture",
        )),
    );
    wait_crash(&mut crashed);
    let brief_path = fixture.checkpoints_dir().join(format!(
        "{}.brief",
        canter::state::checkpoint_id_for(&replacement_id)
    ));
    assert!(
        !brief_path.exists(),
        "the interrupt landed before the artifact was materialized"
    );

    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let checkpoint =
        checkpoint_of(&checkpoint_status(&fixture.socket, 13, &replacement_id)).clone();
    let expected = field_str(&checkpoint, "brief_digest").to_string();
    let bytes = fs::read(&brief_path).expect("regenerated artifact");
    assert_eq!(
        sha256_hex(&bytes),
        expected,
        "the missing artifact is regenerated byte-exact"
    );
    shutdown(daemon);

    // Case C: an artifact without a committed record (only a non-atomic
    // implementation produces this) fails closed on restart: the record is
    // parked ambiguous and the artifact is NEVER deleted.
    let fixture = Fixture::new("checkpoint-orphan");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let requested = request_replacement(
        &fixture,
        20,
        "lane-79-ck-orphan",
        "implementer",
        "ik_79-ck-orphan-request",
    );
    let replacement_id = field_str(replacement_of(&requested), "replacement_id").to_string();
    advance_to_quiescing(
        &fixture.socket,
        21,
        &replacement_id,
        "ik_79-ck-orphan-advance",
    );
    shutdown(daemon);

    let mut crashed = fixture.spawn(Some("lane-checkpoint.after-intent"));
    wait_ready(&fixture);
    let capture = observation("implementer", None);
    rpc_unchecked(
        &fixture.socket,
        &fresh_id(22),
        "lane.checkpoint.create",
        Some(checkpoint_params(
            &replacement_id,
            &capture,
            "ik_79-ck-orphan-capture",
        )),
    );
    wait_crash(&mut crashed);
    let orphan = fixture.checkpoints_dir().join(format!(
        "{}.brief",
        canter::state::checkpoint_id_for(&replacement_id)
    ));
    fs::write(&orphan, b"orphan artifact without a committed record\n").expect("plant orphan");
    let orphan_bytes = fs::read(&orphan).expect("orphan bytes");

    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let parked = status_of(&fixture.socket, 23, &replacement_id);
    assert_eq!(
        field_str(replacement_of(&parked), "outcome"),
        "ambiguous",
        "an artifact without a commit fails closed"
    );
    assert_eq!(
        replacement_of(&parked).get("next_allowed"),
        Some(&null()),
        "the parked record can never advance"
    );
    assert_eq!(
        fs::read(&orphan).expect("orphan preserved"),
        orphan_bytes,
        "the inconsistent artifact is preserved for external reconciliation"
    );
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC (+ probe P1): the generation fence refuses before any effect.
// ---------------------------------------------------------------------------

#[test]
fn stale_generation_evidence_refuses_before_any_effect() {
    let fixture = Fixture::new("stale-generation");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let (replacement_id, digest) =
        retired_record(&fixture, 1, "lane-79-gen", "implementer", None, "gen");
    let before = status_of(&fixture.socket, 10, &replacement_id);
    let invocations_before = fixture.invocations();

    // A stale generation can never start a successor.
    let id = fresh_id(11);
    let stale_start = object(vec![
        ("idempotency_key", string(&format!("ik_start-{id}"))),
        ("replacement_id", string(&replacement_id)),
        (
            "binding",
            object(vec![
                ("generation", integer(2)),
                ("checkpoint_digest", string(&digest)),
                ("nonce", string("nonce-0001")),
            ]),
        ),
        (
            "successor",
            object(vec![
                ("session", string(SUCCESSOR_SESSION)),
                ("kickoff_receipt", string(&kickoff_receipt())),
            ]),
        ),
        ("harness", harness_pi()),
        ("admission", live_admission()),
    ]);
    let (code, message) = rpc_err(&fixture.socket, &id, "lane.start", Some(stale_start));
    assert_eq!(code, "refusal.successor.binding", "{message}");
    assert!(
        message.contains("generation"),
        "the refusal names the generation fence: {message}"
    );
    assert_eq!(
        fixture.invocations(),
        invocations_before,
        "a stale generation invokes nothing"
    );
    assert_eq!(
        status_of(&fixture.socket, 12, &replacement_id),
        before,
        "the record is untouched"
    );

    // A stale generation can never advance (compare-and-set) ...
    let pending = request_replacement(
        &fixture,
        20,
        "lane-79-gen-pending",
        "implementer",
        "ik_79-gen-pending-request",
    );
    let pending_id = field_str(replacement_of(&pending), "replacement_id").to_string();
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(21),
        "lane.replacement.advance",
        Some(object(vec![
            ("idempotency_key", string("ik_79-gen-pending-advance")),
            ("replacement_id", string(&pending_id)),
            ("expected_phase", string("requested")),
            ("generation", integer(2)),
        ])),
    );
    assert_eq!(code, "refusal.replacement.stale", "{message}");

    // ... and a stale generation can never retire.
    let (replacement_id, digest) = checkpointed_record(
        &fixture,
        30,
        "lane-79-gen-retire",
        "implementer",
        None,
        "gen2",
    );
    let interrupts_before = interrupt_count(&fixture);
    let id = fresh_id(40);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.retire",
        Some(retire_params(
            &id,
            &replacement_id,
            2,
            SOURCE_SESSION,
            SOURCE_PROCESS,
            &digest,
        )),
    );
    assert_eq!(code, "refusal.retirement.binding", "{message}");
    assert_eq!(
        interrupt_count(&fixture),
        interrupts_before,
        "a stale generation signals nothing"
    );

    // The exact generation still works: the fence is not a blanket block.
    let retirement = retire_ok(
        &fixture.socket,
        &fresh_id(41),
        &replacement_id,
        1,
        SOURCE_SESSION,
        SOURCE_PROCESS,
        &digest,
    );
    assert_eq!(
        field_str(retirement.get("replacement").expect("replacement"), "phase"),
        "retired",
        "the exact generation retires"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC (+ probes P2a/P2b): the checkpoint-integrity fence refuses before any
// effect.
// ---------------------------------------------------------------------------

#[test]
fn changed_checkpoint_evidence_refuses_start_and_retire() {
    let fixture = Fixture::new("changed-checkpoint");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // A start whose checkpoint digest moved refuses typed, before any effect.
    let (replacement_id, digest) =
        retired_record(&fixture, 1, "lane-79-digest", "implementer", None, "dg");
    let before = status_of(&fixture.socket, 10, &replacement_id);
    let invocations_before = fixture.invocations();
    let id = fresh_id(11);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &replacement_id,
            &"f".repeat(64),
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    );
    assert_eq!(code, "refusal.successor.binding", "{message}");
    assert!(
        message.contains("digest"),
        "the refusal names the checkpoint digest: {message}"
    );
    assert_eq!(
        fixture.invocations(),
        invocations_before,
        "changed evidence invokes nothing"
    );
    assert_eq!(status_of(&fixture.socket, 12, &replacement_id), before);
    // The exact digest still starts: the fence is not a blanket block.
    let started = start_ok(
        &fixture.socket,
        &fresh_id(13),
        &replacement_id,
        &digest,
        "nonce-0001",
    );
    assert_eq!(
        field_str(replacement_of(start_of(&started)), "phase"),
        "adopting"
    );

    // A retirement whose checkpoint digest moved refuses typed, before any
    // signal.
    let (replacement_id, digest) = checkpointed_record(
        &fixture,
        20,
        "lane-79-digest-retire",
        "implementer",
        None,
        "dgr",
    );
    let interrupts_before = interrupt_count(&fixture);
    let id = fresh_id(30);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.retire",
        Some(retire_params(
            &id,
            &replacement_id,
            1,
            SOURCE_SESSION,
            SOURCE_PROCESS,
            &"f".repeat(64),
        )),
    );
    assert_eq!(code, "refusal.retirement.binding", "{message}");
    assert_eq!(
        interrupt_count(&fixture),
        interrupts_before,
        "changed evidence signals nothing"
    );
    let retirement = retire_ok(
        &fixture.socket,
        &fresh_id(31),
        &replacement_id,
        1,
        SOURCE_SESSION,
        SOURCE_PROCESS,
        &digest,
    );
    assert_eq!(
        field_str(retirement.get("replacement").expect("replacement"), "phase"),
        "retired"
    );
    assert_eq!(
        interrupt_count(&fixture),
        interrupts_before + 1,
        "exactly the one bounded stop of the correct-digest retirement"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC (+ probe P3): a pause injected at each transition is sticky across
// restart; no successor starts or begins work while paused.
// ---------------------------------------------------------------------------

#[test]
fn pause_injected_at_each_transition_is_sticky_across_restart() {
    let fixture = Fixture::new("pause-sticky");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // A: held at `requested` — advancement is refused.
    let requested = request_replacement(
        &fixture,
        1,
        "lane-79-pause-a",
        "implementer",
        "ik_79-pa-request",
    );
    let a_id = field_str(replacement_of(&requested), "replacement_id").to_string();
    hold(&fixture, 2, &a_id, "human decision pending");
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(3),
        "lane.replacement.advance",
        Some(object(vec![
            ("idempotency_key", string("ik_79-pa-advance")),
            ("replacement_id", string(&a_id)),
            ("expected_phase", string("requested")),
            ("generation", integer(1)),
        ])),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");

    // B: held at `quiescing` — the checkpoint capture is refused.
    let b = request_replacement(
        &fixture,
        10,
        "lane-79-pause-b",
        "implementer",
        "ik_79-pb-request",
    );
    let b_id = field_str(replacement_of(&b), "replacement_id").to_string();
    advance_to_quiescing(&fixture.socket, 11, &b_id, "ik_79-pb-advance");
    hold(&fixture, 12, &b_id, "human decision pending");
    let observation = observation("implementer", None);
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(13),
        "lane.checkpoint.create",
        Some(checkpoint_params(&b_id, &observation, "ik_79-pb-capture")),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");

    // C: held at `checkpointed` — the retirement is refused and nothing is
    // signalled.
    let (c_id, c_digest) =
        checkpointed_record(&fixture, 20, "lane-79-pause-c", "implementer", None, "pc");
    hold(&fixture, 30, &c_id, "human decision pending");
    let id = fresh_id(31);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.retire",
        Some(retire_params(
            &id,
            &c_id,
            1,
            SOURCE_SESSION,
            SOURCE_PROCESS,
            &c_digest,
        )),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");
    assert_eq!(
        interrupt_count(&fixture),
        0,
        "a paused lane signals nothing"
    );

    // D: held at `retired` — no successor starts.
    let (d_id, d_digest) =
        retired_record(&fixture, 40, "lane-79-pause-d", "implementer", None, "pd");
    hold(&fixture, 50, &d_id, "human decision pending");
    let id = fresh_id(51);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &d_id,
            &d_digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");
    assert_eq!(spawn_count(&fixture), 0, "a paused lane launches nothing");

    // E: held between start and adoption — the booted successor stays
    // fenced (never activated).
    let (e_id, _e_digest, e_successor) =
        started_record(&fixture, 60, "lane-79-pause-e", "implementer", None, "pe");
    hold(&fixture, 70, &e_id, "human decision pending");
    let invocations_before = fixture.invocations().len();
    let id = fresh_id(71);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.adopt",
        Some(adopt_params(&id, &e_id, &e_successor, "implementer")),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");
    assert_eq!(
        fixture.invocations().len(),
        invocations_before,
        "a paused activation invokes nothing"
    );
    let parked = status_of(&fixture.socket, 72, &e_id);
    assert_eq!(
        field_str(successor_of(&parked), "adopted_at"),
        "",
        "the booted successor remains fenced (never activated)"
    );
    let spawns_before_restart = spawn_count(&fixture);
    let interrupts_before_restart = interrupt_count(&fixture);

    // Restart: the pause is durable. No successor starts or begins work
    // while paused (the fixture is disposable; no real fleet pause exists
    // or is lifted).
    shutdown(daemon);
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    for (label, replacement_id) in [
        ("requested", &a_id),
        ("quiescing", &b_id),
        ("checkpointed", &c_id),
        ("retired", &d_id),
        ("adopting", &e_id),
    ] {
        let status = status_of(&fixture.socket, 80, replacement_id);
        assert_eq!(
            field_str(replacement_of(&status), "outcome"),
            "held",
            "{label}: the pause is sticky across the restart"
        );
        assert_eq!(
            replacement_of(&status).get("next_allowed"),
            Some(&null()),
            "{label}: a held record can never advance"
        );
    }
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(81),
        "lane.replacement.advance",
        Some(object(vec![
            ("idempotency_key", string("ik_79-pa-advance-2")),
            ("replacement_id", string(&a_id)),
            ("expected_phase", string("requested")),
            ("generation", integer(1)),
        ])),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(82),
        "lane.checkpoint.create",
        Some(checkpoint_params(&b_id, &observation, "ik_79-pb-capture-2")),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");
    let id = fresh_id(83);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.retire",
        Some(retire_params(
            &id,
            &c_id,
            1,
            SOURCE_SESSION,
            SOURCE_PROCESS,
            &c_digest,
        )),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");
    let id = fresh_id(84);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &d_id,
            &d_digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");
    let id = fresh_id(85);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.adopt",
        Some(adopt_params(&id, &e_id, &e_successor, "implementer")),
    );
    assert_eq!(code, "refusal.replacement.held", "{message}");
    assert_eq!(
        spawn_count(&fixture),
        spawns_before_restart,
        "a paused fleet started no successor across the restart"
    );
    assert_eq!(
        interrupt_count(&fixture),
        interrupts_before_restart,
        "a paused fleet signalled nothing"
    );

    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC: the profile-switch fixture distinguishes requested model, actual
// binding and permitted fallback — no network credentials.
// ---------------------------------------------------------------------------

#[test]
fn profile_switch_fixture_distinguishes_requested_actual_and_permitted_fallback() {
    let fixture = Fixture::new("profile-switch");
    fixture.set_mode("binding-match");
    fixture.write_fake_workspace();
    fixture.write_config(PROFILE_CONFIG);

    // The reviewed plan comes from the real `config show --json` preview
    // under an environment with NO credential set: the fixture needs none.
    let (plan, credentials, preview_raw) = fixture.config_show();
    let revision = field_str(&plan, "revision").to_string();
    assert_eq!(revision.len(), 64, "the plan is revision-fingerprinted");
    assert_eq!(field_str(&plan, "key"), "lane-orch-1");
    assert_eq!(field_str(&plan, "kind"), "pi");
    assert_eq!(field_str(&plan, "provider"), "example-provider");
    assert_eq!(field_str(&plan, "model"), "example-model");
    assert_eq!(
        string_array(&plan, "fallbacks"),
        vec!["example-fallback-provider/example-fallback-model"],
        "the permitted fallback pair is declared, not inferred"
    );
    assert_eq!(
        string_array(&credentials, "missing"),
        vec!["EXAMPLE_PROVIDER_KEY"],
        "no credential is present; the fixture requires none: {preview_raw}"
    );

    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // The requested pair, the actual binding and the fallback stay DISTINCT
    // fields through the whole chain.
    let (replacement_id, digest) =
        retired_record_with_profile(&fixture, 1, "lane-79-profile-match", &plan, "pm");
    let started = start_with_profile(
        &fixture,
        &fresh_id(10),
        &replacement_id,
        &digest,
        "nonce-0001",
        &plan,
    );
    let binding = start_of(&started)
        .get("verification")
        .expect("verification")
        .get("binding")
        .expect("binding")
        .clone();
    assert_eq!(field_str(&binding, "status"), "matched");
    assert_eq!(field_str(&binding, "revision"), revision);
    let intended = binding.get("intended").expect("intended");
    let actual = binding.get("actual").expect("actual");
    assert_eq!(field_str(intended, "provider"), "example-provider");
    assert_eq!(field_str(intended, "model"), "example-model");
    assert_eq!(
        field_str(actual, "provider"),
        "example-provider",
        "the actual binding comes from the adapter read-back, never the request"
    );
    assert_eq!(field_str(&binding, "source"), "adapter");
    let Val::Obj(binding_keys) = &binding else {
        panic!("binding is an object");
    };
    let mut keys: Vec<&str> = binding_keys.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "actual",
            "configured_limits",
            "intended",
            "introspection",
            "revision",
            "source",
            "status"
        ],
        "the binding document reports exactly the closed observation surface"
    );
    let status = status_of(&fixture.socket, 11, &replacement_id);
    assert_eq!(
        field_str(status.get("profile").expect("profile echo"), "revision"),
        revision,
        "the durable plan echo is the reviewed revision (no live profile switch)"
    );

    // The permitted fallback is accepted and reported distinctly.
    fixture.set_mode("binding-fallback");
    let (replacement_id, digest) =
        retired_record_with_profile(&fixture, 20, "lane-79-profile-fallback", &plan, "pf");
    let started = start_with_profile(
        &fixture,
        &fresh_id(30),
        &replacement_id,
        &digest,
        "nonce-0001",
        &plan,
    );
    let binding = start_of(&started)
        .get("verification")
        .expect("verification")
        .get("binding")
        .expect("binding")
        .clone();
    assert_eq!(field_str(&binding, "status"), "fallback");
    assert_eq!(
        field_str(binding.get("actual").expect("actual"), "provider"),
        "example-fallback-provider",
        "the permitted fallback is the actual binding"
    );
    assert_eq!(
        field_str(binding.get("intended").expect("intended"), "provider"),
        "example-provider",
        "intended and actual stay distinct fields"
    );

    // Anything outside the planned binding and the permitted fallback stays
    // fenced: no evidence, never adopted.
    fixture.set_mode("binding-unexpected");
    let (replacement_id, digest) =
        retired_record_with_profile(&fixture, 40, "lane-79-profile-unexpected", &plan, "pu");
    let id = fresh_id(50);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            Some(plan.clone()),
        )),
    );
    assert_eq!(code, "refusal.successor.reused", "{message}");
    let parked = status_of(&fixture.socket, 51, &replacement_id);
    assert_eq!(field_str(replacement_of(&parked), "outcome"), "ambiguous");
    let successor = successor_of(&parked);
    assert_eq!(successor.get("evidence"), Some(&Val::Null));
    assert_eq!(field_str(successor, "adopted_at"), "");

    shutdown(daemon);
}

/// Request + advance + checkpoint + retire one replacement under the
/// reviewed profile plan.
fn retired_record_with_profile(
    fixture: &Fixture,
    id_seed: u32,
    lane: &str,
    plan: &Val,
    key_prefix: &str,
) -> (String, String) {
    let requested = request_replacement_for(
        fixture,
        id_seed,
        lane,
        "implementer",
        SOURCE_SESSION,
        SOURCE_PROCESS,
        Some(plan.clone()),
        &format!("ik_{key_prefix}-request"),
    );
    let replacement_id = field_str(replacement_of(&requested), "replacement_id").to_string();
    assert_eq!(
        field_str(requested.get("profile").expect("profile echo"), "revision"),
        field_str(plan, "revision"),
        "the request echoes the reviewed revision"
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
        "implementer",
        None,
        &format!("ik_{key_prefix}-capture"),
    );
    retire_ok(
        &fixture.socket,
        &fresh_id(id_seed + 3),
        &replacement_id,
        1,
        SOURCE_SESSION,
        SOURCE_PROCESS,
        &digest,
    );
    (replacement_id, digest)
}

/// Start one verified successor under the reviewed profile plan.
fn start_with_profile(
    fixture: &Fixture,
    id: &str,
    replacement_id: &str,
    digest: &str,
    nonce: &str,
    plan: &Val,
) -> Val {
    rpc_ok(
        &fixture.socket,
        id,
        "lane.start",
        Some(start_params(
            id,
            replacement_id,
            digest,
            nonce,
            SUCCESSOR_SESSION,
            live_admission(),
            Some(plan.clone()),
        )),
    )
}

// ---------------------------------------------------------------------------
// AC: an expired (stale) authorization and an unverifiable successor hold
// with no effect.
// ---------------------------------------------------------------------------

#[test]
fn expired_authorization_and_unverified_successors_hold_without_effect() {
    let fixture = Fixture::new("expired-auth");
    fixture.set_mode("ready");
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let (replacement_id, digest) =
        retired_record(&fixture, 1, "lane-79-auth", "implementer", None, "au");
    let before = status_of(&fixture.socket, 10, &replacement_id);
    let invocations_before = fixture.invocations();

    // An EXPIRED (stale) host-resource authorization refuses typed with no
    // effect: unknown/stale measurements never admit new work.
    let stale_at = rfc3339_from_unix(unix_now() - 3600);
    let id = fresh_id(11);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            admission((8, 4, 4), &stale_at),
            None,
        )),
    );
    assert_eq!(code, "refusal.admission.proof_stale", "{message}");
    assert_eq!(
        fixture.invocations(),
        invocations_before,
        "an expired authorization spawns nothing"
    );
    assert_eq!(status_of(&fixture.socket, 12, &replacement_id), before);

    // A missing authorization bundle is a typed hold too.
    let id = fresh_id(13);
    let mut missing = start_params(
        &id,
        &replacement_id,
        &digest,
        "nonce-0001",
        SUCCESSOR_SESSION,
        live_admission(),
        None,
    );
    if let Val::Obj(map) = &mut missing {
        map.remove("admission");
    }
    let (code, message) = rpc_err(&fixture.socket, &id, "lane.start", Some(missing));
    assert_eq!(code, "refusal.admission.proof_missing", "{message}");

    // A fresh authorization is a bounded explicit retry (admission is never
    // disabled by the refusal).
    let started = start_ok(
        &fixture.socket,
        &fresh_id(14),
        &replacement_id,
        &digest,
        "nonce-0001",
    );
    assert_eq!(
        field_str(replacement_of(start_of(&started)), "phase"),
        "adopting",
        "a fresh authorization admits the bounded retry"
    );
    assert_eq!(spawn_count(&fixture), 1);

    // An unverifiable successor read-back holds the boundary with the spawn
    // already counted; the same-nonce retry re-verifies WITHOUT spawning
    // again.
    let (replacement_id, digest) =
        retired_record(&fixture, 20, "lane-79-held", "implementer", None, "he");
    fixture.set_mode("malformed");
    let id = fresh_id(30);
    let (code, message) = rpc_err(
        &fixture.socket,
        &id,
        "lane.start",
        Some(start_params(
            &id,
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            live_admission(),
            None,
        )),
    );
    assert_eq!(code, "refusal.successor.held", "{message}");
    let held = status_of(&fixture.socket, 31, &replacement_id);
    assert_eq!(
        field_str(replacement_of(&held), "phase"),
        "starting",
        "an unverifiable successor never leaves the starting boundary"
    );
    let spawns_after_hold = spawn_count(&fixture);
    assert_eq!(spawns_after_hold, 2, "one bounded spawn attempt per record");
    fixture.set_mode("ready");
    let started = start_ok(
        &fixture.socket,
        &fresh_id(32),
        &replacement_id,
        &digest,
        "nonce-0001",
    );
    assert_eq!(
        field_str(replacement_of(start_of(&started)), "phase"),
        "adopting",
        "the same-nonce retry re-verifies the committed boundary"
    );
    assert_eq!(
        spawn_count(&fixture),
        spawns_after_hold,
        "the re-verification never spawns a second session"
    );
    let successor_id = field_str(successor_of(start_of(&started)), "successor_id").to_string();
    let adopted = adopt_ok(
        &fixture.socket,
        &fresh_id(33),
        &replacement_id,
        &successor_id,
        "implementer",
    );
    assert_eq!(
        field_str(replacement_of(adoption_of(&adopted)), "phase"),
        "adopted"
    );

    shutdown(daemon);
}
