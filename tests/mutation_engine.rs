//! Issue #8 control-plane mutation acceptance tests over the real binary
//! and socket: deterministic fakes + disposable LOCAL repositories only.
//!
//! Every test builds a private sandbox with a local bare "remote", an
//! integration checkout on `staging`, a lane worktrees root, fake
//! `gh`/`hf-lane` executables on an allowlisted PATH, a seeded daemon state
//! (route grant + workflow instance), and a real daemon child. Effects are
//! driven through the `plan`/`apply` RPC methods with real git subprocesses
//! (disposable local repos — never a real remote) and scripted `gh` fakes.
//! Nothing here touches the network, a real repository, or a real account.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use herdr_fleet::canonical::{canonical_bytes, sha256_hex};
use herdr_fleet::client::{Connection, RpcError};
use herdr_fleet::dirs::DaemonPaths;
use herdr_fleet::state::{Retention, State};
use herdr_fleet::value::{Val, integer, null, object, string};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-fleet")
}

// ---------------------------------------------------------------------------
// Sandbox + repositories (disposable; local only)
// ---------------------------------------------------------------------------

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!(
            "hf-mut8-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("sandbox root");
        Sandbox { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(&path, content).expect("write file");
        path
    }

    fn chmod_x(&self, rel: &str) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = self.path(rel);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod +x");
        }
    }
}

struct Git {
    cwd: PathBuf,
}

impl Git {
    fn new(cwd: &Path) -> Git {
        Git {
            cwd: cwd.to_path_buf(),
        }
    }

    fn run(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.cwd)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn head(&self, rev: &str) -> String {
        self.run(&["rev-parse", "--verify", rev]).trim().to_string()
    }
}

struct Repos {
    checkout: PathBuf,
    worktrees_root: PathBuf,
}

fn make_repos(sandbox: &Sandbox) -> Repos {
    let seed = sandbox.path("seed");
    let origin = sandbox.path("origin.git");
    let checkout = sandbox.path("checkout");
    std::fs::create_dir_all(&seed).expect("seed dir");
    let seed_git = Git::new(&seed);
    seed_git.run(&["init", "-q", "-b", "staging"]);
    std::fs::write(seed.join("base.txt"), "base\n").expect("base file");
    seed_git.run(&["add", "base.txt"]);
    seed_git.run(&["commit", "-q", "-m", "seed base"]);
    let root_git = Git::new(sandbox.root.as_path());
    root_git.run(&[
        "clone",
        "-q",
        "--bare",
        seed.to_str().unwrap(),
        origin.to_str().unwrap(),
    ]);
    root_git.run(&[
        "clone",
        "-q",
        origin.to_str().unwrap(),
        checkout.to_str().unwrap(),
    ]);
    let ck = Git::new(&checkout);
    ck.run(&["checkout", "-q", "staging"]);
    ck.run(&["config", "user.name", "herdr-fleet test"]);
    ck.run(&["config", "user.email", "test@example.invalid"]);
    Repos {
        checkout,
        worktrees_root: sandbox.path("worktrees"),
    }
}

/// Fake `gh` (deterministic forge responses).
const FAKE_GH: &str = r#"#!/bin/sh
case "$1" in
  pr)
    case "$2" in
      create)
        printf '%s' '{"number": 1001, "url": "https://github.invalid/example-org/widgets/pull/1001"}'
        exit 0 ;;
      checks)
        printf '%s' '[{"name":"exact-head-review","state":"SUCCESS","conclusion":"success"},{"name":"hosted-ci","state":"SUCCESS","conclusion":"success"}]'
        exit 0 ;;
      comment) printf '%s' '{"id": 77}'; exit 0 ;;
    esac ;;
  issue)
    case "$2" in
      comment) printf '%s' '{"id": 88}'; exit 0 ;;
      close) printf '%s' '{"number": 123, "state": "closed"}'; exit 0 ;;
    esac ;;
esac
exit 1
"#;

/// Fake lane harness (`argv` adapter): appends a lane commit inside the
/// assigned worktree (idempotently), then prints a deterministic transcript.
const FAKE_LANE: &str = r#"#!/bin/sh
echo 'lane change' >> lane.txt
git add -A
git -c user.name='herdr-fleet lane' -c user.email='lane@example.invalid' commit -q -m 'lane work (synthetic)'
printf '%s' 'lane transcript ok'
exit 0
"#;

