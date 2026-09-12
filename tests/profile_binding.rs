//! Issue #77 acceptance integration tests over the real binary and socket:
//! ONE bounded daemon-coordinated replacement requested under an EXPLICIT
//! profile-configuration revision (the CLI `config show` preview), the
//! durable target-profile plan it binds, and the verification of what the
//! successor ACTUALLY bound through the adapter read-back — intended vs
//! actual, an authorized fallback reported distinctly, an unexpected
//! provider/model fenced, an unknown actual binding that stays unknown
//! (never a copy of the requested configuration), configured limits reported
//! as configured limits, an honest capability hold when the profile cannot
//! introspect, credential names without value disclosure, the
//! changed-configuration invalidation that requires a newly reviewed plan,
//! and the restart during adoption that preserves the binding.
//!
//! Every test spawns `canter daemon run` as a child process with isolated
//! XDG state and an explicit socket under a per-test temp dir; the workspace
//! executable the start/adopt/retire paths drive is a fake `herdr` recorded
//! in a per-fixture invocation log, on a PATH that contains nothing else.
//! The per-test `config show` previews run the same binary against a
//! synthetic config under an isolated HOME. Nothing here touches the real
//! host state, a real session, the service manager, or the network; all
//! identities and values are synthetic.

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

/// The synthetic credential VALUE used by the disclosure tests: it must
/// never appear in any plan, report, or log.
const SECRET_VALUE: &str = "example-secret-material-7713";

/// The fake workspace executable. `$HOME/workspace-mode` selects what the
/// successor session reports: the planned pair (`binding-match`), an
/// authorized fallback pair (`binding-fallback`), an unexpected pair
/// (`binding-unexpected`), an incomplete pair (`binding-partial`), or no
/// binding at all (`no-binding`). The source row always reads retired.
const FAKE_WORKSPACE: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin
log="$HOME/workspace-invocations.log"
printf '%s\n' "$*" >> "$log"
mode="$(cat "$HOME/workspace-mode" 2>/dev/null || echo binding-match)"
case "$1 $2" in
  "session interrupt")
    printf '%s\n' '{"interrupted":true}' ;;
  "session start")
    printf '%s\n' '{"started":true}' ;;
  "session show")
    if [ "$3" = "sess-0001" ]; then
      printf '%s\n' '{"session_id":"sess-0001","state":"retired","process":null,"registration":{"state":"released","session":"sess-0001","generation":1}}'
    else
      case "$mode" in
        binding-match) binding=',"binding":{"provider":"example-provider","model":"example-model"}' ;;
        binding-fallback) binding=',"binding":{"provider":"example-fallback-provider","model":"example-fallback-model"}' ;;
        binding-unexpected) binding=',"binding":{"provider":"example-rogue-provider","model":"example-rogue-model"}' ;;
        binding-partial) binding=',"binding":{"provider":"example-provider"}' ;;
        *) binding='' ;;
      esac
      printf '%s\n' "{\"session_id\":\"sess-0002\",\"state\":\"working\",\"process\":\"proc-0002\",\"role\":\"implementer\",\"profile\":{\"key\":\"lane-orch-1\",\"kind\":\"pi\"},\"cwd\":\"worktrees/issues/77\",\"kickoff_receipt\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"readiness\":\"ready\"${binding}}"
    fi ;;
  *) echo '{"error":"unknown row"}' >&2; exit 4 ;;
esac
"#;

/// The synthetic config the human reviews before requesting the replacement:
/// every issue #77 key is exercised with fictional values.
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

/// The edited configuration (the provider moved after the preview).
const EDITED_CONFIG: &str = r#"schema = "hf-config/v1"
[repository.widgets]
origin = "https://github.com/example-org/widgets"
branch = "staging"
[harness.lane-orch-1]
kind = "pi"
executable = "pi-example"
env_allow = ["PATH", "EXAMPLE_PROVIDER_KEY"]
provider = "example-provider-2"
model = "example-model"
fallback = ["example-fallback-provider/example-fallback-model"]
secret_env = ["EXAMPLE_PROVIDER_KEY"]
binding_introspection = true
[harness.lane-orch-1.limits]
context_tokens = 131072
"#;

