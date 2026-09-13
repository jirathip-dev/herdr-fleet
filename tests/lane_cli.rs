//! Issue #78 acceptance integration tests over the real binary and socket:
//! the thin CLI lane surface (`lane preview` / `lane request` / `lane
//! status`) over the completed daemon handoff path (#73–#77).
//!
//! Every test spawns `canter daemon run` (or a recording fake daemon) with
//! isolated XDG state and an explicit socket under a per-test temp dir; the
//! workspace executable the handoff drives is a fake `herdr` recorded in a
//! per-fixture invocation log, on a PATH that contains nothing else (the
//! allowlisted adapter environment is the only channel). Nothing here
//! touches the real host state, a real session, the service manager, or the
//! network; all identities are synthetic.
//!
//! Evidence rules: raw exits are asserted directly (never through grep), the
//! read-only claims are observed on the WIRE (the recording fake daemon's
//! method log) or on the durable state (journal seq), and every JSON
//! assertion parses the documented `hf-output/v1` envelope.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use canter::client::{Connection, RpcError};
use canter::value::{Val, bool_, integer, null, object, string};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_canter")
}

/// Default per-test daemon readiness deadline.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// The fake workspace executable: the ONE workspace (Herdr) adapter row the
/// retirement/start paths drive, with the modes the tests switch between.
const FAKE_WORKSPACE: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin
log="$HOME/workspace-invocations.log"
printf '%s\n' "$*" >> "$log"
mode="$(cat "$HOME/workspace-mode" 2>/dev/null || echo ready)"
case "$1 $2" in
  "session interrupt")
    printf '%s\n' '{"interrupted":true}' ;;
  "session start")
    printf '%s\n' '{"started":true}' ;;
  "session show")
    if [ "$3" = "sess-0001" ]; then
      src="$(cat "$HOME/source-mode" 2>/dev/null || echo retired)"
      case "$src" in
        live) doc='{"session_id":"sess-0001","state":"working","process":"proc-0001","registration":{"state":"active","session":"sess-0001","generation":1}}' ;;
        *) doc='{"session_id":"sess-0001","state":"retired","process":null,"registration":{"state":"released","session":"sess-0001","generation":1}}' ;;
      esac
    else
      doc='{"session_id":"sess-0002","state":"working","process":"proc-0002","role":"implementer","profile":{"key":"lane-orch-1","kind":"pi"},"cwd":"worktrees/issues/78","kickoff_receipt":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","readiness":"ready"}'
    fi
    printf '%s\n' "$doc" ;;
  *) echo '{"error":"unknown row"}' >&2; exit 4 ;;
esac
"#;

// ---------------------------------------------------------------------------
// Fixture: isolated state/config/workspace + the CLI runner
// ---------------------------------------------------------------------------

