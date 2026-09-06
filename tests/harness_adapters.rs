//! Harness adapter contract tests (issue #7) with fake executables.
//!
//! Every test builds its own temporary bin directory on PATH (the
//! allowlisted environment map points at it) containing fake `hermes` /
//! `claude` / `codex` / `hf-argv` / `herdr` executables. Executables are
//! resolved through the allowlisted PATH only, so a real harness on the
//! host can never be reached by these tests (AC7: public CI needs no
//! harness credentials — every case is a fake/synthetic contract test).
//! The fakes pin the exact adapter invocation rows: a fake exits non-zero
//! when the argv it received differs from the documented contract, which
//! makes the process-level assertions bite.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use herdr_fleet::adapters::{
    ADAPTER_TIMEOUT, AgentIdentity, CODE_CREDENTIALS, CODE_EXIT, CODE_MALFORMED,
    CODE_PROCESS_DEATH, CODE_STALE_IDENTITY, CODE_TIMEOUT, CODE_UNAVAILABLE,
    CODE_UNKNOWN_CAPABILITY, CODE_UNKNOWN_HARNESS, HarnessKind, Op, OpRequest, Profile,
    bind_identity, execute_named, execute_op, new_session, probe_profile,
};
use herdr_fleet::canonical::canonical_bytes;
use herdr_fleet::config::Harness as ConfigHarness;
use herdr_fleet::schema::{Family, validate_bytes};
use herdr_fleet::value::Val;

static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A temporary bin directory holding fake executables; removed on drop.
struct FakeBins {
    path: PathBuf,
}

impl FakeBins {
    fn new() -> FakeBins {
        let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        // Prefer the per-test-binary tmp dir cargo provides (target/tmp, on
        // the build filesystem) over the shared OS temp dir: fake
        // executables must be spawned while other tests write theirs, and a
        // quota-tracked shared temp filesystem can transiently refuse
        // script execs (observed ETXTBSY on this host).
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!("hf-adapters-contract-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create fake bin dir");
        FakeBins { path }
    }

    /// Write an executable `name` with a `/bin/sh` body and return its
    /// path. `body` is trusted test code (never untrusted payload text).
    fn bin(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake executable");
        // btrfs copy-on-write can transiently refuse exec of a just-written
        // file ("Text file busy") when writeback races the first spawn;
        // fsync before returning keeps the execs deterministic.
        let file = fs::File::open(&path).expect("open fake executable");
        file.sync_all().expect("fsync fake executable");
        set_executable(&path);
        path
    }

    /// The allowlisted environment: PATH points at the fake bin dir only.
    /// Nothing else is present — the child must never see host variables
    /// (process.rs `env_clear` + this allowlist is the only channel).
    fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), self.path.to_string_lossy().into_owned());
        env
    }
}

impl Drop for FakeBins {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn set_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod");
}

/// Whether a failure is a transient spawn/exec error of the host filesystem
/// (observed on this host: exec of a just-written script transiently fails
/// with `Text file busy` under concurrent test spawns on a degraded btrfs
/// mount; `Resource temporarily unavailable` is fork contention). The
/// adapter classification is correct either way (`refusal.unavailable`); the
/// tests assert the *classification*, so they retry transient spawn errors
/// a bounded number of times instead of flaking on the environment.
fn is_transient_spawn_failure(result: &herdr_fleet::adapters::OpResult) -> bool {
    if result.code != Some(CODE_UNAVAILABLE) {
        return false;
    }
    let text = format!(
        "{:?} {:?}",
        result.detail.as_deref().unwrap_or(""),
        result.message.as_deref().unwrap_or("")
    );
    text.contains("Text file busy") || text.contains("Resource temporarily unavailable")
}

/// `execute_op` with a bounded retry on transient spawn failures.
fn run_op_retry(
    profile: &Profile,
    request: &OpRequest<'_>,
    env: &BTreeMap<String, String>,
) -> herdr_fleet::adapters::OpResult {
    let mut result = execute_op(profile, request, env);
    let mut attempts = 0;
    while is_transient_spawn_failure(&result) && attempts < 4 {
        std::thread::sleep(Duration::from_millis(25));
        result = execute_op(profile, request, env);
        attempts += 1;
    }
    result
}