/// A profile whose adapter cannot introspect the bound provider/model.
const NO_INTROSPECTION_CONFIG: &str = r#"schema = "hf-config/v1"
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
binding_introspection = false
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
        let dir = std::env::temp_dir().join(format!("hf-pb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        Fixture {
            state_dir: dir.join("state"),
            socket: dir.join("daemon.sock"),
            dir,
        }
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

    /// Select the fake workspace's successor read-back behavior.
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

    /// Write the reviewed config at the XDG default path under the fixture
    /// HOME (the exact path `canter config show` discovers).
    fn write_config(&self, text: &str) {
        let path = self.dir.join(".config").join("canter").join("config.toml");
        std::fs::create_dir_all(path.parent().expect("config dir")).expect("config dir");
        std::fs::write(&path, text).expect("write config");
    }

    /// Run the real `config show --json` preview and return
    /// `(profile document, credentials status, raw stdout)`. The credential
    /// environment variable is set only when `secret` is supplied.
    fn config_show(&self, secret: Option<&str>) -> (Val, Val, String) {
        let mut command = Command::new(bin());
        command
            .args(["config", "show", "--json"])
            .env_clear()
            .env("PATH", self.bin_dir())
            .env("HOME", &self.dir)
            .env("LANG", "C");
        if let Some(secret) = secret {
            command.env("EXAMPLE_PROVIDER_KEY", secret);
        }
        let out = command.output().expect("run config show");
        let raw = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(
            out.status.code(),
            Some(0),
            "config show must succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let envelope = Val::parse_json(raw.trim()).unwrap_or_else(|err| {
            panic!("config show envelope parse: {err}\n{raw}");
        });
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
        let stderr_file = std::fs::File::create(&stderr_path).expect("stderr log");
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

// ---------------------------------------------------------------------------
// Synthetic request builders (public-data boundary: no host paths)
// ---------------------------------------------------------------------------

const WORKTREE: &str = "worktrees/issues/77";
const SOURCE_SESSION: &str = "sess-0001";
const SOURCE_PROCESS: &str = "proc-0001";
const SUCCESSOR_SESSION: &str = "sess-0002";
const HEAD: &str = "1111111111111111111111111111111111111111";
const BASE: &str = "2222222222222222222222222222222222222222";
const REVIEWED_SHA: &str = "5555555555555555555555555555555555555555";

fn kickoff_receipt() -> String {
    "a".repeat(64)
}

fn replacement_params(lane: &str, role: &str, key: &str, profile: Option<Val>) -> Val {
    let mut fields = vec![
        ("idempotency_key", string(key)),
        ("lane_id", string(lane)),
        ("generation", integer(1)),
        ("source_session", string(SOURCE_SESSION)),
        ("source_process", string(SOURCE_PROCESS)),
        ("role", string(role)),
        ("worktree", string(WORKTREE)),
        ("reason", string("profile-bound handoff window")),
    ];
    if let Some(profile) = profile {
        fields.push(("profile", profile));
    }
    object(fields)
}

/// A valid synthetic checkpoint observation for one lane.
fn observation(role: &str) -> Val {
    object(vec![
        ("role", string(role)),
        ("task", string("issue-77 profile binding capture")),
        ("worktree", string(WORKTREE)),
        ("branch", string("issue-77-profile-binding")),
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
                ("count", integer(0)),
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
                ("status", string("passed")),
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
    ])
}

/// The fresh adoption re-query: the closed comparison contract.
fn adoption_observation(role: &str) -> Val {
    let capture = observation(role);
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

fn replacement_of(result: &Val) -> &Val {
    result.get("replacement").expect("replacement")
}

fn start_of(result: &Val) -> &Val {
    result.get("start").expect("start")
}

fn field_str<'a>(val: &'a Val, key: &str) -> &'a str {
    val.get(key).and_then(Val::as_str).expect(key)
}