struct Fixture {
    dir: PathBuf,
    state_dir: PathBuf,
    socket: PathBuf,
    config_path: Option<PathBuf>,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("hf-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        Fixture {
            state_dir: dir.join("state"),
            socket: dir.join("daemon.sock"),
            dir,
            config_path: None,
        }
    }

    fn bin_dir(&self) -> PathBuf {
        self.dir.join("bin")
    }

    /// Write the isolated config (and optionally a policy overlay), naming
    /// the explicit socket so the CLI never needs the runtime dir.
    fn write_config(&mut self, harness: &str, provider: &str, model: &str, overlay: Option<&str>) {
        let harness_key = harness;
        let provider = provider.to_string();
        let model = model.to_string();
        let mut body = format!(
            "schema = \"hf-config/v1\"\n\
             \n\
             [daemon]\n\
             enabled = true\n\
             socket = \"{socket}\"\n\
             \n\
             [repository.widgets]\n\
             origin = \"https://example.invalid/example-org/widgets\"\n\
             \n\
             [harness.{harness_key}]\n\
             kind = \"pi\"\n\
             executable = \"herdr\"\n\
             env_allow = []\n\
             provider = \"{provider}\"\n\
             model = \"{model}\"\n\
             binding_introspection = false\n",
            socket = self.socket.display()
        );
        if let Some(rule) = overlay {
            let policy_path = self.dir.join("policy.toml");
            std::fs::write(
                &policy_path,
                format!("schema = \"hf-policy/v1\"\nproduction_confirmation = \"{rule}\"\n"),
            )
            .expect("write policy");
            body.push_str(&format!(
                "\n[policy]\noverlay = \"{}\"\n",
                policy_path.display()
            ));
        }
        let path = self.dir.join("config.toml");
        std::fs::write(&path, body).expect("write config");
        self.config_path = Some(path);
    }

    /// Rewrite the config's provider (the moved-profile-revision scenario).
    fn move_provider(&mut self, provider: &str) {
        let path = self.config_path.clone().expect("config written");
        let text = std::fs::read_to_string(&path).expect("read config");
        let updated = text
            .lines()
            .map(|line| {
                if line.starts_with("provider = ") {
                    format!("provider = \"{provider}\"")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, updated).expect("rewrite config");
    }

    fn set_source_mode(&self, mode: &str) {
        std::fs::write(self.dir.join("source-mode"), format!("{mode}\n")).expect("source mode");
    }

    fn write_fake_workspace(&self) {
        let bin_dir = self.bin_dir();
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let path = bin_dir.join("herdr");
        std::fs::write(&path, FAKE_WORKSPACE).expect("write fake workspace");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
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

    /// One CLI invocation's `Command`, with the fixture's isolated state and
    /// the workspace adapter on PATH. `piped_stdin` mirrors the harness shape
    /// the call sites need (a TTY is never involved).
    fn cli_command(&self, args: &[&str], piped_stdin: bool) -> Command {
        let mut command = Command::new(bin());
        command
            .args(args)
            .env("XDG_STATE_HOME", &self.state_dir)
            .env("XDG_CONFIG_HOME", self.dir.join("config-home"))
            .env("HOME", &self.dir)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdin(if piped_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn cli_stdin(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut child = self
            .cli_command(args, stdin.is_some())
            .spawn()
            .expect("spawn cli");
        if let Some(text) = stdin {
            write_stdin(child.stdin.as_mut().expect("stdin"), text);
        }
        child.wait_with_output().expect("cli output")
    }

    /// Run one lane subcommand with the synthetic plan flags and the
    /// fixture's socket/config.
    fn lane(
        &self,
        sub: &str,
        lane: &str,
        role: &str,
        extra: &[&str],
        stdin: Option<&str>,
    ) -> Output {
        let mut args: Vec<String> = vec!["lane".to_string(), sub.to_string()];
        if sub != "status" {
            args.extend(plan_args(lane, role));
        }
        args.extend(extra.iter().map(|s| s.to_string()));
        args.push("--socket".to_string());
        args.push(self.socket.display().to_string());
        if let Some(config) = &self.config_path {
            args.push("--config".to_string());
            args.push(config.display().to_string());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.cli_stdin(&refs, stdin)
    }
}

/// Write one CLI invocation's piped stdin. JSON mode never consumes stdin
/// (the refusal is typed, never a prompt), so the child may already have
/// exited when this write runs — the ordering rust-ubuntu CI reported as
/// `write stdin: Broken pipe`. A closed pipe is that expected ordering and is
/// never a product failure; every assertion inspects the child's `Output`
/// instead. Any other error stays loud.
fn write_stdin(stdin: &mut ChildStdin, text: &str) {
    match stdin.write_all(text.as_bytes()) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::BrokenPipe => {}
        Err(err) => panic!("write stdin: {err}"),
    }
}

/// The synthetic plan flags of one lane (public-data boundary: no host
/// paths, no real identities).
fn plan_args(lane: &str, role: &str) -> Vec<String> {
    vec![
        "--lane".to_string(),
        lane.to_string(),
        "--generation".to_string(),
        "1".to_string(),
        "--session".to_string(),
        SOURCE_SESSION.to_string(),
        "--process".to_string(),
        SOURCE_PROCESS.to_string(),
        "--role".to_string(),
        role.to_string(),
        "--worktree".to_string(),
        WORKTREE.to_string(),
        "--reason".to_string(),
        "handoff cli window".to_string(),
    ]
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

fn wait_crash(daemon: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if daemon.try_wait().expect("try_wait").is_some() {
            return;
        }
        assert!(Instant::now() < deadline, "daemon did not crash in time");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn shutdown(mut daemon: Child) {
    let _ = daemon.kill();
    wait_exit(daemon, "daemon");
}

// ---------------------------------------------------------------------------
// RPC helpers (real daemon)
// ---------------------------------------------------------------------------

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

fn fresh_id(seed: u32) -> String {
    format!("{:08x}", seed + std::process::id())
}

fn field_str<'a>(val: &'a Val, key: &str) -> &'a str {
    val.get(key).and_then(Val::as_str).expect(key)
}

// ---------------------------------------------------------------------------
// Synthetic handoff builders (copied contract shapes from tests/retirement.rs
// and tests/start_adopt.rs; all identities synthetic)
// ---------------------------------------------------------------------------

const WORKTREE: &str = "worktrees/issues/78";
const SOURCE_SESSION: &str = "sess-0001";
const SOURCE_PROCESS: &str = "proc-0001";
const SUCCESSOR_SESSION: &str = "sess-0002";
const HARNESS_KEY: &str = "lane-orch-1";
const PROVIDER: &str = "acme";
const MODEL: &str = "turbo-9000";

fn kickoff_receipt() -> String {
    "a".repeat(64)
}

fn replacement_params(lane: &str, role: &str, key: &str) -> Val {
    object(vec![
        ("idempotency_key", string(key)),
        ("lane_id", string(lane)),
        ("generation", integer(1)),
        ("source_session", string(SOURCE_SESSION)),
        ("source_process", string(SOURCE_PROCESS)),
        ("role", string(role)),
        ("worktree", string(WORKTREE)),
        ("reason", string("handoff cli window")),
    ])
}

/// A valid synthetic checkpoint observation for one lane; `orchestration`
/// adds the orchestrator reference block when supplied.
fn observation(role: &str, orchestration: Option<Val>) -> Val {
    let mut fields = vec![
        ("role", string(role)),
        ("task", string("issue-78 cli capture")),
        ("worktree", string(WORKTREE)),
        ("branch", string("issue-78-cli-handoff")),
        ("head", string(&"1".repeat(40))),
        ("base", string(&"2".repeat(40))),
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
                ("reviewed_sha", string(&"5".repeat(40))),
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
    ];
    if let Some(orchestration) = orchestration {
        fields.push(("orchestration", orchestration));
    }
    object(fields)
}

fn adoption_fields(role: &str) -> Vec<(&'static str, Val)> {
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
    fields
}

fn adoption_observation(role: &str) -> Val {
    object(adoption_fields(role))
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

fn request_replacement(socket: &Path, id_seed: u32, lane: &str, role: &str, key: &str) -> String {
    let result = rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.request",
        Some(replacement_params(lane, role, key)),
    );
    field_str(
        result.get("replacement").expect("replacement"),
        "replacement_id",
    )
    .to_string()
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
    field_str(result.get("checkpoint").expect("checkpoint"), "digest").to_string()
}

fn checkpointed_record(
    socket: &Path,
    id_seed: u32,
    lane: &str,
    role: &str,
    orchestration: Option<Val>,
    key_prefix: &str,
) -> (String, String) {
    let replacement_id = request_replacement(
        socket,
        id_seed,
        lane,
        role,
        &format!("ik_{key_prefix}-request"),
    );
    advance_to_quiescing(
        socket,
        id_seed + 1,
        &replacement_id,
        &format!("ik_{key_prefix}-advance"),
    );
    let digest = capture_checkpoint(
        socket,
        id_seed + 2,
        &replacement_id,
        role,
        orchestration,
        &format!("ik_{key_prefix}-capture"),
    );
    (replacement_id, digest)
}

fn retirement_binding(generation: i64, session: &str, process: &str, digest: &str) -> Val {
    object(vec![
        ("generation", integer(generation)),
        ("session", string(session)),
        ("process", string(process)),
        ("checkpoint_digest", string(digest)),
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
    object(vec![("key", string(HARNESS_KEY)), ("kind", string("pi"))])
}

fn retire(socket: &Path, id: &str, replacement_id: &str, digest: &str) -> Val {
    let binding = retirement_binding(1, SOURCE_SESSION, SOURCE_PROCESS, digest);
    let recheck = retirement_recheck(SOURCE_SESSION, Some(SOURCE_PROCESS), &[], false);
    rpc_ok(
        socket,
        id,
        "lane.retire",
        Some(object(vec![
            ("idempotency_key", string(&format!("ik_retire-{id}"))),
            ("replacement_id", string(replacement_id)),
            ("binding", binding),
            ("recheck", recheck),
            ("harness", harness_pi()),
        ])),
    )
}

fn live_admission() -> Val {
    object(vec![
        ("repository", string("example-org/widgets")),
        (
            "caps",
            object(vec![
                ("global", integer(8)),
                ("repository", integer(4)),
                ("harness", integer(4)),
            ]),
        ),
        (
            "host_proof",
            object(vec![("measured_at", string(&canter::time::rfc3339_now()))]),
        ),
        ("running", Val::Arr(vec![])),
    ])
}

/// Start ONE successor on a retired record; `profile` is the exact bound
/// plan document the record was requested under (when any).
fn start_successor(
    socket: &Path,
    id: &str,
    replacement_id: &str,
    digest: &str,
    profile: Option<Val>,
) -> Val {
    let mut params = vec![
        ("idempotency_key", string(&format!("ik_start-{id}"))),
        ("replacement_id", string(replacement_id)),
        (
            "binding",
            object(vec![
                ("generation", integer(1)),
                ("checkpoint_digest", string(digest)),
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
    ];
    if let Some(profile) = profile {
        params.push(("profile", profile));
    }
    rpc_ok(socket, id, "lane.start", Some(object(params)))
}

fn adopt_successor(
    socket: &Path,
    id: &str,
    replacement_id: &str,
    successor_id: &str,
    role: &str,
) -> Val {
    let observation = adoption_observation(role);
    rpc_ok(
        socket,
        id,
        "lane.adopt",
        Some(object(vec![
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
        ])),
    )
}

/// The bound `hf-profile-binding/v1` plan document out of a status result.
fn profile_doc_of(socket: &Path, id_seed: u32, replacement_id: &str) -> Val {
    let status = rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.status",
        Some(object(vec![("replacement_id", string(replacement_id))])),
    );
    let profile = status.get("profile").expect("profile row");
    profile.get("profile").expect("profile doc").clone()
}

fn status_of(socket: &Path, id_seed: u32, replacement_id: &str) -> Val {
    rpc_ok(
        socket,
        &fresh_id(id_seed),
        "lane.replacement.status",
        Some(object(vec![("replacement_id", string(replacement_id))])),
    )
}

fn journal_seq(socket: &Path, id_seed: u32) -> i64 {
    let status = rpc_ok(socket, &fresh_id(id_seed), "status", None);
    status
        .get("state")
        .and_then(|state| state.get("journal_seq"))
        .and_then(Val::as_int)
        .expect("journal_seq")
}

// ---------------------------------------------------------------------------
// CLI output helpers (JSON envelope parsing; raw exits)
// ---------------------------------------------------------------------------

fn exit_of(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf8 stdout")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf8 stderr")
}

/// The one `hf-output/v1` document `--json` mode must print on stdout: one
/// JSON document and nothing else (the trailing newline convention of the
/// existing commands is preserved).
fn stdout_json(output: &Output) -> Val {
    let text = stdout_text(output);
    let trimmed = text.trim_end();
    assert_eq!(
        trimmed.lines().count(),
        1,
        "--json stdout must be exactly one JSON document: {text:?}"
    );
    let doc = Val::parse_json(trimmed).expect("json envelope");
    assert_eq!(
        doc.get("schema").and_then(Val::as_str),
        Some("hf-output/v1"),
        "the envelope schema"
    );
    assert_eq!(
        doc.get("exit_code").and_then(Val::as_int),
        Some(exit_of(output) as i64),
        "the envelope exit_code agrees with the process exit"
    );
    doc
}

fn error_code(doc: &Val) -> String {
    doc.get("error")
        .and_then(|error| error.get("code"))
        .and_then(Val::as_str)
        .unwrap_or("")
        .to_string()
}

fn data(doc: &Val) -> &Val {
    doc.get("data").expect("data")
}

// ---------------------------------------------------------------------------
// Recording fake daemon: the read-only wire evidence
// ---------------------------------------------------------------------------

struct FakeDaemon {
    socket: PathBuf,
    methods: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<AtomicBool>,
}

impl FakeDaemon {
    /// Serve canned read-only responses and record every requested method.
    /// `record`: the synthetic `lane.replacement.status` result (None →
    /// `state.not_found`).
    fn start(name: &str, record: Option<Val>) -> FakeDaemon {
        let dir = std::env::temp_dir().join(format!("hf-cli-fake-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fake dir");
        let socket = dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind fake socket");
        listener.set_nonblocking(true).expect("nonblocking");
        let methods = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_methods = Arc::clone(&methods);
        let thread_shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve_one(stream, &thread_methods, &record),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("fake daemon accept: {err}"),
                }
            }
        });
        FakeDaemon {
            socket,
            methods,
            shutdown,
        }
    }

    fn recorded(&self) -> Vec<String> {
        self.methods.lock().expect("lock").clone()
    }
}

fn serve_one(stream: UnixStream, methods: &Arc<Mutex<Vec<String>>>, record: &Option<Val>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut line = String::new();
    if reader.read_line(&mut line).expect("read request") == 0 {
        return;
    }
    let request = Val::parse_json(line.trim_end()).expect("request json");
    let method = request
        .get("method")
        .and_then(Val::as_str)
        .unwrap_or("")
        .to_string();
    let id = request
        .get("id")
        .and_then(Val::as_str)
        .unwrap_or("00000000")
        .to_string();
    methods.lock().expect("lock").push(method.clone());
    let response = match method.as_str() {
        "lane.replacement.status" => match record {
            Some(record) => rpc_response_line(&id, Some(record.clone()), None),
            None => rpc_response_line(&id, None, Some(("state.not_found", "no lane replacement"))),
        },
        "lane.checkpoint.status" => rpc_response_line(
            &id,
            None,
            Some(("state.not_found", "no committed checkpoint")),
        ),
        other => rpc_response_line(
            &id,
            None,
            Some(("refusal.malformed", &format!("unexpected method {other}"))),
        ),
    };
    let mut writer = stream;
    writer
        .write_all(response.as_bytes())
        .expect("write response");
    writer.flush().expect("flush");
    // Keep the connection open until the client closes: a read-back client
    // must see the full line before EOF.
    let mut sink = String::new();
    let _ = reader.read_line(&mut sink);
}

fn rpc_response_line(id: &str, result: Option<Val>, error: Option<(&str, &str)>) -> String {
    let doc = match error {
        Some((code, message)) => object(vec![
            ("schema", string("hf-rpc-response/v1")),
            ("id", string(id)),
            ("ok", bool_(false)),
            ("result", null()),
            (
                "error",
                object(vec![
                    ("schema", string("hf-error/v1")),
                    ("code", string(code)),
                    ("message", string(message)),
                    ("retryable", bool_(false)),
                    ("details", null()),
                ]),
            ),
        ]),
        None => object(vec![
            ("schema", string("hf-rpc-response/v1")),
            ("id", string(id)),
            ("ok", bool_(true)),
            ("result", result.unwrap_or_else(object_empty)),
            ("error", null()),
        ]),
    };
    let verdict = canter::schema::validate_doc(canter::schema::Family::RpcResponse, &doc);
    assert!(
        verdict.is_accepted(),
        "the fake response must validate: {}",
        verdict.message()
    );
    canter::canonical::canonical_text(&doc) + "\n"
}

fn object_empty() -> Val {
    object(vec![])
}

// ---------------------------------------------------------------------------
// AC2 + AC7: preview/status are read-only on the wire, and previews render
// the source identity, target profile, worktree, boundaries and retained refs
// ---------------------------------------------------------------------------

/// The closed set of methods the read-only lane surface may ever issue.
const READ_ONLY_METHODS: [&str; 2] = ["lane.replacement.status", "lane.checkpoint.status"];

#[test]
fn preview_and_status_send_only_read_only_rpcs() {
    let fake = FakeDaemon::start("readonly", None);
    let mut fixture = Fixture::new("readonly");
    fixture.socket = fake.socket.clone();
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, None);

    // A preview of a not-yet-requested lane: read-only, exit 0.
    let preview = fixture.lane("preview", "lane-cli-1", "implementer", &["--json"], None);
    assert_eq!(exit_of(&preview), 0, "{}", stderr_text(&preview));
    let doc = stdout_json(&preview);
    assert_eq!(
        data(&doc).get("record"),
        Some(&Val::Null),
        "no record exists yet"
    );
    assert_eq!(
        data(&doc)
            .get("boundaries")
            .and_then(|boundaries| boundaries.get("mutates"))
            .and_then(Val::as_bool),
        Some(false)
    );

    // A status read of a missing record: a typed refusal, still read-only.
    let status = fixture.lane(
        "status",
        "lane-cli-1",
        "implementer",
        &["--lane", "lane-cli-1", "--generation", "1", "--json"],
        None,
    );
    assert_eq!(exit_of(&status), 4, "{}", stderr_text(&status));
    assert_eq!(error_code(&stdout_json(&status)), "state.not_found");

    let recorded = fake.recorded();
    assert!(!recorded.is_empty(), "the read-only reads must be recorded");
    for method in &recorded {
        assert!(
            READ_ONLY_METHODS.contains(&method.as_str()),
            "preview/status must never issue {method:?}; recorded: {recorded:?}"
        );
    }
    assert!(
        !recorded.iter().any(|method| method.contains("request")
            || method.contains("advance")
            || method.contains("hold")
            || method.contains("cancel")
            || method.contains("retire")
            || method.contains("start")
            || method.contains("adopt")
            || method.contains("consume")
            || method.contains("create")),
        "no mutating method may appear: {recorded:?}"
    );
    fake.shutdown.store(true, Ordering::SeqCst);
}

#[test]
fn preview_shows_source_profile_worktree_boundaries_and_retained_refs() {
    let mut fixture = Fixture::new("preview-plan");
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, None);
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // One orchestrator record with a committed checkpoint that retains a
    // worker and a reviewer: the references must name EXISTING replacement
    // records (worker/reviewer identities), as the checkpoint contract
    // requires.
    let worker = request_replacement(
        &fixture.socket,
        90,
        "lane-worker-1",
        "implementer",
        "ik_worker-1",
    );
    let reviewer = request_replacement(
        &fixture.socket,
        91,
        "lane-rev-1",
        "reviewer",
        "ik_reviewer-1",
    );
    let retained_refs = orchestration(&[&worker], &[&reviewer]);
    let (replacement_id, _digest) = checkpointed_record(
        &fixture.socket,
        1,
        "lane-cli-2",
        "orchestrator",
        Some(retained_refs),
        "cli-preview",
    );
    let seq_before = journal_seq(&fixture.socket, 5);

    let preview = fixture.lane(
        "preview",
        "lane-cli-2",
        "orchestrator",
        &["--profile", HARNESS_KEY, "--json"],
        None,
    );
    assert_eq!(exit_of(&preview), 0, "{}", stderr_text(&preview));
    let doc = stdout_json(&preview);
    let plan = data(&doc).get("plan").expect("plan");
    assert_eq!(plan.get("lane").and_then(Val::as_str), Some("lane-cli-2"));
    assert_eq!(
        plan.get("replacement_id").and_then(Val::as_str),
        Some(replacement_id.as_str())
    );
    let source = plan.get("source").expect("source");
    assert_eq!(
        source.get("session").and_then(Val::as_str),
        Some(SOURCE_SESSION)
    );
    assert_eq!(
        source.get("process").and_then(Val::as_str),
        Some(SOURCE_PROCESS)
    );
    assert_eq!(
        source.get("role").and_then(Val::as_str),
        Some("orchestrator")
    );
    assert_eq!(source.get("worktree").and_then(Val::as_str), Some(WORKTREE));
    let profile = plan.get("profile").expect("profile");
    assert_eq!(profile.get("key").and_then(Val::as_str), Some(HARNESS_KEY));
    assert_eq!(
        profile.get("provider").and_then(Val::as_str),
        Some(PROVIDER)
    );
    assert_eq!(profile.get("model").and_then(Val::as_str), Some(MODEL));
    assert!(
        canter::formats::is_hex64(field_str(profile, "revision")),
        "the target profile carries its configuration revision"
    );
    // The effect boundaries: request-only through consumption, no mutation.
    let boundaries = data(&doc).get("boundaries").expect("boundaries");
    assert_eq!(
        boundaries.get("mutates").and_then(Val::as_bool),
        Some(false)
    );
    assert_eq!(
        boundaries
            .get("effects")
            .and_then(Val::as_array)
            .expect("effects")
            .len(),
        7
    );
    // The retained workers/reviewers/gates come from the committed checkpoint.
    let retained = data(&doc).get("retained").expect("retained");
    assert_eq!(retained.get("captured").and_then(Val::as_bool), Some(true));
    assert_eq!(
        retained
            .get("workers")
            .and_then(Val::as_array)
            .expect("workers")[0]
            .as_str(),
        Some(worker.as_str())
    );
    assert_eq!(
        retained
            .get("reviewers")
            .and_then(Val::as_array)
            .expect("reviewers")[0]
            .as_str(),
        Some(reviewer.as_str())
    );
    assert_eq!(
        retained
            .get("pending_gates")
            .and_then(Val::as_array)
            .expect("gates")[0]
            .as_str(),
        Some("worker-finished:lane-8")
    );
    // The digest is the 64-hex plan identity the authorization binds.
    assert!(canter::formats::is_hex64(field_str(data(&doc), "digest")));
    // Read-only: the durable record did not move.
    assert_eq!(
        journal_seq(&fixture.socket, 6),
        seq_before,
        "preview must not write anything"
    );
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC3: both authorization paths bind the SAME plan digest; --yes cannot
// bypass the policy overlay
// ---------------------------------------------------------------------------

#[test]
fn authorization_paths_bind_the_same_plan_digest() {
    let mut fixture = Fixture::new("digest-parity");
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, None);
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // The preview digest is deterministic for the same inputs.
    let preview = fixture.lane(
        "preview",
        "lane-cli-3",
        "implementer",
        &["--profile", HARNESS_KEY, "--json"],
        None,
    );
    assert_eq!(exit_of(&preview), 0, "{}", stderr_text(&preview));
    let digest = field_str(data(&stdout_json(&preview)), "digest").to_string();
    let preview_again = fixture.lane(
        "preview",
        "lane-cli-3",
        "implementer",
        &["--profile", HARNESS_KEY, "--json"],
        None,
    );
    assert_eq!(
        field_str(data(&stdout_json(&preview_again)), "digest"),
        digest,
        "same inputs, same plan digest"
    );

    // The human confirmation path: the typed digest is the preview digest.
    let human = fixture.lane(
        "request",
        "lane-cli-3",
        "implementer",
        &["--confirm", "--profile", HARNESS_KEY],
        Some(&format!("{digest}\n")),
    );
    assert_eq!(exit_of(&human), 0, "{}", stderr_text(&human));
    assert!(
        stdout_text(&human).contains(&format!("plan digest: {digest} (authorized by digest)")),
        "the human confirmation binds the same digest: {}",
        stdout_text(&human)
    );

    // The noninteractive explicit path binds the same digest for another
    // lane: the digest is a pure function of the plan inputs.
    let other_preview = fixture.lane(
        "preview",
        "lane-cli-4",
        "implementer",
        &["--profile", HARNESS_KEY, "--json"],
        None,
    );
    let other_digest = field_str(data(&stdout_json(&other_preview)), "digest").to_string();
    assert_ne!(other_digest, digest, "a different lane is a different plan");
    let explicit = fixture.lane(
        "request",
        "lane-cli-4",
        "implementer",
        &[
            "--confirm-digest",
            &other_digest,
            "--profile",
            HARNESS_KEY,
            "--json",
        ],
        None,
    );
    assert_eq!(exit_of(&explicit), 0, "{}", stderr_text(&explicit));
    assert_eq!(
        field_str(data(&stdout_json(&explicit)), "plan_digest"),
        other_digest
    );

    // A wrong digest refuses as a stale plan in BOTH paths and nothing is
    // created.
    let wrong = "0".repeat(64);
    // The noninteractive explicit path (JSON, no prompt).
    let refused = fixture.lane(
        "request",
        "lane-cli-5",
        "implementer",
        &[
            "--confirm-digest",
            &wrong,
            "--json",
            "--profile",
            HARNESS_KEY,
        ],
        None,
    );
    assert_eq!(exit_of(&refused), 4, "{}", stderr_text(&refused));
    assert_eq!(error_code(&stdout_json(&refused)), "refusal.plan.stale");
    // The human confirmation path: a typed wrong digest is the same stale
    // refusal (the same code, the same exit).
    let human_refused = fixture.lane(
        "request",
        "lane-cli-5",
        "implementer",
        &["--confirm", "--profile", HARNESS_KEY],
        Some(&format!("{wrong}\n")),
    );
    assert_eq!(
        exit_of(&human_refused),
        4,
        "{}",
        stderr_text(&human_refused)
    );
    assert!(
        stderr_text(&human_refused).contains("refusal.plan.stale"),
        "the human path binds the same digest check: {}",
        stderr_text(&human_refused)
    );
    let missing = fixture.lane(
        "status",
        "lane-cli-5",
        "implementer",
        &["--lane", "lane-cli-5", "--generation", "1", "--json"],
        None,
    );
    assert_eq!(exit_of(&missing), 4, "the stale plan created nothing");
    assert_eq!(error_code(&stdout_json(&missing)), "state.not_found");
    shutdown(daemon);
}

#[test]
fn blanket_yes_cannot_bypass_the_policy_overlay() {
    let mut fixture = Fixture::new("policy-overlay");
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, Some("tty"));
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // The preview reports the policy-derived requirement.
    let preview = fixture.lane(
        "preview",
        "lane-cli-6",
        "implementer",
        &["--profile", HARNESS_KEY, "--json"],
        None,
    );
    assert_eq!(exit_of(&preview), 0, "{}", stderr_text(&preview));
    let authorization = data(&stdout_json(&preview))
        .get("authorization")
        .expect("authorization")
        .clone();
    assert_eq!(
        authorization.get("policy").and_then(Val::as_str),
        Some("tty")
    );
    assert_eq!(
        authorization.get("requirement").and_then(Val::as_str),
        Some("digest")
    );

    // A blanket --yes is refused BEFORE any daemon call.
    let yes = fixture.lane("request", "lane-cli-6", "implementer", &["--yes"], None);
    assert_eq!(exit_of(&yes), 4, "{}", stderr_text(&yes));
    assert!(
        stderr_text(&yes).contains("refusal.confirmation.policy"),
        "the typed refusal code is carried: {}",
        stderr_text(&yes)
    );
    let missing = fixture.lane(
        "status",
        "lane-cli-6",
        "implementer",
        &["--lane", "lane-cli-6", "--generation", "1", "--json"],
        None,
    );
    assert_eq!(exit_of(&missing), 4);
    assert_eq!(
        error_code(&stdout_json(&missing)),
        "state.not_found",
        "the refused blanket authorization created nothing"
    );

    // The explicit digest still authorizes under the same policy.
    let authorized = fixture.lane(
        "request",
        "lane-cli-6",
        "implementer",
        &["--profile", HARNESS_KEY, "--json"],
        None,
    );
    assert_eq!(
        exit_of(&authorized),
        2,
        "no authorization mode is a usage error"
    );
    assert_eq!(
        error_code(&stdout_json(&authorized)),
        "usage.confirmation_required"
    );
    let digest = field_str(
        data(&stdout_json(&fixture.lane(
            "preview",
            "lane-cli-6",
            "implementer",
            &["--profile", HARNESS_KEY, "--json"],
            None,
        ))),
        "digest",
    )
    .to_string();
    let ok = fixture.lane(
        "request",
        "lane-cli-6",
        "implementer",
        &["--confirm-digest", &digest, "--profile", HARNESS_KEY],
        None,
    );
    assert_eq!(exit_of(&ok), 0, "{}", stderr_text(&ok));
    shutdown(daemon);

    // A `deny` overlay blocks every mode.
    let mut deny_fixture = Fixture::new("policy-deny");
    deny_fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, Some("deny"));
    deny_fixture.write_fake_workspace();
    let deny_daemon = deny_fixture.spawn(None);
    wait_ready(&deny_fixture);
    let preview = deny_fixture.lane("preview", "lane-cli-7", "implementer", &["--json"], None);
    assert_eq!(exit_of(&preview), 0, "{}", stderr_text(&preview));
    let authorization = data(&stdout_json(&preview))
        .get("authorization")
        .expect("authorization")
        .clone();
    assert_eq!(
        authorization.get("requirement").and_then(Val::as_str),
        Some("denied")
    );
    let any_digest = "a".repeat(64);
    for mode in [vec!["--yes"], vec!["--confirm-digest", any_digest.as_str()]] {
        let refused = deny_fixture.lane("request", "lane-cli-7", "implementer", &mode, None);
        assert_eq!(exit_of(&refused), 4, "{}", stderr_text(&refused));
        assert!(
            stderr_text(&refused).contains("refusal.policy.production"),
            "the deny policy refuses every mode: {}",
            stderr_text(&refused)
        );
    }
    let missing = deny_fixture.lane(
        "status",
        "lane-cli-7",
        "implementer",
        &["--lane", "lane-cli-7", "--generation", "1", "--json"],
        None,
    );
    assert_eq!(exit_of(&missing), 4);
    assert_eq!(
        error_code(&stdout_json(&missing)),
        "state.not_found",
        "the denied request created nothing"
    );
    shutdown(deny_daemon);
}