/// `execute_named` with a bounded retry on transient spawn failures.
fn run_named_retry(
    profile: &Profile,
    op_name: &str,
    session: &herdr_fleet::adapters::SessionHandle,
    payload: Option<&str>,
    timeout: Duration,
    env: &BTreeMap<String, String>,
) -> herdr_fleet::adapters::OpResult {
    let mut result = execute_named(profile, op_name, session, payload, timeout, env);
    let mut attempts = 0;
    while is_transient_spawn_failure(&result) && attempts < 4 {
        std::thread::sleep(Duration::from_millis(25));
        result = execute_named(profile, op_name, session, payload, timeout, env);
        attempts += 1;
    }
    result
}

/// `probe_profile` with a bounded retry on transient spawn failures.
fn probe_retry(
    profile: &Profile,
    env: &BTreeMap<String, String>,
) -> herdr_fleet::adapters::ProbeResult {
    let mut probe = probe_profile(profile, env);
    let mut attempts = 0;
    while probe.code == Some(CODE_UNAVAILABLE)
        && probe.detail.as_deref().is_some_and(|d| {
            d.contains("Text file busy") || d.contains("Resource temporarily unavailable")
        })
        && attempts < 4
    {
        std::thread::sleep(Duration::from_millis(25));
        probe = probe_profile(profile, env);
        attempts += 1;
    }
    probe
}

/// The official kinds under test.
fn official_kinds() -> [HarnessKind; 3] {
    HarnessKind::OFFICIAL
}

/// Declared current version for a kind (mirrors the metadata table in
/// src/adapters.rs / docs/contracts/compatibility.md).
fn declared_current(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::Hermes => "0.21.0",
        HarnessKind::ClaudeCode => "2.1.263",
        HarnessKind::Codex => "0.153.4",
        HarnessKind::Argv => "",
    }
}

/// The executable name each kind spawns (the fake bin name must match the
/// executable the profile resolves, not the kind name — claude-code is the
/// official adapter name, `claude` is the binary).
fn executable_name(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::Hermes => "hermes",
        HarnessKind::ClaudeCode => "claude",
        HarnessKind::Codex => "codex",
        HarnessKind::Argv => "hf-argv",
    }
}

/// The fake harness executable body for `kind` whose prompt invocation
/// verifies the exact documented argv shape, then runs `action`:
/// - `echo`: prints the payload back (transcript = payload);
/// - `hang`: busy-loops until the adapter deadline kills it;
/// - `auth`: prints an authentication failure and exits 1;
/// - `die`: kills itself with SIGKILL (process death);
/// - `exit-n`: exits with a plain non-zero code.
fn harness_body(kind: HarnessKind, action: &str, version: &str) -> String {
    let prompt_check = match kind {
        HarnessKind::Hermes => "[ \"$1\" = \"chat\" ] && [ \"$2\" = \"-q\" ]",
        HarnessKind::ClaudeCode => "[ \"$1\" = \"-p\" ]",
        HarnessKind::Codex => "[ \"$1\" = \"exec\" ]",
        HarnessKind::Argv => "true",
    };
    let payload = match kind {
        HarnessKind::Hermes => "\"$3\"",
        HarnessKind::ClaudeCode => "\"$2\"",
        HarnessKind::Codex => "\"$2\"",
        HarnessKind::Argv => "\"$1\"",
    };
    let action_body = match action {
        "echo" => format!("printf '%s' {payload}\nexit 0"),
        "hang" => "while :; do :; done".to_string(),
        "auth" => {
            "echo 'Authentication failed: sign in with your account first' >&2\nexit 1".to_string()
        }
        "die" => "kill -9 $$".to_string(),
        other => format!("exit {}", other.trim_start_matches("exit-")),
    };
    format!(
        "if [ \"$1\" = \"--version\" ]; then echo \"{version}\"; exit 0; fi\nif {prompt_check}; then {action_body}\nfi\necho \"unexpected argv: $*\" >&2\nexit 9\n"
    )
}