fn request_replacement(
    socket: &Path,
    id_seed: u32,
    lane: &str,
    key: &str,
    profile: Option<Val>,
) -> Val {
    rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.request",
        Some(replacement_params(lane, "implementer", key, profile)),
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

fn capture_checkpoint(socket: &Path, id_seed: u32, replacement_id: &str, key: &str) -> String {
    let observation = observation("implementer");
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
    field_str(result.get("checkpoint").expect("checkpoint"), "digest").to_string()
}

fn status_of(socket: &Path, id_seed: u32, replacement_id: &str) -> Val {
    rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.status",
        Some(object(vec![("replacement_id", string(replacement_id))])),
    )
}

fn harness_pi() -> Val {
    object(vec![("key", string("lane-orch-1")), ("kind", string("pi"))])
}

fn retire(socket: &Path, id: &str, replacement_id: &str, digest: &str) {
    let result = rpc_ok(
        socket,
        id,
        "lane.retire",
        Some(object(vec![
            ("idempotency_key", string(&format!("ik_retire-{id}"))),
            ("replacement_id", string(replacement_id)),
            (
                "binding",
                object(vec![
                    ("generation", integer(1)),
                    ("session", string(SOURCE_SESSION)),
                    ("process", string(SOURCE_PROCESS)),
                    ("checkpoint_digest", string(digest)),
                ]),
            ),
            (
                "recheck",
                object(vec![
                    ("observed_at", string("2026-09-12T00:00:00Z")),
                    ("session", string(SOURCE_SESSION)),
                    ("process", string(SOURCE_PROCESS)),
                    ("children", Val::Arr(vec![])),
                    ("active", bool_(false)),
                ]),
            ),
            ("harness", harness_pi()),
        ])),
    );
    assert_eq!(
        field_str(
            replacement_of(result.get("retirement").expect("retirement")),
            "phase"
        ),
        "retired",
        "the source retirement must commit before the start"
    );
}

/// Drive the full pre-start chain: request (under `profile`, when supplied)
/// → advance → checkpoint → retire. Returns `(replacement_id, digest)`.
fn retired_record(
    fixture: &Fixture,
    id_seed: u32,
    lane: &str,
    role: &str,
    profile: Option<Val>,
    key_prefix: &str,
) -> (String, String) {
    let requested = rpc_ok(
        &fixture.socket,
        &fresh_id(id_seed),
        "lane.replacement.request",
        Some(replacement_params(
            lane,
            role,
            &format!("ik_{key_prefix}-request"),
            profile.clone(),
        )),
    );
    let replacement_id = field_str(replacement_of(&requested), "replacement_id").to_string();
    if let Some(profile) = &profile {
        let echoed = requested.get("profile").expect("profile echo");
        assert_eq!(
            field_str(echoed, "revision"),
            field_str(profile, "revision"),
            "the request echoes the reviewed revision"
        );
    } else {
        assert_eq!(
            requested.get("profile"),
            Some(&Val::Null),
            "an unbound request carries no plan"
        );
    }
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
        &format!("ik_{key_prefix}-capture"),
    );
    retire(
        &fixture.socket,
        &fresh_id(id_seed + 3),
        &replacement_id,
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
        ("admission", live_admission()),
    ];
    if let Some(profile) = profile {
        fields.push(("profile", profile));
    }
    object(fields)
}

fn start(
    socket: &Path,
    id: &str,
    replacement_id: &str,
    digest: &str,
    nonce: &str,
    profile: Option<Val>,
) -> Val {
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
            profile,
        )),
    )
}

fn adopt_params(id: &str, replacement_id: &str, successor_id: &str) -> Val {
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
        ("observation", adoption_observation("implementer")),
        ("reobservation", adoption_observation("implementer")),
        ("harness", harness_pi()),
    ])
}