#[test]
fn moved_profile_revision_rejects_the_stale_plan_digest() {
    let mut fixture = Fixture::new("stale-plan");
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, None);
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    let preview = fixture.lane(
        "preview",
        "lane-cli-8",
        "implementer",
        &["--profile", HARNESS_KEY, "--json"],
        None,
    );
    assert_eq!(exit_of(&preview), 0, "{}", stderr_text(&preview));
    let stale_digest = field_str(data(&stdout_json(&preview)), "digest").to_string();

    // The reviewed configuration moves (a new profile revision).
    fixture.move_provider("other");
    let refused = fixture.lane(
        "request",
        "lane-cli-8",
        "implementer",
        &[
            "--confirm-digest",
            &stale_digest,
            "--profile",
            HARNESS_KEY,
            "--json",
        ],
        None,
    );
    assert_eq!(exit_of(&refused), 4, "{}", stderr_text(&refused));
    assert_eq!(error_code(&stdout_json(&refused)), "refusal.plan.stale");
    let missing = fixture.lane(
        "status",
        "lane-cli-8",
        "implementer",
        &["--lane", "lane-cli-8", "--generation", "1", "--json"],
        None,
    );
    assert_eq!(exit_of(&missing), 4, "the stale plan created nothing");
    assert_eq!(error_code(&stdout_json(&missing)), "state.not_found");

    // The fresh preview digest authorizes the same request.
    let fresh = fixture.lane(
        "preview",
        "lane-cli-8",
        "implementer",
        &["--profile", HARNESS_KEY, "--json"],
        None,
    );
    let fresh_digest = field_str(data(&stdout_json(&fresh)), "digest").to_string();
    assert_ne!(fresh_digest, stale_digest, "a moved revision is a new plan");
    let ok = fixture.lane(
        "request",
        "lane-cli-8",
        "implementer",
        &["--confirm-digest", &fresh_digest, "--profile", HARNESS_KEY],
        None,
    );
    assert_eq!(exit_of(&ok), 0, "{}", stderr_text(&ok));
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC4: JSON purity, human/JSON agreement, stable codes
// ---------------------------------------------------------------------------