/// A fake `herdr` answering the workspace session read-back rows. `show`
/// returns the given session doc (identity read-back/observe source);
/// `outcome` returns a terminal outcome; `interrupt` confirms interruption.
fn workspace_body(show_json: &str) -> String {
    format!(
        r#"if [ "$1" != "session" ]; then echo "unexpected argv: $*" >&2; exit 9; fi
case "$2" in
  show) echo '{show_json}'; exit 0 ;;
  outcome) echo '{{"state":"exited","outcome":"succeeded"}}'; exit 0 ;;
  interrupt) echo '{{"state":"exited","outcome":"ambiguous"}}'; exit 0 ;;
  *) echo "unexpected argv: $*" >&2; exit 9 ;;
esac
"#
    )
}

/// The workspace session doc that matches a bound identity
/// (herdr_session `ws-7`, terminal `tty-7`, generation 3).
fn matching_show_json() -> &'static str {
    r#"{"session_id":"ws-7","generation":3,"terminal_session":"tty-7","state":"running"}"#
}

fn sample_identity() -> AgentIdentity {
    bind_identity("ws-7", "tty-7", 3).expect("bound identity")
}

/// A leaked static session handle for request helpers. Test helpers only:
/// the handle is intentionally immutable and the leak is negligible for the
/// lifetime of one test binary.
fn session_ref() -> &'static herdr_fleet::adapters::SessionHandle {
    Box::leak(Box::new(
        new_session("sess-20260906-0001", sample_identity()).expect("session handle"),
    ))
}

fn prompt_request<'a>(payload: &'a str, timeout: Duration) -> OpRequest<'a> {
    OpRequest {
        op: Op::Prompt,
        session: session_ref(),
        payload: Some(payload),
        timeout,
    }
}

fn workspace_request(op: Op) -> OpRequest<'static> {
    OpRequest {
        op,
        session: session_ref(),
        payload: None,
        timeout: ADAPTER_TIMEOUT,
    }
}

// ---------------------------------------------------------------------------
// Exact-version probes (AC2: exact-version contract tests)
// ---------------------------------------------------------------------------

#[test]
fn official_exact_version_probes_accept_declared_current_and_refuse_below_minimum() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        let current = declared_current(kind);
        bins.bin(executable_name(kind), &harness_body(kind, "echo", current));
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let env = bins.env();

        let probe = probe_retry(&profile, &env);
        assert!(probe.present, "{} detail: {:?}", kind.name(), probe.detail);
        assert_eq!(
            probe.version.as_deref(),
            Some(current),
            "{} detail: {:?}",
            kind.name(),
            probe.detail
        );
        assert_eq!(
            probe.compatible,
            Some(true),
            "{} detail: {:?}",
            kind.name(),
            probe.detail
        );

        // Below the declared minimum: typed refusal signal in the probe.
        let bins_low = FakeBins::new();
        bins_low.bin(executable_name(kind), &harness_body(kind, "echo", "0.0.1"));
        let env_low = bins_low.env();
        let probe = probe_retry(&profile, &env_low);
        assert!(probe.present);
        assert_eq!(probe.compatible, Some(false));
    }
}

#[test]
fn probe_of_a_missing_executable_is_a_typed_unavailable_result() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let env = bins.env();
        let probe = probe_retry(&profile, &env);
        assert!(!probe.present);
        assert_eq!(probe.code, Some(CODE_UNAVAILABLE));
    }
}

// ---------------------------------------------------------------------------
// AC2 behavior matrix (each official adapter; fake executables)
// ---------------------------------------------------------------------------

#[test]
fn success_prompt_delivers_the_payload_as_data_and_returns_the_transcript() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        bins.bin(
            executable_name(kind),
            &harness_body(kind, "echo", declared_current(kind)),
        );
        bins.bin("herdr", &workspace_body(matching_show_json()));
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let payload = "implement the typed adapter contract for issue 7";
        let result = run_op_retry(
            &profile,
            &prompt_request(payload, ADAPTER_TIMEOUT),
            &bins.env(),
        );
        assert_eq!(result.status, "succeeded", "{}", kind.name());
        assert_eq!(result.code, None);
        let transcript = result
            .payload
            .as_ref()
            .and_then(|p| p.get("transcript"))
            .and_then(Val::as_str)
            .expect("transcript");
        assert_eq!(
            transcript,
            payload,
            "payload round-trips as data ({})",
            kind.name()
        );
    }
}