fn adopt(socket: &Path, id: &str, replacement_id: &str, successor_id: &str) -> Val {
    rpc_ok(
        socket,
        id,
        "lane.adopt",
        Some(adopt_params(id, replacement_id, successor_id)),
    )
}

/// The binding document of one successful adoption result
/// (`result.adoption.adoption.binding`).
fn adoption_binding(result: &Val) -> &Val {
    result
        .get("adoption")
        .expect("adoption")
        .get("adoption")
        .expect("adoption document")
        .get("binding")
        .expect("binding")
}

/// The verification binding document of one successful start result.
fn start_binding(start: &Val) -> &Val {
    start
        .get("verification")
        .expect("verification")
        .get("binding")
        .expect("binding")
}

// ---------------------------------------------------------------------------
// AC1 (unchanged profile) + AC2 + AC3 + AC4 (planned pair) + AC6 (limits):
// one replacement requested under the explicit `config show` revision binds
// the durable plan; the successor is verified against it; the actual binding
// is recorded from adapter evidence; the source is untouched until normal
// quiescence/retirement.
// ---------------------------------------------------------------------------

#[test]
fn reviewed_profile_plan_binds_the_replacement_and_verifies_the_actual_binding() {
    let fixture = Fixture::new("profile-happy");
    fixture.set_mode("binding-match");
    fixture.write_fake_workspace();
    fixture.write_config(PROFILE_CONFIG);
    let (plan, credentials, preview_raw) = fixture.config_show(Some(SECRET_VALUE));
    let revision = field_str(&plan, "revision").to_string();
    assert_eq!(
        credentials.get("missing"),
        Some(&Val::Arr(vec![])),
        "the declared credential is present: {credentials:?}"
    );
    assert!(
        !preview_raw.contains(SECRET_VALUE),
        "the preview never discloses the credential value"
    );

    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // AC1: the human requests the replacement under the explicit revision.
    let requested = request_replacement(
        &fixture.socket,
        1,
        "lane-1",
        "ik_profile-request",
        Some(plan.clone()),
    );
    let replacement_id = field_str(replacement_of(&requested), "replacement_id").to_string();
    assert_eq!(
        field_str(requested.get("profile").expect("profile echo"), "revision"),
        revision,
        "the request echoes the reviewed revision"
    );

    // The durable status exposes the target profile identity/fingerprint and
    // the intended pair — the plan a later start is fenced on.
    let status = status_of(&fixture.socket, 2, &replacement_id);
    let bound = status.get("profile").expect("profile");
    assert_eq!(field_str(bound, "revision"), revision);
    assert_eq!(
        field_str(bound.get("profile").expect("document"), "provider"),
        "example-provider"
    );

    // AC2: the request itself is a durable record, not an effect — the
    // source session was never touched (no workspace invocation at all).
    assert!(
        fixture.invocations().is_empty(),
        "the profile-bound request invoked nothing: {:?}",
        fixture.invocations()
    );

    advance_to_quiescing(&fixture.socket, 3, &replacement_id, "ik_profile-advance");
    let digest = capture_checkpoint(&fixture.socket, 4, &replacement_id, "ik_profile-capture");
    retire(&fixture.socket, &fresh_id(5), &replacement_id, &digest);
    let pre_start = fixture.invocations().len();

    // AC3/AC4: the successor answers with the planned pair; the verification
    // records intended and actual from authoritative adapter evidence.
    let started = start(
        &fixture.socket,
        &fresh_id(6),
        &replacement_id,
        &digest,
        "nonce-0001",
        Some(plan.clone()),
    );
    let binding = start_binding(start_of(&started));
    assert_eq!(field_str(binding, "status"), "matched");
    assert_eq!(
        field_str(binding, "revision"),
        revision,
        "the verification names the reviewed revision"
    );
    assert_eq!(
        field_str(binding.get("intended").expect("intended"), "provider"),
        "example-provider"
    );
    assert_eq!(
        field_str(binding.get("intended").expect("intended"), "model"),
        "example-model"
    );
    assert_eq!(
        field_str(binding.get("actual").expect("actual"), "provider"),
        "example-provider",
        "the actual binding comes from the adapter read-back"
    );
    assert_eq!(field_str(binding, "source"), "adapter");
    assert!(
        fixture.invocations().len() > pre_start,
        "the verified start ran the bounded workspace rows"
    );

    // AC6: the configured limits are reported as CONFIGURED limits, and the
    // binding document claims nothing else (no provider-support proof).
    let limits = binding.get("configured_limits").expect("configured_limits");
    assert_eq!(field_str(limits, "context_tokens"), "131072", "{limits:?}");
    let Val::Obj(binding_keys) = binding else {
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

    let successor_id = field_str(
        start_of(&started).get("successor").expect("successor"),
        "successor_id",
    )
    .to_string();

    // The committed successor evidence carries the same binding (durable).
    let status = status_of(&fixture.socket, 7, &replacement_id);
    let successor = status.get("successor").expect("successor");
    let evidence_binding = successor
        .get("evidence")
        .expect("evidence")
        .get("binding")
        .expect("evidence binding");
    assert_eq!(field_str(evidence_binding, "status"), "matched");

    // Adoption re-verifies the same binding and commits.
    let adopted = adopt(
        &fixture.socket,
        &fresh_id(8),
        &replacement_id,
        &successor_id,
    );
    assert_eq!(field_str(adoption_binding(&adopted), "status"), "matched");
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC1 (mid-plan edit): changing the relevant configuration after the
// preview changes the revision and invalidates the reviewed plan.
// ---------------------------------------------------------------------------

#[test]
fn changing_the_configuration_revision_requires_a_newly_reviewed_plan() {
    let fixture = Fixture::new("profile-edit");
    fixture.set_mode("binding-match");
    fixture.write_fake_workspace();
    fixture.write_config(PROFILE_CONFIG);
    let (reviewed, _, _) = fixture.config_show(Some(SECRET_VALUE));
    let reviewed_revision = field_str(&reviewed, "revision").to_string();

    // A request whose revision does not fingerprint its own material is
    // refused at the boundary (a revision is never a claim).
    let mut inconsistent = reviewed.clone();
    if let Val::Obj(map) = &mut inconsistent {
        map.insert("revision".to_string(), string(&"0".repeat(64)));
    }
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let (code, _message) = rpc_err(
        &fixture.socket,
        &fresh_id(1),
        "lane.replacement.request",
        Some(replacement_params(
            "lane-2",
            "implementer",
            "ik_edit-bogus",
            Some(inconsistent),
        )),
    );
    assert_eq!(code, "refusal.profile.revision");

    // A malformed plan (unknown key) refuses as a binding error.
    let mut malformed = reviewed.clone();
    if let Val::Obj(map) = &mut malformed {
        map.insert("extra".to_string(), string("value"));
    }
    let (code, _message) = rpc_err(
        &fixture.socket,
        &fresh_id(2),
        "lane.replacement.request",
        Some(replacement_params(
            "lane-2",
            "implementer",
            "ik_edit-malformed",
            Some(malformed),
        )),
    );
    assert_eq!(code, "refusal.profile.binding");

    // The reviewed plan binds the record.
    let (replacement_id, digest) = retired_record(
        &fixture,
        3,
        "lane-2",
        "implementer",
        Some(reviewed.clone()),
        "edit",
    );
    let pre_start = fixture.invocations().len();

    // The human edited the configuration after the preview: the fresh
    // preview carries a different revision.
    fixture.write_config(EDITED_CONFIG);
    let (edited, _, _) = fixture.config_show(Some(SECRET_VALUE));
    let edited_revision = field_str(&edited, "revision").to_string();
    assert_ne!(
        edited_revision, reviewed_revision,
        "the edited configuration produces a new revision"
    );

    // A start under the NEW revision is invalidated: nothing is spawned and
    // the record stays at `retired` (a newly reviewed plan is required).
    let (code, message) = rpc_err(
        &fixture.socket,
        &fresh_id(20),
        "lane.start",
        Some(start_params(
            &fresh_id(20),
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            Some(edited),
        )),
    );
    assert_eq!(code, "refusal.profile.revision", "{message}");
    assert_eq!(
        fixture.invocations().len(),
        pre_start,
        "an invalidated revision spawns nothing: {:?}",
        fixture.invocations()
    );
    assert_eq!(
        field_str(
            status_of(&fixture.socket, 21, &replacement_id)
                .get("replacement")
                .expect("replacement"),
            "phase"
        ),
        "retired",
        "the record is untouched by the invalidated start"
    );

    // A missing plan on a profile-bound record refuses too (a start cannot
    // silently drop the reviewed binding).
    let (code, _message) = rpc_err(
        &fixture.socket,
        &fresh_id(22),
        "lane.start",
        Some(start_params(
            &fresh_id(22),
            &replacement_id,
            &digest,
            "nonce-0001",
            SUCCESSOR_SESSION,
            None,
        )),
    );
    assert_eq!(code, "refusal.profile.binding");

    // The reviewed plan (unchanged revision) still starts and verifies.
    let started = start(
        &fixture.socket,
        &fresh_id(23),
        &replacement_id,
        &digest,
        "nonce-0001",
        Some(reviewed),
    );
    assert_eq!(
        field_str(start_binding(start_of(&started)), "status"),
        "matched"
    );
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC3: an unknown actual binding stays unknown/unverified — never a copy of
// the requested configuration.
// ---------------------------------------------------------------------------

#[test]
fn unknown_actual_binding_stays_unknown_and_is_never_copied() {
    let fixture = Fixture::new("profile-unknown");
    fixture.set_mode("no-binding");
    fixture.write_fake_workspace();
    fixture.write_config(NO_INTROSPECTION_CONFIG);
    let (plan, _, _) = fixture.config_show(Some(SECRET_VALUE));
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let (replacement_id, digest) = retired_record(
        &fixture,
        1,
        "lane-3",
        "implementer",
        Some(plan.clone()),
        "unknown",
    );
    let started = start(
        &fixture.socket,
        &fresh_id(10),
        &replacement_id,
        &digest,
        "nonce-0001",
        Some(plan.clone()),
    );
    let binding = start_binding(start_of(&started));
    assert_eq!(
        field_str(binding, "status"),
        "unknown",
        "a profile without binding introspection records the actual as unknown"
    );
    assert_eq!(
        binding.get("actual"),
        Some(&Val::Null),
        "the unknown actual binding is null — NEVER a copy of the requested pair"
    );
    assert_eq!(
        field_str(binding.get("intended").expect("intended"), "provider"),
        field_str(&plan, "provider"),
        "the intended pair is still recorded"
    );
    assert_eq!(binding.get("source"), Some(&Val::Null), "no adapter claim");
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC4 (permitted fallback): an authorized fallback pair is accepted and
// reported distinctly.
// ---------------------------------------------------------------------------

#[test]
fn authorized_fallback_is_accepted_and_reported_distinctly() {
    let fixture = Fixture::new("profile-fallback");
    fixture.set_mode("binding-fallback");
    fixture.write_fake_workspace();
    fixture.write_config(PROFILE_CONFIG);
    let (plan, _, _) = fixture.config_show(Some(SECRET_VALUE));
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let (replacement_id, digest) = retired_record(
        &fixture,
        1,
        "lane-4",
        "implementer",
        Some(plan.clone()),
        "fallback",
    );
    let started = start(
        &fixture.socket,
        &fresh_id(10),
        &replacement_id,
        &digest,
        "nonce-0001",
        Some(plan.clone()),
    );
    let binding = start_binding(start_of(&started));
    assert_eq!(
        field_str(binding, "status"),
        "fallback",
        "the configured fallback is reported distinctly"
    );
    assert_eq!(
        field_str(binding.get("actual").expect("actual"), "provider"),
        "example-fallback-provider"
    );
    assert_eq!(
        field_str(binding.get("intended").expect("intended"), "provider"),
        "example-provider",
        "intended and actual stay distinct fields"
    );
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC4 (unexpected fallback): a provider/model outside the planned binding
// and the authorized fallbacks stays fenced.
// ---------------------------------------------------------------------------

#[test]
fn unexpected_provider_model_is_fenced() {
    let fixture = Fixture::new("profile-unexpected");
    fixture.set_mode("binding-unexpected");
    fixture.write_fake_workspace();
    fixture.write_config(PROFILE_CONFIG);
    let (plan, _, _) = fixture.config_show(Some(SECRET_VALUE));
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let (replacement_id, digest) = retired_record(
        &fixture,
        1,
        "lane-5",
        "implementer",
        Some(plan.clone()),
        "unexpected",
    );
    let id = fresh_id(10);
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
            Some(plan.clone()),
        )),
    );
    assert_eq!(code, "refusal.successor.reused", "{message}");
    let status = status_of(&fixture.socket, 11, &replacement_id);
    assert_eq!(
        field_str(status.get("replacement").expect("replacement"), "outcome"),
        "ambiguous",
        "an unexpected binding fails closed (parked for reconciliation)"
    );
    let successor = status.get("successor").expect("successor");
    assert_eq!(
        successor.get("evidence"),
        Some(&Val::Null),
        "no verification evidence is committed under an unexpected binding"
    );
    assert_eq!(field_str(successor, "adopted_at"), "");
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC6: unsupported binding introspection yields an honest capability hold.
// ---------------------------------------------------------------------------

#[test]
fn missing_introspection_evidence_is_an_honest_capability_hold() {
    let fixture = Fixture::new("profile-hold");
    fixture.set_mode("no-binding");
    fixture.write_fake_workspace();
    fixture.write_config(PROFILE_CONFIG);
    let (plan, _, _) = fixture.config_show(Some(SECRET_VALUE));
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let (replacement_id, digest) = retired_record(
        &fixture,
        1,
        "lane-6",
        "implementer",
        Some(plan.clone()),
        "hold",
    );
    let id = fresh_id(10);
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
            Some(plan),
        )),
    );
    assert_eq!(code, "refusal.successor.held", "{message}");
    assert!(
        message.contains("capability hold"),
        "the hold names the honest capability hold: {message}"
    );
    let status = status_of(&fixture.socket, 11, &replacement_id);
    let successor = status.get("successor").expect("successor");
    assert_eq!(
        successor.get("evidence"),
        Some(&Val::Null),
        "nothing is verified under an unproven binding"
    );
    assert_eq!(field_str(successor, "adopted_at"), "");
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC5 + AC7 (missing credentials): the preview reports credential NAMES
// without disclosure, and a credential change invalidates the revision.
// ---------------------------------------------------------------------------

#[test]
fn missing_credentials_are_reported_without_disclosure() {
    let fixture = Fixture::new("profile-credentials");
    fixture.write_fake_workspace();
    fixture.write_config(PROFILE_CONFIG);

    // Without the declared credential the preview discloses the NAME only.
    let (unset_plan, credentials, raw) = fixture.config_show(None);
    assert_eq!(
        credentials.get("present"),
        Some(&Val::Arr(vec![])),
        "{credentials:?}"
    );
    assert_eq!(
        credentials.get("missing"),
        Some(&Val::Arr(vec![string("EXAMPLE_PROVIDER_KEY")])),
        "{credentials:?}"
    );
    let secrets = unset_plan.get("secrets").expect("secrets");
    let Val::Arr(entries) = secrets else {
        panic!("secrets array");
    };
    assert_eq!(field_str(&entries[0], "digest"), "unset");
    assert!(!raw.contains(SECRET_VALUE));

    // A rotated credential changes the revision: the previous plan no longer
    // matches (safe revision semantics).
    let (set_plan, _, raw) = fixture.config_show(Some(SECRET_VALUE));
    assert_ne!(
        field_str(&set_plan, "revision"),
        field_str(&unset_plan, "revision"),
        "a credential change invalidates the reviewed revision"
    );
    assert!(
        !raw.contains(SECRET_VALUE),
        "the credential value never appears in the preview"
    );
    let set_digest = {
        let secrets = set_plan.get("secrets").expect("secrets");
        let Val::Arr(entries) = secrets else {
            panic!("secrets array");
        };
        field_str(&entries[0], "digest").to_string()
    };
    assert_ne!(set_digest, "unset");
    assert_ne!(set_digest, SECRET_VALUE);

    // The unset-revision plan still binds and verifies through the daemon
    // (credentials live in the harness, never here) — and the secret value
    // never appears in the daemon responses either.
    fixture.set_mode("binding-match");
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let (replacement_id, digest) = retired_record(
        &fixture,
        1,
        "lane-7",
        "implementer",
        Some(unset_plan.clone()),
        "credentials",
    );
    let started = start(
        &fixture.socket,
        &fresh_id(10),
        &replacement_id,
        &digest,
        "nonce-0001",
        Some(unset_plan),
    );
    let rendered = canter::canonical::canonical_text(&started);
    assert!(!rendered.contains(SECRET_VALUE));
    assert_eq!(
        field_str(start_binding(start_of(&started)), "status"),
        "matched"
    );
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC7 (restart during adoption): the interrupted adoption reconciles and the
// profile binding survives the restart.
// ---------------------------------------------------------------------------

#[test]
fn restart_during_adoption_preserves_the_profile_binding() {
    let fixture = Fixture::new("profile-restart");
    fixture.set_mode("binding-match");
    fixture.write_fake_workspace();
    fixture.write_config(PROFILE_CONFIG);
    let (plan, _, _) = fixture.config_show(Some(SECRET_VALUE));

    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let (replacement_id, digest) = retired_record(
        &fixture,
        1,
        "lane-8",
        "implementer",
        Some(plan.clone()),
        "restart",
    );
    let started = start(
        &fixture.socket,
        &fresh_id(10),
        &replacement_id,
        &digest,
        "nonce-0001",
        Some(plan.clone()),
    );
    let successor_id = field_str(
        start_of(&started).get("successor").expect("successor"),
        "successor_id",
    )
    .to_string();
    assert_eq!(
        field_str(start_binding(start_of(&started)), "status"),
        "matched"
    );
    shutdown(daemon);

    // Restart with the adoption interrupted: the crash point aborts the
    // daemon after the adoption claim, before the commit.
    let mut crashing = fixture.spawn(Some("lane-adopt.after-intent"));
    wait_ready(&fixture);
    rpc_unchecked(
        &fixture.socket,
        &fresh_id(20),
        "lane.adopt",
        Some(adopt_params(&fresh_id(20), &replacement_id, &successor_id)),
    );
    wait_crash(&mut crashing);

    // A fresh start reconciles the interrupted adoption and adopts once, with
    // the SAME reviewed binding.
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);
    let adopted = adopt(
        &fixture.socket,
        &fresh_id(21),
        &replacement_id,
        &successor_id,
    );
    assert_eq!(field_str(adoption_binding(&adopted), "status"), "matched");
    let status = status_of(&fixture.socket, 22, &replacement_id);
    assert_eq!(
        field_str(status.get("replacement").expect("replacement"), "phase"),
        "adopted"
    );
    let successor = status.get("successor").expect("successor");
    assert_ne!(
        successor.get("adopted_at"),
        Some(&Val::Null),
        "the successor is adopted exactly once after the restart"
    );
    shutdown(daemon);
}