#[test]
fn json_mode_never_prompts_and_human_and_json_agree() {
    let mut fixture = Fixture::new("json-purity");
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, None);
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // No authorization in JSON mode: a typed usage refusal, no prompt, no
    // stdin read (the piped stdin is never consumed).
    let started = Instant::now();
    let bare = fixture.lane(
        "request",
        "lane-cli-9",
        "implementer",
        &["--json"],
        Some("not-a-digest\n"),
    );
    assert_eq!(exit_of(&bare), 2, "{}", stderr_text(&bare));
    assert_eq!(
        error_code(&stdout_json(&bare)),
        "usage.confirmation_required"
    );
    assert!(
        !stderr_text(&bare).contains("type the plan digest"),
        "JSON mode never prompts: {}",
        stderr_text(&bare)
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "no wait on stdin"
    );

    // `--confirm` is refused in JSON mode (it would need the terminal).
    let confirm = fixture.lane(
        "request",
        "lane-cli-9",
        "implementer",
        &["--json", "--confirm"],
        Some("x\n"),
    );
    assert_eq!(exit_of(&confirm), 2, "{}", stderr_text(&confirm));
    assert_eq!(
        error_code(&stdout_json(&confirm)),
        "usage.confirmation_required"
    );

    // The human preview and the JSON preview agree on the plan identity.
    let human = fixture.lane(
        "preview",
        "lane-cli-9",
        "implementer",
        &["--profile", HARNESS_KEY],
        None,
    );
    assert_eq!(exit_of(&human), 0, "{}", stderr_text(&human));
    let json = fixture.lane(
        "preview",
        "lane-cli-9",
        "implementer",
        &["--profile", HARNESS_KEY, "--json"],
        None,
    );
    let doc = stdout_json(&json);
    let digest = field_str(data(&doc), "digest");
    let human_text = stdout_text(&human);
    assert!(
        human_text.contains(digest),
        "the human rendering carries the same digest"
    );
    assert!(
        human_text.contains(SOURCE_SESSION) && human_text.contains(WORKTREE),
        "the human rendering carries the same source identity and worktree"
    );
    assert!(
        human_text.contains(PROVIDER) && human_text.contains(MODEL),
        "the human rendering carries the intended pair"
    );
    shutdown(daemon);
}