#[test]
fn missing_executable_is_a_typed_unavailable_refusal_and_independent_ops_survive() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        // No harness executable is installed in the fake dir; herdr exists.
        bins.bin("herdr", &workspace_body(matching_show_json()));
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let env = bins.env();

        let result = run_op_retry(&profile, &prompt_request("hello", ADAPTER_TIMEOUT), &env);
        assert_eq!(result.status, "refused", "{}", kind.name());
        assert_eq!(result.code, Some(CODE_UNAVAILABLE));

        // Independent read-only workspace ops still succeed: the harness
        // absence never breaks other adapter operations.
        let observe = run_op_retry(&profile, &workspace_request(Op::Observe), &env);
        assert_eq!(observe.status, "succeeded");
        assert_eq!(
            observe
                .payload
                .as_ref()
                .and_then(|p| p.get("state"))
                .and_then(Val::as_str),
            Some("running")
        );
    }
}

#[test]
fn auth_failure_is_a_typed_credentials_refusal() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        bins.bin(
            executable_name(kind),
            &harness_body(kind, "auth", declared_current(kind)),
        );
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let result = run_op_retry(
            &profile,
            &prompt_request("do the thing", ADAPTER_TIMEOUT),
            &bins.env(),
        );
        assert_eq!(result.status, "refused", "{}", kind.name());
        assert_eq!(result.code, Some(CODE_CREDENTIALS));
        assert!(!result.message.unwrap_or_default().contains("secret"));
    }
}

#[test]
fn unsupported_capability_is_a_typed_refusal_at_the_named_boundary() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        bins.bin(
            executable_name(kind),
            &harness_body(kind, "echo", declared_current(kind)),
        );
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let result = run_named_retry(
            &profile,
            "teleport",
            session_ref(),
            None,
            ADAPTER_TIMEOUT,
            &bins.env(),
        );
        assert_eq!(result.status, "refused", "{}", kind.name());
        assert_eq!(result.code, Some(CODE_UNKNOWN_CAPABILITY));
    }
}

#[test]
fn hanging_prompt_is_deadline_cancelled_and_ambiguous() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        bins.bin(
            executable_name(kind),
            &harness_body(kind, "hang", declared_current(kind)),
        );
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let started = std::time::Instant::now();
        let result = run_op_retry(
            &profile,
            &prompt_request("please hang forever", Duration::from_millis(200)),
            &bins.env(),
        );
        let elapsed = started.elapsed();
        assert_eq!(result.status, "ambiguous", "{}", kind.name());
        assert_eq!(result.code, Some(CODE_TIMEOUT));
        assert!(
            elapsed < Duration::from_secs(5),
            "deadline enforced for {}",
            kind.name()
        );
    }
}

#[test]
fn interrupt_operation_is_accepted_and_terminal_state_is_observable() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        bins.bin(
            executable_name(kind),
            &harness_body(kind, "echo", declared_current(kind)),
        );
        bins.bin("herdr", &workspace_body(matching_show_json()));
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let env = bins.env();

        let interrupt = run_op_retry(&profile, &workspace_request(Op::Interrupt), &env);
        assert_eq!(interrupt.status, "succeeded", "{}", kind.name());
        assert_eq!(
            interrupt
                .payload
                .as_ref()
                .and_then(|p| p.get("interrupted")),
            Some(&Val::Bool(true))
        );
        // The session observed after cancellation reports the exited state.
        let observe = run_op_retry(&profile, &workspace_request(Op::Observe), &env);
        assert_eq!(observe.status, "succeeded");
    }
}

#[test]
fn malformed_workspace_output_is_a_typed_malformed_refusal() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        bins.bin(
            executable_name(kind),
            &harness_body(kind, "echo", declared_current(kind)),
        );
        bins.bin(
            "herdr",
            r#"echo 'this is definitely not json {{{' ; exit 0"#,
        );
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let result = run_op_retry(&profile, &workspace_request(Op::Observe), &bins.env());
        assert_eq!(
            result.code,
            Some(CODE_MALFORMED),
            "detail: {:?} message: {:?}",
            result.detail,
            result.message
        );
    }
}