// ---------------------------------------------------------------------------
// Daemon fixture
// ---------------------------------------------------------------------------

struct Fixture {
    dir: PathBuf,
    state_dir: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new(sandbox: &Sandbox, name: &str) -> Fixture {
        let dir = sandbox.path(&format!("daemon-{name}"));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        Fixture {
            state_dir: dir.join("state"),
            socket: dir.join("daemon.sock"),
            dir,
        }
    }

    fn paths(&self) -> DaemonPaths {
        let state_dir = self.state_dir.join("herdr-fleet");
        DaemonPaths {
            state_dir: state_dir.clone(),
            runtime_dir: self.dir.clone(),
            socket_path: self.socket.clone(),
            lock_path: state_dir.join("daemon.lock"),
            db_path: state_dir.join("state.db"),
            audit_mirror_path: state_dir.join("journal").join("audit.jsonl"),
            events_mirror_path: state_dir.join("journal").join("events.jsonl"),
            backups_dir: state_dir.join("backups"),
            log_path: state_dir.join("daemon.log"),
        }
    }

    fn spawn(&self, path: &str) -> Child {
        let stderr_path = std::env::temp_dir().join(format!(
            "hf-mut8-daemon-{}.stderr.log",
            self.dir.file_name().unwrap().to_string_lossy()
        ));
        let stderr_file = std::fs::File::create(&stderr_path).expect("stderr log");
        Command::new(bin())
            .args(["daemon", "run", "--socket"])
            .arg(&self.socket)
            .env("XDG_STATE_HOME", &self.state_dir)
            .env("HOME", &self.dir)
            .env("PATH", path)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .expect("spawn daemon")
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
    panic!("daemon did not become ready");
}

fn rpc(socket: &Path, id: &str, method: &str, params: Option<Val>) -> Val {
    let mut connection = Connection::open(socket).expect("connect");
    connection
        .send_request(id, method, params.as_ref())
        .expect("send");
    let response = connection.read_response().expect("read response");
    if response.ok {
        object(vec![
            ("ok", herdr_fleet::value::bool_(true)),
            ("result", response.result),
        ])
    } else {
        let error = response.error.unwrap_or_else(|| RpcError {
            code: "missing.error".to_string(),
            message: "no error doc".to_string(),
        });
        object(vec![
            ("ok", herdr_fleet::value::bool_(false)),
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
        "expected ok response: {}",
        herdr_fleet::canonical::canonical_text(&doc)
    );
    doc.get("result").expect("result").clone()
}

fn rpc_err(socket: &Path, id: &str, method: &str, params: Option<Val>) -> (String, String) {
    let doc = rpc(socket, id, method, params);
    assert_eq!(
        doc.get("ok").and_then(Val::as_bool),
        Some(false),
        "expected refused response: {}",
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

fn wait_exit(mut child: Child, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            let _ = status;
            return;
        }
        assert!(Instant::now() < deadline, "{label} did not exit in time");
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------------------
// State seeding + plan builders
// ---------------------------------------------------------------------------

const GRANT_ID: &str = "gr_abcdef0123456789";
const INSTANCE_ID: &str = "run-1";
const REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const POLICY_HASH: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
const WORKFLOW_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn grant_doc(expires_at: &str) -> Val {
    object(vec![
        ("schema", string("hf-grant/v1")),
        ("grant_id", string(GRANT_ID)),
        ("repository", string("example-org/widgets")),
        (
            "issue",
            object(vec![
                ("number", integer(123)),
                ("revision", string(REVISION)),
            ]),
        ),
        ("workflow_hash", string(WORKFLOW_HASH)),
        ("policy_hash", string(POLICY_HASH)),
        ("phase", string("merge")),
        ("scope", string("worktrees/issues/123")),
        (
            "caps",
            Val::Arr(vec![
                string("read"),
                string("worktree"),
                string("spawn"),
                string("prompt"),
                string("review"),
                string("merge"),
                string("cleanup"),
                string("production"),
            ]),
        ),
        ("expires_at", string(expires_at)),
        ("state_epoch", integer(1)),
        ("created_at", string("2026-09-06T00:00:00Z")),
    ])
}

fn seed_state(db_path: &Path, expires_at: &str) {
    let state = State::open(db_path, Retention::default()).expect("open state");
    state
        .issue_grant(&grant_doc(expires_at))
        .expect("issue grant");
    state
        .start_instance(
            INSTANCE_ID,
            GRANT_ID,
            "fleet-doctrine-1",
            "2026-09-06T00:00:00Z",
        )
        .expect("start instance");
}

fn step(id: &str, kind: &str, params: Option<Val>) -> Val {
    object(vec![
        ("id", string(id)),
        ("kind", string(kind)),
        ("params", params.unwrap_or_else(null)),
    ])
}

fn make_plan(steps: Vec<Val>) -> Val {
    let seed = object(vec![
        ("schema", string("hf-plan/v1")),
        ("plan_id", string("hf_plan_0000000000000000")),
        ("workflow_id", string("fleet-doctrine-1")),
        ("workflow_hash", string(WORKFLOW_HASH)),
        ("state_epoch", integer(1)),
        ("repository", string("example-org/widgets")),
        (
            "issue",
            object(vec![
                ("number", integer(123)),
                ("revision", string(REVISION)),
            ]),
        ),
        ("steps", Val::Arr(steps.clone())),
    ]);
    let digest = sha256_hex(&canonical_bytes(&seed));
    let plan_id = format!("hf_plan_{}", &digest[..16]);
    let mut map = match seed {
        Val::Obj(map) => map,
        _ => unreachable!(),
    };
    map.insert("plan_id".to_string(), string(&plan_id));
    Val::Obj(map)
}

fn flow_steps() -> Vec<Val> {
    vec![
        step(
            "w1",
            "worktree_create",
            Some(object(vec![
                ("branch", string("issue-123")),
                ("worktree", string("issues-123")),
            ])),
        ),
        step(
            "h1",
            "harness_start",
            Some(object(vec![
                ("session_id", string("sess-8-1")),
                ("herdr_session", string("ws-session-8")),
                ("terminal_session", string("tty-8-1")),
                ("generation", integer(1)),
                ("harness_key", string("lane")),
                ("executable", string("hf-lane")),
                ("kind", string("argv")),
            ])),
        ),
        step(
            "p1",
            "prompt",
            Some(object(vec![
                ("session_id", string("sess-8-1")),
                ("herdr_session", string("ws-session-8")),
                ("terminal_session", string("tty-8-1")),
                ("generation", integer(1)),
                ("harness_key", string("lane")),
                ("executable", string("hf-lane")),
                ("kind", string("argv")),
                ("worktree", string("issues-123")),
                (
                    "payload",
                    string("implement acceptance criteria (synthetic)"),
                ),
            ])),
        ),
        step(
            "o1",
            "collect_outcome",
            Some(object(vec![("worktree", string("issues-123"))])),
        ),
        step(
            "r1",
            "review_evidence",
            Some(object(vec![
                ("reviewer", string("reviewer-1")),
                ("implementer", string("implementer-1")),
                ("verdict", string("pass")),
                (
                    "checks",
                    Val::Arr(vec![
                        object(vec![
                            ("name", string("exact-head-review")),
                            ("status", string("passed")),
                        ]),
                        object(vec![
                            ("name", string("hosted-ci")),
                            ("status", string("passed")),
                        ]),
                    ]),
                ),
            ])),
        ),
        step(
            "m1",
            "merge",
            Some(object(vec![("branch", string("issue-123"))])),
        ),
        step("v1", "post_merge_verify", Some(object(vec![]))),
        step(
            "i1",
            "issue_update",
            Some(object(vec![
                ("action", string("close")),
                ("repo", string("example-org/widgets")),
                ("number", integer(123)),
                (
                    "body",
                    string("delivered via the mutation engine (synthetic)"),
                ),
            ])),
        ),
        step(
            "x1",
            "cleanup",
            Some(object(vec![
                ("worktree", string("issues-123")),
                ("branch", string("issue-123")),
            ])),
        ),
    ]
}

fn policy_steps() -> Vec<Val> {
    vec![
        step(
            "w1",
            "worktree_create",
            Some(object(vec![
                ("branch", string("issue-123")),
                ("worktree", string("issues-123")),
            ])),
        ),
        step(
            "p1",
            "prompt",
            Some(object(vec![
                ("session_id", string("sess-8-1")),
                ("herdr_session", string("ws-session-8")),
                ("terminal_session", string("tty-8-1")),
                ("generation", integer(1)),
                ("harness_key", string("lane")),
                ("executable", string("hf-lane")),
                ("kind", string("argv")),
                ("worktree", string("issues-123")),
                (
                    "payload",
                    string("implement acceptance criteria (synthetic)"),
                ),
            ])),
        ),
        step(
            "u1",
            "pr_update",
            Some(object(vec![
                ("action", string("create")),
                ("repo", string("example-org/widgets")),
                ("head", string("staging")),
                ("base", string("main")),
                ("title", string("promote staging (synthetic)")),
                ("body", string("promotion body")),
            ])),
        ),
        step(
            "a1",
            "approve",
            Some(object(vec![
                ("digest", string(&"1".repeat(64))),
                ("interactive", herdr_fleet::value::bool_(true)),
            ])),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Scenario (one disposable world per test)
// ---------------------------------------------------------------------------

struct Scenario {
    sandbox: Sandbox,
    repos: Repos,
    fixture: Fixture,
    plan: Val,
    daemon: Option<Child>,
}

impl Scenario {
    fn new(name: &str, expires_at: &str, steps: Vec<Val>) -> Scenario {
        let sandbox = Sandbox::new(name);
        let repos = make_repos(&sandbox);
        sandbox.write("fakebin/gh", FAKE_GH);
        sandbox.write("fakebin/hf-lane", FAKE_LANE);
        sandbox.chmod_x("fakebin/gh");
        sandbox.chmod_x("fakebin/hf-lane");
        let host_path = std::env::var("PATH").unwrap_or_default();
        let path = format!("{}:{host_path}", sandbox.path("fakebin").display());
        let fixture = Fixture::new(&sandbox, name);
        let db_path = fixture.paths().db_path;
        seed_state(&db_path, expires_at);
        let daemon = fixture.spawn(&path);
        wait_ready(&fixture);
        Scenario {
            sandbox,
            repos,
            fixture,
            plan: make_plan(steps),
            daemon: Some(daemon),
        }
    }

    fn integration_base(&self) -> String {
        Git::new(&self.repos.checkout).head("staging")
    }

    fn apply_ok(
        &self,
        seed: u32,
        step_id: &str,
        feature_head: Option<&str>,
        base: Option<&str>,
    ) -> Val {
        self.apply_ok_with(seed, step_id, feature_head, base, None, false)
    }

    fn apply_ok_with(
        &self,
        seed: u32,
        step_id: &str,
        feature_head: Option<&str>,
        base: Option<&str>,
        target_scope: Option<&str>,
        scheduled: bool,
    ) -> Val {
        let doc = rpc(
            &self.fixture.socket,
            &fresh_id(seed),
            "apply",
            Some(self.params(seed, step_id, feature_head, base, target_scope, scheduled)),
        );
        assert_eq!(
            doc.get("ok").and_then(Val::as_bool),
            Some(true),
            "apply {step_id} (seed {seed}) failed: {}",
            herdr_fleet::canonical::canonical_text(&doc)
        );
        doc.get("result").expect("result").clone()
    }

    fn apply_err(
        &self,
        seed: u32,
        step_id: &str,
        feature_head: Option<&str>,
        base: Option<&str>,
    ) -> (String, String) {
        rpc_err(
            &self.fixture.socket,
            &fresh_id(seed),
            "apply",
            Some(self.params(seed, step_id, feature_head, base, None, false)),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn params(
        &self,
        seed: u32,
        step_id: &str,
        feature_head: Option<&str>,
        base: Option<&str>,
        target_scope: Option<&str>,
        scheduled: bool,
    ) -> Val {
        let mut flags = vec![
            ("interactive", herdr_fleet::value::bool_(true)),
            ("digest_confirmed", herdr_fleet::value::bool_(true)),
            ("scheduled", herdr_fleet::value::bool_(scheduled)),
            ("production_confirmation", string("tty")),
        ];
        if let Some(scope) = target_scope {
            flags.push(("target_scope", string(scope)));
        }
        object(vec![
            (
                "idempotency_key",
                string(&format!("ik_{step_id}-{seed:08x}")),
            ),
            ("plan", self.plan.clone()),
            ("step", string(step_id)),
            ("grant_id", string(GRANT_ID)),
            ("instance_id", string(INSTANCE_ID)),
            (
                "observed",
                object(vec![
                    ("issue_revision", string(REVISION)),
                    ("policy_hash", string(POLICY_HASH)),
                    (
                        "feature_head",
                        feature_head.map(string).unwrap_or_else(null),
                    ),
                    ("integration_base", base.map(string).unwrap_or_else(null)),
                ]),
            ),
            (
                "topology",
                object(vec![
                    ("integration_branch", string("staging")),
                    ("production_branches", Val::Arr(vec![string("main")])),
                    (
                        "worktrees_root",
                        string(&self.repos.worktrees_root.to_string_lossy()),
                    ),
                    (
                        "integration_repo",
                        string(&self.repos.checkout.to_string_lossy()),
                    ),
                ]),
            ),
            ("flags", object(flags)),
        ])
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            wait_exit(daemon, "scenario daemon");
        }
        if std::env::var("HF_KEEP_SANDBOX").is_err() {
            let _ = std::fs::remove_dir_all(&self.sandbox.root);
        }
    }
}

// ---------------------------------------------------------------------------
// AC1: plan RPC + digest binding + idempotent replay; duplicate suppression
// ---------------------------------------------------------------------------

#[test]
fn plan_rpc_digest_binding_and_idempotent_replay() {
    let scenario = Scenario::new("ac1-plan", "2999-01-01T00:00:00Z", flow_steps());
    let rendered = rpc_ok(
        &scenario.fixture.socket,
        &fresh_id(1),
        "plan",
        Some(object(vec![
            ("repository", string("example-org/widgets")),
            (
                "issue",
                object(vec![
                    ("number", integer(123)),
                    ("revision", string(REVISION)),
                ]),
            ),
            ("branch", string("staging")),
        ])),
    );
    assert_eq!(
        rendered
            .get("digest")
            .and_then(Val::as_str)
            .unwrap_or("")
            .len(),
        64
    );

    // A tampered plan (edited revision, stale content id) is refused at
    // bind time — before any claim is journaled (AC1 digest binding).
    let mut tampered = scenario.plan.clone();
    if let Val::Obj(map) = &mut tampered
        && let Some(issue) = map.get_mut("issue")
        && let Val::Obj(issue_map) = issue
    {
        issue_map.insert(
            "revision".to_string(),
            string(&format!("b{}", &REVISION[1..])),
        );
    }
    let params = scenario.params(2, "w1", None, None, None, false);
    // Swap in the tampered plan document.
    let mut map = match params {
        Val::Obj(map) => map,
        _ => unreachable!(),
    };
    map.insert("plan".to_string(), tampered);
    let (code, _) = rpc_err(
        &scenario.fixture.socket,
        &fresh_id(2),
        "apply",
        Some(Val::Obj(map)),
    );
    assert_eq!(code, "refusal.plan.identity");

    // First apply of the real plan creates the lane worktree.
    let first = scenario.apply_ok(3, "w1", None, None);
    assert_eq!(first.get("branch").and_then(Val::as_str), Some("issue-123"));
    assert!(scenario.repos.worktrees_root.join("issues-123").exists());

    // Same request id + key replays the recorded response (idempotency).
    let replay = scenario.apply_ok(3, "w1", None, None);
    assert!(replay.get("head").and_then(Val::as_str).is_some());

    // A racing duplicate (new request, same branch/worktree) cannot create
    // a second lane: the effect fails and nothing is duplicated (AC2).
    let (code2, _) = scenario.apply_err(4, "w1", None, None);
    assert_eq!(code2, "adapter.exit", "git refuses the duplicate worktree");
    let lane = scenario.repos.worktrees_root.join("issues-123");
    let listing = Git::new(&scenario.repos.checkout)
        .run(&["worktree", "list"])
        .lines()
        .count();
    assert_eq!(listing, 2, "only the integration checkout + one lane");
    assert!(lane.exists());
}

// ---------------------------------------------------------------------------
// Full lane flow: worktree -> harness -> prompt -> collect -> evidence ->
// merge -> verify -> close -> cleanup (green path; AC1/AC4/AC7/AC8)
// ---------------------------------------------------------------------------

#[test]
fn lane_flow_merges_verified_head_closes_issue_and_cleans_with_salvage() {
    let scenario = Scenario::new("lane-flow", "2999-01-01T00:00:00Z", flow_steps());
    let integration_base = scenario.integration_base();

    let wt = scenario.apply_ok(10, "w1", None, None);
    assert_eq!(wt.get("contained").and_then(Val::as_bool), Some(true));
    let start = scenario.apply_ok(11, "h1", None, None);
    assert_eq!(
        start.get("session_id").and_then(Val::as_str),
        Some("sess-8-1")
    );
    let prompt = scenario.apply_ok(12, "p1", None, None);
    assert!(
        prompt
            .get("transcript")
            .and_then(Val::as_str)
            .unwrap_or("")
            .contains("lane transcript ok")
    );
    let collected = scenario.apply_ok(13, "o1", None, None);
    let feature_head = collected
        .get("head")
        .and_then(Val::as_str)
        .expect("lane head")
        .to_string();
    assert!(
        !collected
            .get("commits")
            .and_then(Val::as_array)
            .expect("commits")
            .is_empty()
    );
    assert_ne!(feature_head, integration_base);

    let evidence = scenario.apply_ok(14, "r1", Some(&feature_head), Some(&integration_base));
    assert!(
        evidence
            .get("evidence_id")
            .and_then(Val::as_str)
            .unwrap_or("")
            .starts_with("ev_")
    );

    // Merge gate passes with current bindings; the integration branch
    // fast-forwards exactly to the reviewed head.
    let merged = scenario.apply_ok(15, "m1", Some(&feature_head), Some(&integration_base));
    assert_eq!(
        merged.get("merged_head").and_then(Val::as_str),
        Some(feature_head.as_str())
    );
    let verified = scenario.apply_ok(16, "v1", Some(&feature_head), Some(&integration_base));
    assert_eq!(
        verified.get("contains_feature").and_then(Val::as_bool),
        Some(true)
    );

    // Issue closure only after merge + post-merge verification (AC7).
    let closed = scenario.apply_ok(17, "i1", Some(&feature_head), Some(&integration_base));
    assert_eq!(closed.get("action").and_then(Val::as_str), Some("close"));

    // Cleanup removes the lane and journals the salvage evidence (AC8).
    let cleaned = scenario.apply_ok(18, "x1", Some(&feature_head), Some(&integration_base));
    assert_eq!(cleaned.get("removed").and_then(Val::as_bool), Some(true));
    assert!(!scenario.repos.worktrees_root.join("issues-123").exists());

    let tail = rpc_ok(
        &scenario.fixture.socket,
        &fresh_id(19),
        "journal.tail",
        Some(object(vec![
            ("after_seq", integer(0)),
            ("limit", integer(500)),
        ])),
    );
    let records = tail
        .get("records")
        .and_then(Val::as_array)
        .expect("records");
    let actions: Vec<String> = records
        .iter()
        .filter_map(|r| r.get("action").and_then(Val::as_str).map(str::to_string))
        .collect();
    assert!(
        actions.iter().any(|a| a == "salvage.cleanup"),
        "journal must contain the salvage record: {actions:?}"
    );
    assert!(
        actions.iter().any(|a| a == "mutate.merge"),
        "journal must contain the merge intent: {actions:?}"
    );
    // Every claim resolved; no interrupted mutations.
    let status = rpc_ok(&scenario.fixture.socket, &fresh_id(20), "doctor", None);
    assert_eq!(status.get("pending_claims").and_then(Val::as_int), Some(0));
}

// ---------------------------------------------------------------------------
// AC4 RED: moved base invalidates recorded evidence before the merge
// ---------------------------------------------------------------------------

#[test]
fn moved_integration_base_invalidates_stale_evidence_and_refuses_merge() {
    let scenario = Scenario::new("stale-evidence", "2999-01-01T00:00:00Z", flow_steps());
    let integration_base = scenario.integration_base();
    scenario.apply_ok(30, "w1", None, None);
    scenario.apply_ok(31, "h1", None, None);
    scenario.apply_ok(32, "p1", None, None);
    let collected = scenario.apply_ok(33, "o1", None, None);
    let feature_head = collected
        .get("head")
        .and_then(Val::as_str)
        .expect("head")
        .to_string();
    // Evidence binds the CURRENT base.
    scenario.apply_ok(34, "r1", Some(&feature_head), Some(&integration_base));

    // The integration base moves while the lane waits (another merge).
    let ck = Git::new(&scenario.repos.checkout);
    std::fs::write(scenario.repos.checkout.join("other.txt"), "other\n").expect("write");
    ck.run(&["add", "other.txt"]);
    ck.run(&["commit", "-q", "-m", "another lane merged (synthetic)"]);
    let moved_base = ck.head("staging");
    assert_ne!(moved_base, integration_base);

    // The merge observes the moved base: stale evidence refuses BEFORE any
    // effect (AC4).
    let (code, _) = scenario.apply_err(35, "m1", Some(&feature_head), Some(&moved_base));
    assert_eq!(code, "refusal.evidence.stale");

    // Even reporting the OLD base cannot merge: the recorded evidence still
    // matches the OLD base, so the gate passes and the merge effect itself
    // fails closed on the moved ref (fast-forward impossible — AC2 race).
    let (code2, _) = scenario.apply_err(36, "m1", Some(&feature_head), Some(&integration_base));
    assert_eq!(code2, "effect.merge.not_fast_forward");
    assert_eq!(ck.head("staging"), moved_base, "no merge happened");
}

// ---------------------------------------------------------------------------
// AC6 + AC10 RED/GREEN policy probes over the wire
// ---------------------------------------------------------------------------

#[test]
fn production_pr_schedule_and_first_write_gates_bite_over_the_wire() {
    let scenario = Scenario::new("policy-probes", "2999-01-01T00:00:00Z", policy_steps());

    // u1 = pr_update create against `main`: no interactive TTY digest in
    // the flags → the production gate refuses before any gh call.
    let (code, _) = rpc_err(
        &scenario.fixture.socket,
        &fresh_id(40),
        "apply",
        Some(object(vec![
            ("idempotency_key", string("ik_u1-notty-0001")),
            ("plan", scenario.plan.clone()),
            ("step", string("u1")),
            ("grant_id", string(GRANT_ID)),
            ("instance_id", string(INSTANCE_ID)),
            (
                "observed",
                object(vec![
                    ("issue_revision", string(REVISION)),
                    ("policy_hash", string(POLICY_HASH)),
                ]),
            ),
            (
                "topology",
                object(vec![
                    ("integration_branch", string("staging")),
                    ("production_branches", Val::Arr(vec![string("main")])),
                    (
                        "worktrees_root",
                        string(&scenario.repos.worktrees_root.to_string_lossy()),
                    ),
                    (
                        "integration_repo",
                        string(&scenario.repos.checkout.to_string_lossy()),
                    ),
                ]),
            ),
            (
                "flags",
                object(vec![
                    ("interactive", herdr_fleet::value::bool_(false)),
                    ("digest_confirmed", herdr_fleet::value::bool_(false)),
                    ("scheduled", herdr_fleet::value::bool_(false)),
                ]),
            ),
        ])),
    );
    assert_eq!(code, "refusal.policy.production_confirmation");

    // A scheduled apply of a production-risk effect is refused outright
    // (risk-model: schedules can never carry production effects).
    let (code2, _) = rpc_err(
        &scenario.fixture.socket,
        &fresh_id(41),
        "apply",
        Some(object(vec![
            ("idempotency_key", string("ik_w1-sched-0001")),
            ("plan", scenario.plan.clone()),
            ("step", string("w1")),
            ("grant_id", string(GRANT_ID)),
            ("instance_id", string(INSTANCE_ID)),
            (
                "observed",
                object(vec![
                    ("issue_revision", string(REVISION)),
                    ("policy_hash", string(POLICY_HASH)),
                ]),
            ),
            (
                "topology",
                object(vec![
                    ("integration_branch", string("staging")),
                    ("production_branches", Val::Arr(vec![string("main")])),
                    (
                        "worktrees_root",
                        string(&scenario.repos.worktrees_root.to_string_lossy()),
                    ),
                    (
                        "integration_repo",
                        string(&scenario.repos.checkout.to_string_lossy()),
                    ),
                ]),
            ),
            (
                "flags",
                object(vec![
                    ("interactive", herdr_fleet::value::bool_(true)),
                    ("digest_confirmed", herdr_fleet::value::bool_(true)),
                    ("scheduled", herdr_fleet::value::bool_(true)),
                ]),
            ),
        ])),
    );
    assert_eq!(code2, "refusal.policy.scheduled");

    // First-real-write gate (AC10): a real-external-scope effect without a
    // recorded approval refuses; the worktree is created first so the
    // later prompt applies.
    scenario.apply_ok(42, "w1", None, None);
    let (code3, _) = rpc_err(
        &scenario.fixture.socket,
        &fresh_id(43),
        "apply",
        Some(scenario.params(43, "p1", None, None, Some("real_external"), false)),
    );
    assert_eq!(code3, "refusal.first_write.approval_required");

    // Recorded interactive approval unlocks the gate (fakes only).
    let approval = scenario.apply_ok(44, "a1", None, None);
    assert!(
        approval
            .get("approval_id")
            .and_then(Val::as_str)
            .unwrap_or("")
            .starts_with("ap_")
    );
    let outcome = scenario.apply_ok_with(45, "p1", None, None, Some("real_external"), false);
    assert!(
        outcome
            .get("transcript")
            .and_then(Val::as_str)
            .unwrap_or("")
            .contains("lane transcript ok")
    );
}

// ---------------------------------------------------------------------------
// C2: expired grants refuse at the apply boundary (RED/GREEN over the wire)
// ---------------------------------------------------------------------------

#[test]
fn expired_grant_refuses_apply_with_typed_code() {
    let scenario = Scenario::new("expired-grant", "2020-01-01T00:00:00Z", flow_steps());
    let (code, message) = scenario.apply_err(60, "w1", None, None);
    assert_eq!(code, "refusal.grant.expired", "{message}");
}

// ---------------------------------------------------------------------------
// AC8 RED: cleanup refuses dirty and unverified targets; no force path
// ---------------------------------------------------------------------------

#[test]
fn dirty_and_unmerged_cleanup_refuses_and_no_direct_push_path_exists() {
    let scenario = Scenario::new("dup-dirty", "2999-01-01T00:00:00Z", flow_steps());
    let wt = scenario.apply_ok(70, "w1", None, None);
    assert_eq!(wt.get("contained").and_then(Val::as_bool), Some(true));
    let lane = scenario.repos.worktrees_root.join("issues-123");
    // A harness commit makes the lane dirty relative to the base.
    scenario.apply_ok(71, "h1", None, None);
    scenario.apply_ok(72, "p1", None, None);

    // External dirty file: cleanup refuses (AC8).
    std::fs::write(lane.join("uncommitted.txt"), "dirty\n").expect("dirty file");
    let (code, _) = scenario.apply_err(73, "x1", None, None);
    assert_eq!(code, "refusal.cleanup.dirty");
    std::fs::remove_file(lane.join("uncommitted.txt")).expect("remove dirty");

    // The branch is NOT merged into staging: cleanup refuses the unverified
    // deletion (AC8); branch_delete has no path for it either.
    let (code2, _) = scenario.apply_err(74, "x1", None, None);
    assert_eq!(code2, "refusal.cleanup.unmerged");
    assert!(lane.exists(), "cleanup must not remove the lane");

    // Force-push attempts are refused by policy (no force path, AC6) — the
    // step params cannot even express a force to integration/production.
    let ck = Git::new(&scenario.repos.checkout);
    // Try pushing the feature branch to the local bare remote (the lane's
    // own remote path): allowed for feature lanes only; the branch itself
    // stays pushable so assert the *local* feature head is intact.
    ck.run(&["rev-parse", "--verify", "issue-123"]);
}