/// Deterministic bite for the stdin-ordering repair in [`write_stdin`]: the
/// unauthorized JSON request exits without ever consuming stdin, so a harness
/// write can land after the child is gone (rust-ubuntu CI reported exactly
/// that as `write stdin: Broken pipe`). Waiting for the refusal's exit first
/// makes that ordering certain instead of racy; the write stays a no-op and
/// the typed refusal is unchanged.
#[test]
fn a_stdin_write_after_the_json_refusal_exit_is_not_a_harness_failure() {
    let mut fixture = Fixture::new("json-stdin-ordering");
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, None);
    fixture.write_fake_workspace();
    // No daemon: an unauthorized JSON request is refused before any socket
    // use, so the refusal itself is what this ordering test pins.
    let mut args: Vec<String> = vec!["lane".to_string(), "request".to_string()];
    args.extend(plan_args("lane-cli-9", "implementer"));
    args.push("--json".to_string());
    args.push("--socket".to_string());
    args.push(fixture.socket.display().to_string());
    if let Some(config) = &fixture.config_path {
        args.push("--config".to_string());
        args.push(config.display().to_string());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut child = fixture.cli_command(&refs, true).spawn().expect("spawn cli");
    // Hold the write end first: `wait` closes a still-owned stdin. The child
    // has certainly exited by the time the write below runs.
    let mut stdin = child.stdin.take().expect("stdin");
    let status = child.wait().expect("wait cli");
    assert_eq!(status.code(), Some(2), "the JSON refusal is typed");
    write_stdin(&mut stdin, "not-a-digest\n");
    let output = child.wait_with_output().expect("cli output");
    assert_eq!(exit_of(&output), 2, "{}", stderr_text(&output));
    assert_eq!(
        error_code(&stdout_json(&output)),
        "usage.confirmation_required"
    );
    assert!(
        !stderr_text(&output).contains("type the plan digest"),
        "JSON mode never prompts: {}",
        stderr_text(&output)
    );
}