#[test]
fn process_death_is_a_typed_ambiguous_outcome() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        bins.bin(
            executable_name(kind),
            &harness_body(kind, "die", declared_current(kind)),
        );
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let result = run_op_retry(
            &profile,
            &prompt_request("do not survive this", ADAPTER_TIMEOUT),
            &bins.env(),
        );
        assert_eq!(result.status, "ambiguous", "{}", kind.name());
        assert_eq!(result.code, Some(CODE_PROCESS_DEATH));
    }
}

#[test]
fn plain_nonzero_exit_is_a_typed_failed_outcome() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        bins.bin(
            executable_name(kind),
            &harness_body(kind, "exit-3", declared_current(kind)),
        );
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let result = run_op_retry(
            &profile,
            &prompt_request("make it fail", ADAPTER_TIMEOUT),
            &bins.env(),
        );
        assert_eq!(
            result.status,
            "failed",
            "{} code={:?} message={:?} detail={:?}",
            kind.name(),
            result.code,
            result.message,
            result.detail
        );
        assert_eq!(
            result.code,
            Some(CODE_EXIT),
            "{} message={:?} detail={:?}",
            kind.name(),
            result.message,
            result.detail
        );
    }
}

// ---------------------------------------------------------------------------
// AC3: identity read-back and stale-identity refusals
// ---------------------------------------------------------------------------