// ---------------------------------------------------------------------------
// AC4 + AC6: status reports phase/intended/actual/blocker/next and guidance
// ---------------------------------------------------------------------------

#[test]
fn status_reports_phase_bindings_next_action_and_guidance() {
    let mut fixture = Fixture::new("status-view");
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, None);
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // A real record: requested through the CLI (bound profile), advanced,
    // checkpointed and retired through the daemon RPCs.
    let preview = fixture.lane(
        "preview",
        "lane-cli-10",
        "implementer",
        &["--profile", HARNESS_KEY, "--json"],
        None,
    );
    let digest = field_str(data(&stdout_json(&preview)), "digest").to_string();
    let requested = fixture.lane(
        "request",
        "lane-cli-10",
        "implementer",
        &[
            "--confirm-digest",
            &digest,
            "--profile",
            HARNESS_KEY,
            "--json",
        ],
        None,
    );
    assert_eq!(exit_of(&requested), 0, "{}", stderr_text(&requested));
    let replacement_id = field_str(
        data(&stdout_json(&requested))
            .get("replacement")
            .expect("replacement"),
        "replacement_id",
    )
    .to_string();
    advance_to_quiescing(&fixture.socket, 20, &replacement_id, "ik_st-advance");
    let checkpoint_digest = capture_checkpoint(
        &fixture.socket,
        21,
        &replacement_id,
        "implementer",
        None,
        "ik_st-capture",
    );
    retire(
        &fixture.socket,
        &fresh_id(22),
        &replacement_id,
        &checkpoint_digest,
    );

    // `retired`: the capacity-hold guidance for the admission-gated start.
    let retired = fixture.lane(
        "status",
        "lane-cli-10",
        "implementer",
        &["--replacement", &replacement_id, "--json"],
        None,
    );
    assert_eq!(exit_of(&retired), 0, "{}", stderr_text(&retired));
    let doc = stdout_json(&retired);
    assert_eq!(field_str(data(&doc), "phase"), "retired");
    assert_eq!(field_str(data(&doc), "outcome"), "pending");
    assert_eq!(data(&doc).get("blocker"), Some(&Val::Null));
    let intended = data(&doc).get("intended").expect("intended");
    assert_eq!(field_str(intended, "provider"), PROVIDER);
    assert_eq!(field_str(intended, "model"), MODEL);
    assert!(canter::formats::is_hex64(field_str(intended, "revision")));
    let next = data(&doc).get("next").expect("next");
    assert_eq!(field_str(next, "phase"), "starting");
    assert_eq!(field_str(next, "operation"), "lane.start");
    assert_eq!(
        next.get("exposed_by_cli").and_then(Val::as_bool),
        Some(false)
    );
    let last = data(&doc).get("last_transition").expect("transition");
    assert_eq!(field_str(last, "to"), "retired");
    let guidance = data(&doc)
        .get("guidance")
        .and_then(Val::as_array)
        .expect("guidance");
    assert!(
        guidance.iter().any(|line| line
            .as_str()
            .unwrap_or_default()
            .contains("refusal.admission.cap_global")),
        "the capacity-hold guidance names the admission refusals"
    );

    // The human rendering agrees on every status field (AC4).
    let human = fixture.lane(
        "status",
        "lane-cli-10",
        "implementer",
        &["--replacement", &replacement_id],
        None,
    );
    assert_eq!(exit_of(&human), 0, "{}", stderr_text(&human));
    let human_text = stdout_text(&human);
    for fragment in [
        "phase: retired",
        "outcome: pending",
        "blocker: none",
        &format!("intended: {PROVIDER}/{MODEL}"),
        "last transition:",
        "next: starting",
        "lane.start",
        "refusal.admission.cap_global",
    ] {
        assert!(
            human_text.contains(fragment),
            "the human rendering must carry {fragment:?}: {human_text}"
        );
    }

    // Start the successor under the bound plan: the read-back declares no
    // provider/model and the profile declares no introspection, so the
    // actual binding is recorded `unknown` — never a copy of the intended.
    let profile_doc = profile_doc_of(&fixture.socket, 23, &replacement_id);
    let started = start_successor(
        &fixture.socket,
        &fresh_id(24),
        &replacement_id,
        &checkpoint_digest,
        Some(profile_doc),
    );
    let successor_id = field_str(
        started
            .get("start")
            .expect("start")
            .get("successor")
            .expect("successor"),
        "successor_id",
    )
    .to_string();
    let adopting = fixture.lane(
        "status",
        "lane-cli-10",
        "implementer",
        &["--replacement", &replacement_id, "--json"],
        None,
    );
    assert_eq!(exit_of(&adopting), 0, "{}", stderr_text(&adopting));
    let doc = stdout_json(&adopting);
    assert_eq!(field_str(data(&doc), "phase"), "adopting");
    let actual = data(&doc).get("actual").expect("actual");
    assert_eq!(field_str(actual, "status"), "unknown");
    assert_eq!(
        actual.get("provider"),
        Some(&Val::Null),
        "the actual binding is never copied from the intended pair"
    );
    assert_eq!(actual.get("model"), Some(&Val::Null));
    let guidance = data(&doc)
        .get("guidance")
        .and_then(Val::as_array)
        .expect("guidance");
    assert!(
        guidance
            .iter()
            .any(|line| line.as_str().unwrap_or_default().contains("UNKNOWN")),
        "the unknown-binding guidance is present"
    );
    // The human rendering agrees with the JSON nulls: the unreported pair is
    // never rendered as a value.
    let human_adopting = fixture.lane(
        "status",
        "lane-cli-10",
        "implementer",
        &["--replacement", &replacement_id],
        None,
    );
    assert_eq!(
        exit_of(&human_adopting),
        0,
        "{}",
        stderr_text(&human_adopting)
    );
    assert!(
        stdout_text(&human_adopting).contains("actual: unknown (not reported/not reported)"),
        "the unreported actual pair is explicit, never a value: {}",
        stdout_text(&human_adopting)
    );

    // The adoption completes at the same boundary (nothing here is the CLI's
    // doing — the CLI only reads).
    let adopted = adopt_successor(
        &fixture.socket,
        &fresh_id(25),
        &replacement_id,
        &successor_id,
        "implementer",
    );
    assert_eq!(
        field_str(
            adopted
                .get("adoption")
                .expect("adoption")
                .get("replacement")
                .expect("replacement"),
            "phase"
        ),
        "adopted"
    );
    let done = fixture.lane(
        "status",
        "lane-cli-10",
        "implementer",
        &["--replacement", &replacement_id],
        None,
    );
    assert_eq!(exit_of(&done), 0, "{}", stderr_text(&done));
    assert!(stdout_text(&done).contains("phase: adopted"));
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC6: ambiguous retirement and failed adoption give guided actions
// ---------------------------------------------------------------------------