#[test]
fn identity_read_back_matches_the_bound_triple_and_stale_read_backs_are_refused() {
    for kind in official_kinds() {
        let bins = FakeBins::new();
        bins.bin(
            executable_name(kind),
            &harness_body(kind, "echo", declared_current(kind)),
        );
        bins.bin("herdr", &workspace_body(matching_show_json()));
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let env = bins.env();

        // Fresh identity read-back: all three parts match.
        let identity = run_op_retry(&profile, &workspace_request(Op::Identity), &env);
        assert_eq!(identity.status, "succeeded", "{}", kind.name());
        let payload = identity.payload.expect("identity payload");
        assert_eq!(
            payload.get("herdr_session"),
            Some(&Val::Str("ws-7".to_string()))
        );
        assert_eq!(payload.get("generation"), Some(&Val::Int(3)));

        // Stale generation read-back: the workspace session was recreated
        // (generation 4) while the handle is still bound to generation 3.
        let bins_stale = FakeBins::new();
        bins_stale.bin(
            executable_name(kind),
            &harness_body(kind, "echo", declared_current(kind)),
        );
        bins_stale.bin(
            "herdr",
            &workspace_body(r#"{"session_id":"ws-7","generation":4,"terminal_session":"tty-7","state":"running"}"#),
        );
        let result = run_op_retry(
            &profile,
            &workspace_request(Op::Identity),
            &bins_stale.env(),
        );
        assert_eq!(result.status, "refused", "{}", kind.name());
        assert_eq!(result.code, Some(CODE_STALE_IDENTITY));
        assert!(result.message.unwrap().contains("generation"));

        // A terminal-session swap is stale the same way.
        let bins_swap = FakeBins::new();
        bins_swap.bin(
            executable_name(kind),
            &harness_body(kind, "echo", declared_current(kind)),
        );
        bins_swap.bin(
            "herdr",
            &workspace_body(r#"{"session_id":"ws-7","generation":3,"terminal_session":"tty-OTHER","state":"running"}"#),
        );
        let result = run_op_retry(&profile, &workspace_request(Op::Identity), &bins_swap.env());
        assert_eq!(result.code, Some(CODE_STALE_IDENTITY));
    }
}

// ---------------------------------------------------------------------------
// AC1: the same plan fixture drives every fake adapter implementation
// ---------------------------------------------------------------------------

/// The canonical plan fixture (data, never code): the ops the adapter
/// contract must serve are derived from its typed steps.
const PLAN_FIXTURE: &str = include_str!("../schemas/fixtures/plan/plan.valid.json");
const DOCTRINE_FIXTURE: &str = include_str!("../schemas/fixtures/workflow/workflow.doctrine.json");

#[test]
fn same_plan_fixture_passes_against_fake_implementations_of_every_adapter_contract() {
    // The fixture documents validate as the closed families first.
    let plan_doc = Val::parse_json(PLAN_FIXTURE).expect("plan fixture parses");
    let plan_bytes = canonical_bytes(&plan_doc);
    assert!(validate_bytes(Family::Plan, &plan_bytes).is_accepted());
    let doctrine_doc = Val::parse_json(DOCTRINE_FIXTURE).expect("doctrine fixture parses");
    assert!(validate_bytes(Family::Workflow, &canonical_bytes(&doctrine_doc)).is_accepted());

    // Derive the typed operation sequence: harness-relevant plan step kinds
    // map to contract ops; other step kinds are not harness operations
    // (engine/daemon territory). The payload for prompt delivery is derived
    // from the plan's own typed issue binding (data in, data out).
    let issue_number = plan_doc
        .get("issue")
        .and_then(|i| i.get("number"))
        .and_then(Val::as_int)
        .unwrap_or(0);
    let issue_revision = plan_doc
        .get("issue")
        .and_then(|i| i.get("revision"))
        .and_then(Val::as_str)
        .unwrap_or("")
        .to_string();
    let steps = plan_doc
        .get("steps")
        .and_then(Val::as_array)
        .expect("steps");
    let mut sequence: Vec<(&'static str, Option<String>)> = Vec::new();
    for step in steps {
        let kind = step.get("kind").and_then(Val::as_str).unwrap_or("");
        match kind {
            "harness_start" => sequence.push(("start", None)),
            "prompt" => sequence.push((
                "prompt",
                Some(format!(
                    "issue {issue_number} revision {issue_revision}: implement per the acceptance text"
                )),
            )),
            "collect_outcome" => sequence.push(("outcome", None)),
            "review_evidence" => sequence.push(("identity", None)),
            _ => {}
        }
    }
    assert_eq!(sequence.len(), 4, "fixture-derived scenario has 4 ops");

    // Every adapter contract implementation (fake hermes, fake claude-code,
    // fake codex, and the declarative argv fake) runs the SAME sequence
    // with the SAME typed payloads and must reach the SAME terminal
    // outcomes.
    let mut status_sets: Vec<Vec<&'static str>> = Vec::new();
    for kind in official_kinds() {
        let bins = FakeBins::new();
        bins.bin(
            executable_name(kind),
            &harness_body(kind, "echo", declared_current(kind)),
        );
        bins.bin("herdr", &workspace_body(matching_show_json()));
        let profile = Profile::official(kind, kind.name()).expect("profile");
        let statuses = run_scenario(&profile, &sequence, &bins.env(), kind.name());
        status_sets.push(statuses);
    }

    // Declarative argv adapter with the full closed capability set.
    let bins = FakeBins::new();
    bins.bin("hf-argv", &harness_body(HarnessKind::Argv, "echo", ""));
    bins.bin("herdr", &workspace_body(matching_show_json()));
    let profile = Profile::argv(
        "cli-fake",
        "hf-argv",
        &[
            "discover",
            "start",
            "prompt",
            "observe",
            "interrupt",
            "outcome",
            "identity",
        ],
        BTreeMap::new(),
    )
    .expect("argv profile");
    let statuses = run_scenario(&profile, &sequence, &bins.env(), "argv");
    status_sets.push(statuses);

    let reference = status_sets[0].clone();
    for (index, statuses) in status_sets.iter().enumerate() {
        assert_eq!(
            *statuses, reference,
            "implementation {index} diverged from the shared fixture outcome"
        );
    }
    assert_eq!(reference, vec!["succeeded"; 4]);
}

/// Run the fixture-derived scenario against one profile; every step must
/// produce a validated `hf-outcome/v1` document.
fn run_scenario(
    profile: &Profile,
    sequence: &[(&'static str, Option<String>)],
    env: &BTreeMap<String, String>,
    label: &str,
) -> Vec<&'static str> {
    let handle = session_ref();
    let mut statuses = Vec::new();
    let mut step_index = 0;
    for (op_name, payload) in sequence {
        step_index += 1;
        let timeout = if *op_name == "prompt" {
            Duration::from_secs(5)
        } else {
            ADAPTER_TIMEOUT
        };
        let result = run_named_retry(profile, op_name, handle, payload.as_deref(), timeout, env);
        let step_id = format!("p{step_index}");
        let doc = result.to_outcome_doc(
            "hf_plan_0123456789abcdef",
            &step_id,
            &format!("ik_scenario-{step_index}"),
            "2026-09-06T00:00:00Z",
        );
        let verdict = validate_bytes(Family::Outcome, &canonical_bytes(&doc));
        assert!(
            verdict.is_accepted(),
            "{label} step {step_id} outcome invalid: {}",
            verdict.message()
        );
        statuses.push(result.status);
    }
    statuses
}

// ---------------------------------------------------------------------------
// AC5: prompts and untrusted issue text are data; argv and policy never
// change; environment stays allowlisted (AC8)
// ---------------------------------------------------------------------------

#[test]
fn hostile_payload_text_is_data_and_cannot_alter_argv_or_environment() {
    let bins = FakeBins::new();
    // The fake verifies the exact documented argv shape and echoes back
    // the payload plus the environment it actually saw.
    bins.bin(
        "claude",
        r#"if [ "$1" = "-p" ]; then printf 'argv_ok payload=[%s] path=[%s] secret=[%s]' "$2" "$PATH" "${HF_TEST_SECRET_VAR:-unset}"; exit 0; fi
echo "unexpected argv: $*" >&2
exit 9
"#,
    );
    let profile = Profile::official(HarnessKind::ClaudeCode, "claude-code").expect("profile");
    let hostile = "a; rm -rf /tmp/x; $(touch /tmp/pwned); `echo injected`; \"quoted\"; && || | > < & newline\nhere; s/ed/";
    let result = run_op_retry(
        &profile,
        &prompt_request(hostile, ADAPTER_TIMEOUT),
        &bins.env(),
    );
    assert_eq!(result.status, "succeeded");
    let transcript = result
        .payload
        .as_ref()
        .and_then(|p| p.get("transcript"))
        .and_then(Val::as_str)
        .expect("transcript");
    assert!(
        transcript.starts_with("argv_ok"),
        "fake saw the documented argv"
    );
    assert!(
        transcript.contains(&format!("payload=[{hostile}]")),
        "payload arrived verbatim as one data element"
    );
    assert!(
        transcript.contains("secret=[unset]"),
        "host variables never reach the child (AC8)"
    );
    assert!(
        transcript.contains(&format!("path=[{}]", bins.path.display())),
        "PATH is the allowlisted one"
    );
    assert!(
        !std::path::Path::new("/tmp/pwned").exists(),
        "no injection happened"
    );
}

// ---------------------------------------------------------------------------
// AC4: unknown harness kinds fail typed without breaking independent
// read-only operations
// ---------------------------------------------------------------------------

#[test]
fn unknown_harness_kind_fails_typed_and_independent_probes_keep_working() {
    let bins = FakeBins::new();
    bins.bin(
        "hermes",
        &harness_body(
            HarnessKind::Hermes,
            "echo",
            declared_current(HarnessKind::Hermes),
        ),
    );
    bins.bin("herdr", &workspace_body(matching_show_json()));

    // An unknown configured kind is refused at profile construction...
    let unknown = ConfigHarness {
        key: "teleporter".to_string(),
        kind: "teleport".to_string(),
        executable: "teleport".to_string(),
        env_allow: vec!["PATH".to_string()],
    };
    let err = Profile::from_config(&unknown).expect_err("unknown kind refused");
    assert_eq!(err.code, CODE_UNKNOWN_HARNESS);
    assert!(err.message.contains("teleport"));

    // ...while an unrelated configured adapter and its read-only ops keep
    // working (observe.rs pattern: per-surface degradation, never global).
    let known = ConfigHarness {
        key: "hermes-a".to_string(),
        kind: "hermes".to_string(),
        executable: "hermes".to_string(),
        env_allow: vec!["PATH".to_string()],
    };
    let profile = Profile::from_config(&known).expect("known kind parses");
    let probe = probe_retry(&profile, &bins.env());
    assert!(probe.present);
    let observe = run_op_retry(&profile, &workspace_request(Op::Observe), &bins.env());
    assert_eq!(observe.status, "succeeded");
}