#[test]
fn ambiguous_retirement_is_reported_with_reconciliation_guidance() {
    let mut fixture = Fixture::new("ambiguous-retire");
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, None);
    fixture.write_fake_workspace();
    fixture.set_source_mode("live");

    // Crash immediately after the stop: the restart must reconcile the
    // absence, and a still-present session parks the record ambiguous.
    let mut daemon = fixture.spawn(Some("lane-retire.after-stop"));
    wait_ready(&fixture);
    let (replacement_id, digest) = checkpointed_record(
        &fixture.socket,
        30,
        "lane-cli-11",
        "implementer",
        None,
        "cli-amb",
    );
    let retire_params = object(vec![
        (
            "idempotency_key",
            string(&format!("ik_amb-{}", fresh_id(31))),
        ),
        ("replacement_id", string(&replacement_id)),
        (
            "binding",
            retirement_binding(1, SOURCE_SESSION, SOURCE_PROCESS, &digest),
        ),
        (
            "recheck",
            retirement_recheck(SOURCE_SESSION, Some(SOURCE_PROCESS), &[], false),
        ),
        ("harness", harness_pi()),
    ]);
    let mut connection = Connection::open(&fixture.socket).expect("connect");
    connection
        .send_request(&fresh_id(31), "lane.retire", Some(&retire_params))
        .expect("send retire");
    // The daemon aborts at the crash point: no response arrives.
    wait_crash(&mut daemon);

    // Restart with the session still live: reconciliation parks it ambiguous.
    let restarted = fixture.spawn(None);
    wait_ready(&fixture);
    let status = status_of(&fixture.socket, 32, &replacement_id);
    assert_eq!(
        field_str(status.get("replacement").expect("replacement"), "outcome"),
        "ambiguous",
        "a still-present source parks the interrupted retirement ambiguous"
    );

    let cli = fixture.lane(
        "status",
        "lane-cli-11",
        "implementer",
        &["--replacement", &replacement_id, "--json"],
        None,
    );
    assert_eq!(exit_of(&cli), 0, "{}", stderr_text(&cli));
    let doc = stdout_json(&cli);
    assert_eq!(field_str(data(&doc), "outcome"), "ambiguous");
    assert!(
        field_str(data(&doc), "blocker").contains("ambiguous"),
        "the blocker is the recorded reason: {:?}",
        data(&doc).get("blocker")
    );
    let guidance = data(&doc)
        .get("guidance")
        .and_then(Val::as_array)
        .expect("guidance");
    assert!(
        guidance.iter().any(|line| line
            .as_str()
            .unwrap_or_default()
            .contains("external reconciliation")),
        "the ambiguous retirement names its required reconciliation: {guidance:?}"
    );
    assert!(
        guidance.iter().all(|line| {
            let text = line.as_str().unwrap_or_default();
            text.contains("canter lane status") || text.contains("lane.")
        }),
        "guidance names only existing commands: {guidance:?}"
    );

    // A failed adoption is the SAME durable park: the status contract
    // reports it identically.
    shutdown(restarted);
}

// ---------------------------------------------------------------------------
// Fix round #78-R1: the human status rendering carries the RECORDED blocker
// exactly as the JSON document does (held / ambiguous / cancelled records)
// ---------------------------------------------------------------------------

fn hold_record(socket: &Path, id: &str, replacement_id: &str, reason: &str) {
    rpc_ok(
        socket,
        id,
        "lane.replacement.hold",
        Some(object(vec![
            ("idempotency_key", string(&format!("ik_hold-{id}"))),
            ("replacement_id", string(replacement_id)),
            ("reason", string(reason)),
        ])),
    );
}

fn cancel_record(socket: &Path, id: &str, replacement_id: &str, reason: &str) {
    rpc_ok(
        socket,
        id,
        "lane.replacement.cancel",
        Some(object(vec![
            ("idempotency_key", string(&format!("ik_cancel-{id}"))),
            ("replacement_id", string(replacement_id)),
            ("reason", string(reason)),
        ])),
    );
}

fn retire_request_params(id: &str, replacement_id: &str, digest: &str) -> Val {
    object(vec![
        ("idempotency_key", string(&format!("ik_retire-{id}"))),
        ("replacement_id", string(replacement_id)),
        (
            "binding",
            retirement_binding(1, SOURCE_SESSION, SOURCE_PROCESS, digest),
        ),
        (
            "recheck",
            retirement_recheck(SOURCE_SESSION, Some(SOURCE_PROCESS), &[], false),
        ),
        ("harness", harness_pi()),
    ])
}

/// Assert that the human `lane status` rendering and the JSON document agree
/// on the recorded blocker — exactly, with no `unknown` substitution.
fn assert_blocker_agreement(fixture: &Fixture, replacement_id: &str, recorded: &str) {
    let json = fixture.lane(
        "status",
        "lane-blocker-probe",
        "implementer",
        &["--replacement", replacement_id, "--json"],
        None,
    );
    assert_eq!(exit_of(&json), 0, "{}", stderr_text(&json));
    let doc = stdout_json(&json);
    let blocker = data(&doc)
        .get("blocker")
        .and_then(Val::as_str)
        .expect("a recorded blocker is carried as a string");
    assert_eq!(blocker, recorded, "the JSON carries the recorded reason");

    let human = fixture.lane(
        "status",
        "lane-blocker-probe",
        "implementer",
        &["--replacement", replacement_id],
        None,
    );
    assert_eq!(exit_of(&human), 0, "{}", stderr_text(&human));
    let rendered = stdout_text(&human);
    assert!(
        rendered.contains(&format!("blocker: {recorded}")),
        "the human rendering carries the recorded blocker exactly: {rendered}"
    );
    assert!(
        !rendered.contains("blocker: unknown"),
        "a recorded blocker is never substituted with `unknown`: {rendered}"
    );
}

#[test]
fn human_and_json_agree_on_the_recorded_blocker() {
    let mut fixture = Fixture::new("blocker-parity");
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, None);
    fixture.write_fake_workspace();
    fixture.set_source_mode("live");
    let mut daemon = fixture.spawn(Some("lane-retire.after-stop"));
    wait_ready(&fixture);

    // A HELD record with a recorded reason (outcome `held`).
    let held = request_replacement(
        &fixture.socket,
        60,
        "lane-cli-held",
        "implementer",
        "ik_held-0001",
    );
    hold_record(
        &fixture.socket,
        &fresh_id(61),
        &held,
        "operator hold: capacity review",
    );
    // A CANCELLED record with a recorded reason (outcome `cancelled`).
    let cancelled = request_replacement(
        &fixture.socket,
        62,
        "lane-cli-cancelled",
        "implementer",
        "ik_cancelled-0001",
    );
    cancel_record(
        &fixture.socket,
        &fresh_id(63),
        &cancelled,
        "invalidated: the lane was re-scoped",
    );
    // An interrupted retirement: parked `ambiguous` by the restart
    // reconciliation (the source session is still present).
    let (ambiguous, digest) = checkpointed_record(
        &fixture.socket,
        64,
        "lane-cli-amb",
        "implementer",
        None,
        "cli-blocker",
    );
    let retire_params = retire_request_params(&fresh_id(65), &ambiguous, &digest);
    let mut connection = Connection::open(&fixture.socket).expect("connect");
    connection
        .send_request(&fresh_id(65), "lane.retire", Some(&retire_params))
        .expect("send retire");
    wait_crash(&mut daemon);
    let restarted = fixture.spawn(None);
    wait_ready(&fixture);

    // Held: the human rendering carries the recorded reason verbatim.
    let held_status = status_of(&fixture.socket, 66, &held);
    assert_eq!(
        field_str(
            held_status.get("replacement").expect("replacement"),
            "outcome"
        ),
        "held"
    );
    assert_blocker_agreement(&fixture, &held, "operator hold: capacity review");

    // Cancelled: likewise.
    let cancelled_status = status_of(&fixture.socket, 67, &cancelled);
    assert_eq!(
        field_str(
            cancelled_status.get("replacement").expect("replacement"),
            "outcome"
        ),
        "cancelled"
    );
    assert_blocker_agreement(&fixture, &cancelled, "invalidated: the lane was re-scoped");

    // Ambiguous retirement: the reconcile reason is the recorded blocker.
    let ambiguous_status = status_of(&fixture.socket, 68, &ambiguous);
    let ambiguous_reason = field_str(
        ambiguous_status.get("replacement").expect("replacement"),
        "outcome_reason",
    )
    .to_string();
    assert!(
        ambiguous_reason.contains("ambiguous"),
        "the interrupted retirement is parked ambiguous: {ambiguous_reason}"
    );
    assert_blocker_agreement(&fixture, &ambiguous, &ambiguous_reason);

    // The genuinely-absent case stays explicitly honest (no blanket
    // `unknown`): a pending record renders `blocker: none`.
    let pending = request_replacement(
        &fixture.socket,
        69,
        "lane-cli-pending",
        "implementer",
        "ik_pending-0001",
    );
    let human = fixture.lane(
        "status",
        "lane-blocker-probe",
        "implementer",
        &["--replacement", &pending],
        None,
    );
    assert_eq!(exit_of(&human), 0, "{}", stderr_text(&human));
    assert!(
        stdout_text(&human).contains("blocker: none"),
        "an absent blocker stays an explicit none: {}",
        stdout_text(&human)
    );
    shutdown(restarted);
}

// ---------------------------------------------------------------------------
// AC5: while the fleet is PAUSED, status stays useful and request/advance
// never resume anything
// ---------------------------------------------------------------------------

fn schedule_doc() -> Val {
    object(vec![
        ("schema", string("hf-schedule/v1")),
        ("schedule_id", string("sd_0123456789abcdef")),
        ("repository", string("example-org/widgets")),
        (
            "issue",
            object(vec![
                ("number", integer(123)),
                ("revision", string(&"a".repeat(40))),
            ]),
        ),
        ("workflow_hash", string(&"b".repeat(64))),
        ("policy_hash", string(&"c".repeat(64))),
        ("phase", string("read")),
        ("scope", string("worktrees/issues/123")),
        ("caps", Val::Arr(vec![string("read")])),
        ("expires_at", string("2999-01-01T00:00:00Z")),
        (
            "anchor",
            string(&canter::time::rfc3339_from_unix(
                canter::time::unix_now() + 3600,
            )),
        ),
        ("every_secs", integer(300)),
    ])
}

fn schedule_enabled(socket: &Path, id_seed: u32, schedule_id: &str) -> bool {
    let listed = rpc_ok(socket, &fresh_id(id_seed), "schedules.list", None);
    let rows = listed
        .get("schedules")
        .and_then(Val::as_array)
        .expect("schedules");
    rows.iter()
        .find(|row| row.get("schedule_id").and_then(Val::as_str) == Some(schedule_id))
        .and_then(|row| row.get("enabled"))
        .and_then(Val::as_bool)
        .expect("the schedule row")
}

#[test]
fn paused_fleet_status_stays_useful_and_request_does_not_resume() {
    let mut fixture = Fixture::new("paused-fleet");
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, None);
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // The fleet is PAUSED: its automation schedule is durably disabled.
    rpc_ok(
        &fixture.socket,
        &fresh_id(40),
        "schedules.create",
        Some(object(vec![
            ("schedule", schedule_doc()),
            ("idempotency_key", string("ik_paused-create")),
        ])),
    );
    rpc_ok(
        &fixture.socket,
        &fresh_id(41),
        "schedules.pause",
        Some(object(vec![
            ("schedule_id", string("sd_0123456789abcdef")),
            ("idempotency_key", string("ik_paused-pause")),
        ])),
    );
    assert!(
        !schedule_enabled(&fixture.socket, 42, "sd_0123456789abcdef"),
        "the fleet starts paused"
    );

    // Request records durable intent WITHOUT resuming anything.
    let digest = field_str(
        data(&stdout_json(&fixture.lane(
            "preview",
            "lane-cli-12",
            "implementer",
            &["--json"],
            None,
        ))),
        "digest",
    )
    .to_string();
    let requested = fixture.lane(
        "request",
        "lane-cli-12",
        "implementer",
        &["--confirm-digest", &digest, "--json"],
        None,
    );
    assert_eq!(exit_of(&requested), 0, "{}", stderr_text(&requested));
    let replacement_id = field_str(
        data(&stdout_json(&requested))
            .get("replacement")
            .expect("replacement"),
        "replacement_id",
    )
    .to_string();

    // The schedule is STILL paused: no hidden auto-resume.
    assert!(
        !schedule_enabled(&fixture.socket, 43, "sd_0123456789abcdef"),
        "request must not resume the paused fleet"
    );
    // The record is useful and read-only while paused.
    let status = fixture.lane(
        "status",
        "lane-cli-12",
        "implementer",
        &["--replacement", &replacement_id, "--json"],
        None,
    );
    assert_eq!(exit_of(&status), 0, "{}", stderr_text(&status));
    assert_eq!(field_str(data(&stdout_json(&status)), "phase"), "requested");
    let human_status = fixture.lane(
        "status",
        "lane-cli-12",
        "implementer",
        &["--replacement", &replacement_id],
        None,
    );
    assert_eq!(exit_of(&human_status), 0, "{}", stderr_text(&human_status));
    let rendered = stdout_text(&human_status);
    for fragment in ["phase: requested", "next: quiescing"] {
        assert!(
            rendered.contains(fragment),
            "status stays useful: {rendered}"
        );
    }
    let every_output = format!(
        "{}{}{}",
        stdout_text(&requested),
        rendered,
        stderr_text(&status)
    );
    for banned in ["resume", "auto-start", "auto-advance"] {
        assert!(
            !every_output.contains(banned),
            "the CLI never advertises a resume: {rendered}"
        );
    }
    shutdown(daemon);
}

// ---------------------------------------------------------------------------
// AC7: refusal parity, bounded waiting
// ---------------------------------------------------------------------------

#[test]
fn refusals_and_successes_render_identically_in_human_and_json_modes() {
    let mut fixture = Fixture::new("parity");
    fixture.write_config(HARNESS_KEY, PROVIDER, MODEL, None);
    fixture.write_fake_workspace();
    let daemon = fixture.spawn(None);
    wait_ready(&fixture);

    // Success parity: the same request in the two renderings.
    let digest = field_str(
        data(&stdout_json(&fixture.lane(
            "preview",
            "lane-cli-13",
            "implementer",
            &["--json"],
            None,
        ))),
        "digest",
    )
    .to_string();
    let human = fixture.lane(
        "request",
        "lane-cli-13",
        "implementer",
        &["--confirm-digest", &digest],
        None,
    );
    assert_eq!(exit_of(&human), 0, "{}", stderr_text(&human));
    assert!(stdout_text(&human).contains(&digest));
    assert!(stdout_text(&human).contains("phase: requested"));

    // Refusal parity: a second request for the same lane generation is the
    // daemon's typed `refusal.replacement.exists` in both renderings.
    let digest_again = field_str(
        data(&stdout_json(&fixture.lane(
            "preview",
            "lane-cli-14",
            "implementer",
            &["--json"],
            None,
        ))),
        "digest",
    )
    .to_string();
    let first = fixture.lane(
        "request",
        "lane-cli-14",
        "implementer",
        &["--confirm-digest", &digest_again],
        None,
    );
    assert_eq!(exit_of(&first), 0, "{}", stderr_text(&first));
    let human_refusal = fixture.lane(
        "request",
        "lane-cli-14",
        "implementer",
        &["--confirm-digest", &digest_again],
        None,
    );
    assert_eq!(
        exit_of(&human_refusal),
        4,
        "{}",
        stderr_text(&human_refusal)
    );
    assert!(
        stderr_text(&human_refusal).contains("refusal.replacement.exists"),
        "the human rendering carries the typed code: {}",
        stderr_text(&human_refusal)
    );
    let json_refusal = fixture.lane(
        "request",
        "lane-cli-14",
        "implementer",
        &["--confirm-digest", &digest_again, "--json"],
        None,
    );
    assert_eq!(exit_of(&json_refusal), 4, "{}", stderr_text(&json_refusal));
    assert_eq!(
        error_code(&stdout_json(&json_refusal)),
        "refusal.replacement.exists"
    );
    shutdown(daemon);
}

#[test]
fn a_hung_daemon_is_a_bounded_typed_refusal() {
    // A socket that accepts and never answers: the CLI must give up on its
    // bounded deadline with a stable transport code, never hang forever.
    let dir = std::env::temp_dir().join(format!("hf-cli-hung-socket-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let socket = dir.join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    let mut held: Vec<UnixStream> = Vec::new();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => held.push(stream),
                Err(_) => break,
            }
        }
    });

    let mut fixture = Fixture::new("hung");
    fixture.socket = socket;
    fixture.write_fake_workspace();
    let started = Instant::now();
    let status = fixture.lane(
        "status",
        "lane-cli-15",
        "implementer",
        &["--lane", "lane-cli-15", "--generation", "1", "--json"],
        None,
    );
    let elapsed = started.elapsed();
    assert_eq!(exit_of(&status), 1, "{}", stderr_text(&status));
    assert_eq!(error_code(&stdout_json(&status)), "client.read");
    assert!(
        elapsed < Duration::from_secs(30),
        "the read must be bounded by the client deadline, took {elapsed:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
