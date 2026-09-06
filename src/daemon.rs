//! The local daemon: the single writer that owns SQLite fleet state and
//! serves the versioned hf-rpc-request/v1 protocol over a per-user Unix
//! socket (issue #5, AC1/AC2/AC4/AC5/AC7/AC8/AC10).
//!
//! Design invariants implemented here:
//!
//! - One daemon per host/user: `DaemonLock` (flock + stale recovery) is
//!   acquired before the socket binds, so a second daemon can never become a
//!   concurrent writer (AC1).
//! - Mutations journal their intent durably (hash-chained audit + claim)
//!   *before* any effect and resolve with a typed outcome and a recorded
//!   response; replaying the same request id + idempotency key returns the
//!   recorded response (AC6). A crash anywhere between intent and outcome is
//!   reconciled on restart: the claim is marked ambiguous with a typed
//!   outcome and a new key is required (AC4).
//! - Every journaled state change appends an `hf-event/v1` row; `events
//!   .subscribe` connections receive the response, then a snapshot when the
//!   cursor is stale/absent, then the contiguous replay, then live events
//!   (seq-ordered, one line each). Subscriber queues are bounded; a
//!   subscriber that does not drain is disconnected (AC7).
//! - Logs are JSONL with only allowlisted fields and redacted bounded
//!   summaries (AC8); there is no network listener, usage reporting,
//!   auto-update path, or notification integration anywhere (AC10).
//! - `HERDR_FLEET_CRASH_POINT` aborts the process at a named journal
//!   boundary in **debug builds only**; release binaries ignore it, so it
//!   cannot be weaponized against a production daemon.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

use crate::backup;
use crate::canonical::canonical_text;
use crate::dirs::DaemonPaths;
use crate::lock::{DaemonLock, bind_listener};
use crate::redact::redact;
use crate::schema::{Family, RPC_METHODS, Refusal, validate_doc};
use crate::state::{AuditRow, ClaimAttempt, State, StateError, StateSummary};
use crate::time;
use crate::value::{Val, bool_, integer, null, object, string};

/// Maximum accepted request line length (bounded memory per connection).
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Upper bound for one replay window read. The default event retention
/// (2000 rows) is smaller, so a replay can never be truncated by this cap;
/// the bound exists to keep one hub-locked read finite.
pub const REPLAY_MAX_LINES: i64 = 8192;
/// Per-subscriber bounded event queue; a subscriber that does not drain it
/// is disconnected (bounded backpressure, AC7).
pub const SUBSCRIBER_QUEUE_CAP: usize = 64;
/// Upper bound on journal records served per `journal.tail` call.
pub const JOURNAL_TAIL_LIMIT: i64 = 2000;
/// Fallback id used for responses to unparseable request bytes (schema-valid
/// 8-lowercase-hex; such requests carry no id to echo).
pub const FALLBACK_REQUEST_ID: &str = "00000000";
/// Canonical plan/step identity used by daemon-owned outcome documents.
pub const DAEMON_PLAN_ID: &str = "hf_plan_0000000000000000";
/// Canonical step id used by daemon-owned outcome documents.
pub const DAEMON_STEP_ID: &str = "daemon";

/// A daemon startup/serve failure (maps to CLI exit codes by the caller).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonError {
    /// Stable code (`daemon.busy`, `daemon.state`, `daemon.bind`,
    /// `daemon.io`, `daemon.reconcile`).
    pub code: &'static str,
    /// Human message.
    pub message: String,
}

fn daemon_error(code: &'static str, message: impl Into<String>) -> DaemonError {
    DaemonError {
        code,
        message: message.into(),
    }
}

impl From<crate::dirs::PathError> for DaemonError {
    fn from(err: crate::dirs::PathError) -> DaemonError {
        DaemonError {
            code: "daemon.io",
            message: format!("{}: {}", err.code, err.message),
        }
    }
}

impl From<StateError> for DaemonError {
    fn from(err: StateError) -> DaemonError {
        DaemonError {
            code: "daemon.state",
            message: format!("{}: {}", err.code, err.message),
        }
    }
}

/// Allowlisted daemon log (JSONL; AC8). Only structured fields with a
/// redacted, bounded summary — full request bodies are never persisted.
#[derive(Debug)]
pub struct DaemonLog {
    path: std::path::PathBuf,
}

impl DaemonLog {
    /// Open the log path, rotating an oversized previous log at startup.
    pub fn open(path: &std::path::Path) -> Result<DaemonLog, DaemonError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                daemon_error("daemon.io", format!("create {}: {err}", parent.display()))
            })?;
        }
        if let Ok(meta) = std::fs::metadata(path)
            && meta.len() > 8 * 1024 * 1024
        {
            let _ = std::fs::rename(path, path.with_extension("log.old"));
        }
        Ok(DaemonLog {
            path: path.to_path_buf(),
        })
    }

    /// Append one record: `{schema?no: ts, level, event, message}` with
    /// `message` a redacted, bounded summary (never a full payload).
    pub fn write(&self, level: &str, event: &str, message: &str) {
        let summary = bounded(redact(message).as_str(), 300);
        let line = canonical_text(&object(vec![
            ("ts", string(&time::rfc3339_now())),
            ("level", string(level)),
            ("event", string(event)),
            ("message", string(&summary)),
        ]));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            file.write_all(line.as_bytes())?;
            Ok(())
        })();
        if let Err(err) = result {
            eprintln!("daemon log write failed: {err}");
        }
    }
}

/// Bound a summary string to `cap` chars at a char boundary.
fn bounded(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// One registered event subscriber (bounded queue).
struct Subscriber {
    sender: SyncSender<String>,
}

/// Event fan-out hub. Locking rule (documented, deadlock-free by order):
/// the hub lock may be held while a *brief* state read happens
/// (`compute_replay`), but the hub is never acquired while a state guard is
/// held — state mutations release their guard before publishing.
struct Hub {
    subscribers: Vec<Subscriber>,
    /// Highest event seq already fanned out by this daemon run.
    last_published: i64,
}

impl Hub {
    fn new(last_published: i64) -> Hub {
        Hub {
            subscribers: Vec::new(),
            last_published,
        }
    }

    /// Publish one canonical event line; a subscriber whose queue is full
    /// (or gone) is dropped (bounded backpressure).
    fn publish(&mut self, line: &str) {
        let mut retained = Vec::with_capacity(self.subscribers.len());
        for subscriber in self.subscribers.drain(..) {
            match subscriber.sender.try_send(line.to_string()) {
                Ok(()) => retained.push(subscriber),
                Err(TrySendError::Full(_)) => {
                    // Backpressure: the subscriber is not draining; drop it.
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
        self.subscribers = retained;
    }
}

/// Everything shared between connection threads.
struct Shared {
    state: Mutex<State>,
    hub: Mutex<Hub>,
    log: DaemonLog,
    paths: DaemonPaths,
    pid: u32,
    started_at: String,
    running: AtomicBool,
}

impl Shared {
    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, State>, String> {
        self.state
            .lock()
            .map_err(|_| "state mutex poisoned".to_string())
    }
}

/// A parsed and validated request plus its raw canonical line.
struct Request {
    id: String,
    method: String,
    params: Option<Val>,
    line: String,
}

/// The outcome of handling one request.
enum HandleOutcome {
    /// One response line.
    Response(String),
    /// A subscribe response, then a switch to event-stream mode.
    Subscribed {
        response: String,
        cursor: Option<i64>,
    },
}

/// Run the daemon until a fatal serve error (used by `daemon run`). On
/// return the lock is released, the lease dropped, and the socket unlinked.
pub fn serve(paths: &DaemonPaths) -> Result<(), DaemonError> {
    paths.prepare()?;
    let started_at = time::rfc3339_now();
    // Single-writer lock: a second daemon is refused with the owner detail.
    let _lock = DaemonLock::acquire(&paths.lock_path, &started_at).map_err(|err| {
        daemon_error(
            "daemon.busy",
            match err.code {
                "lock.busy" => format!("another daemon is already running: {}", err.message),
                _ => err.message,
            },
        )
    })?;
    // Reclaim a stale socket from a crashed previous daemon, then bind.
    let listener = bind_listener(&paths.socket_path)
        .map_err(|err| daemon_error("daemon.bind", err.message))?;

    let state = State::open(&paths.db_path, crate::state::Retention::default())
        .map_err(|err| daemon_error("daemon.state", format!("{}: {}", err.code, err.message)))?;
    let log = DaemonLog::open(&paths.log_path)?;
    log.write(
        "info",
        "daemon.start",
        &format!("pid {} serving", std::process::id()),
    );

    // Restart reconciliation BEFORE the daemon accepts requests (AC4):
    // claims left `claimed` by an interrupted run become ambiguous, and any
    // interrupted mutation needs a fresh key before it may retry.
    let mirrored = state.rebuild_audit_mirror(&paths.audit_mirror_path)?;
    if mirrored {
        log.write(
            "warn",
            "journal.mirror.rebuilt",
            "audit mirror drifted or was missing; rebuilt from the journal table",
        );
    }
    let events_mirrored = state.rebuild_events_mirror(&paths.events_mirror_path)?;
    if events_mirrored {
        log.write(
            "warn",
            "events.mirror.rebuilt",
            "events mirror drifted or was missing; rebuilt from the events table",
        );
    }
    let reconciled = reconcile_claims(&state, &log)?;
    // Cold-boot schedule recovery (issue #9 AC2/AC9): every due schedule
    // fires at most ONE fresh coalesced evaluation per boot (missed windows
    // are skipped, never replayed), refused schedules park themselves, and
    // paused schedules stay paused. Runs even when Herdr is absent or
    // unhealthy: the evaluation path is daemon-state only and never spawns.
    match crate::lifecycle::reconcile_schedules(&state, time::unix_now(), None) {
        Ok(summary) => {
            if !summary.ran.is_empty() || !summary.paused.is_empty() {
                log.write(
                    "info",
                    "schedules.reconciled",
                    &format!(
                        "{} ran once; {} parked ({}); {} idle",
                        summary.ran.len(),
                        summary.paused.len(),
                        summary
                            .paused
                            .iter()
                            .map(|(_, reason)| reason.as_str())
                            .collect::<Vec<_>>()
                            .join(","),
                        summary.idle.len()
                    ),
                );
            }
        }
        Err(err) => {
            return Err(daemon_error(
                "daemon.reconcile",
                format!("schedule recovery failed: {}: {}", err.code, err.message),
            ));
        }
    }

    let (max_seq, _) = state
        .event_bounds()
        .map_err(|err| daemon_error("daemon.state", format!("{}: {}", err.code, err.message)))?;
    state.put_daemon_lease(std::process::id(), &started_at)?;
    let shared = Arc::new(Shared {
        state: Mutex::new(state),
        hub: Mutex::new(Hub::new(max_seq.unwrap_or(0))),
        log,
        paths: paths.clone(),
        pid: std::process::id(),
        started_at,
        running: AtomicBool::new(true),
    });
    shared.log.write(
        "info",
        "daemon.ready",
        &format!(
            "state open; reconciled {} interrupted claim(s); serving {}",
            reconciled,
            paths.socket_path.display()
        ),
    );

    let result = serve_loop(&shared, &listener);
    // Graceful cleanup: drop the lease and unlink the socket so the next
    // start classifies it Absent rather than Stale.
    let state = shared
        .lock_state()
        .map_err(|message| daemon_error("daemon.io", message))?;
    let _ = state.drop_daemon_lease();
    drop(state);
    let _ = std::fs::remove_file(&paths.socket_path);
    result
}

/// Accept connections and handle requests until a fatal accept error.
fn serve_loop(shared: &Arc<Shared>, listener: &UnixListener) -> Result<(), DaemonError> {
    for stream in listener.incoming() {
        if !shared.running.load(Ordering::SeqCst) {
            break;
        }
        match stream {
            Ok(stream) => {
                let shared = Arc::clone(shared);
                std::thread::spawn(move || {
                    if let Err(err) = handle_connection(&shared, stream) {
                        shared
                            .log
                            .write("warn", "connection.error", &err.to_string());
                    }
                });
            }
            Err(err) => {
                return Err(daemon_error("daemon.io", format!("accept failed: {err}")));
            }
        }
    }
    Ok(())
}

/// Serve one client connection: request/response lines until EOF, with an
/// event-stream handoff for `events.subscribe`.
fn handle_connection(shared: &Arc<Shared>, stream: UnixStream) -> Result<(), String> {
    let reader_stream = stream.try_clone().map_err(|err| err.to_string())?;
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|err| format!("read request: {err}"))?;
        if read == 0 {
            return Ok(()); // peer closed; clean end.
        }
        if line.len() > MAX_REQUEST_BYTES {
            writer
                .write_all(unsafe_response("request exceeds the maximum size").as_bytes())
                .and_then(|()| writer.write_all(b"\n"))
                .map_err(|err| err.to_string())?;
            writer.flush().map_err(|err| err.to_string())?;
            return Err("oversized request line".to_string());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        match handle_request(shared, trimmed) {
            HandleOutcome::Response(response) => {
                writer
                    .write_all(response.as_bytes())
                    .and_then(|()| writer.write_all(b"\n"))
                    .and_then(|()| writer.flush())
                    .map_err(|err| format!("write response: {err}"))?;
            }
            HandleOutcome::Subscribed { response, cursor } => {
                writer
                    .write_all(response.as_bytes())
                    .and_then(|()| writer.write_all(b"\n"))
                    .and_then(|()| writer.flush())
                    .map_err(|err| format!("write response: {err}"))?;
                return serve_event_stream(shared, &mut writer, cursor);
            }
        }
    }
}

/// A schema-valid error response for bytes we could not parse (no id to
/// echo; the fallback id is documented).
fn unsafe_response(code: &str) -> String {
    response_line(FALLBACK_REQUEST_ID, false, None, code, code)
}

/// Parse and validate one request line, then dispatch.
fn handle_request(shared: &Arc<Shared>, line: &str) -> HandleOutcome {
    let parsed = Val::parse_json(line);
    let doc = match parsed {
        Ok(doc) => doc,
        Err(message) => {
            shared.log.write("warn", "request.parse", &message);
            return HandleOutcome::Response(unsafe_response("request does not parse"));
        }
    };
    let verdict = validate_doc(Family::RpcRequest, &doc);
    if !verdict.is_accepted() {
        // The id on an invalid doc is attacker-controlled (bounded only by
        // the request line cap): echo a capped version, never raw bytes.
        let id = bounded(
            doc.get("id")
                .and_then(Val::as_str)
                .unwrap_or(FALLBACK_REQUEST_ID),
            64,
        );
        let class = verdict
            .refusal()
            .map(|refusal| refusal_code(refusal).to_string())
            .unwrap_or_else(|| "refusal.malformed".to_string());
        shared.log.write(
            "warn",
            "request.refused",
            &format!("{class}: {}", verdict.message()),
        );
        return HandleOutcome::Response(response_line(&id, false, None, &class, verdict.message()));
    }
    let id = doc
        .get("id")
        .and_then(Val::as_str)
        .unwrap_or_default()
        .to_string();
    let method = doc
        .get("method")
        .and_then(Val::as_str)
        .unwrap_or_default()
        .to_string();
    let params = match doc.get("params") {
        Some(Val::Obj(_)) => Some(doc.get("params").expect("checked").clone()),
        _ => None,
    };
    let request = Request {
        id,
        method,
        params,
        line: canonical_text(&doc),
    };
    if request.method == "events.subscribe" {
        return subscribe_request(shared, &request);
    }
    HandleOutcome::Response(dispatch(shared, &request))
}

/// Map a schema refusal class onto the stable dotted RPC error code.
fn refusal_code(class: Refusal) -> &'static str {
    match class {
        Refusal::Parse => "refusal.parse",
        Refusal::Schema => "refusal.schema",
        Refusal::Version => "refusal.version",
        Refusal::Malformed => "refusal.malformed",
        Refusal::Noncanonical => "refusal.noncanonical",
    }
}

/// Build one canonical hf-rpc-response/v1 line.
fn response_line(id: &str, ok: bool, result: Option<Val>, code: &str, message: &str) -> String {
    let (result, error) = if ok {
        (result.unwrap_or_else(|| object(vec![])), null())
    } else {
        (
            null(),
            object(vec![
                ("code", string(code)),
                ("message", string(message)),
                ("retryable", bool_(false)),
            ]),
        )
    };
    canonical_text(&object(vec![
        ("schema", string("hf-rpc-response/v1")),
        ("id", string(id)),
        ("ok", bool_(ok)),
        ("result", result),
        ("error", error),
    ]))
}

fn ok_response(id: &str, result: Val) -> String {
    response_line(id, true, Some(result), "", "")
}

fn err_response(id: &str, code: &str, message: impl Into<String>) -> String {
    response_line(id, false, None, code, &message.into())
}

// ---------------------------------------------------------------------------
// Request dispatch (closed method set; unknown methods refused typed)
// ---------------------------------------------------------------------------

fn dispatch(shared: &Arc<Shared>, request: &Request) -> String {
    match request.method.as_str() {
        "capabilities" => method_capabilities(request),
        "doctor" => method_doctor(shared, request),
        "status" => method_status(shared, request),
        "state.epoch" => method_state_epoch(shared, request),
        "schedules.list" => method_schedules(shared, request),
        "schedules.create" => method_schedule_create(shared, request),
        "schedules.pause" => method_schedule_pause(shared, request),
        "schedules.resume" => method_schedule_resume(shared, request),
        "schedules.delete" => method_schedule_delete(shared, request),
        "schedules.evaluate" => method_schedule_evaluate(shared, request),
        "grants.list" => method_grants_list(shared, request),
        "journal.tail" => method_journal_tail(shared, request),
        "grants.revoke" => method_grants_revoke(shared, request),
        "backup.create" => method_backup_create(shared, request),
        "restore.begin" => method_restore_begin(shared, request),
        "plan" => method_plan(shared, request),
        "apply" => method_apply(shared, request),
        other => {
            shared.log.write("warn", "method.unknown", other);
            err_response(
                &request.id,
                "refusal.method",
                format!("unknown method {other:?}"),
            )
        }
    }
}

fn method_capabilities(request: &Request) -> String {
    let methods: Vec<Val> = RPC_METHODS.iter().map(|m| string(m)).collect();
    let result = object(vec![
        ("protocol", string("hf-rpc/v1")),
        (
            "schema_families",
            Val::Arr(vec![
                string("hf-rpc-request/v1"),
                string("hf-rpc-response/v1"),
                string("hf-event/v1"),
                string("hf-audit/v1"),
            ]),
        ),
        ("methods", Val::Arr(methods)),
        ("max_request_bytes", integer(MAX_REQUEST_BYTES as i64)),
        ("event_queue_cap", integer(SUBSCRIBER_QUEUE_CAP as i64)),
    ]);
    ok_response(&request.id, result)
}

fn method_doctor(shared: &Arc<Shared>, request: &Request) -> String {
    match summary_val(shared) {
        Ok(summary) => ok_response(
            &request.id,
            object(vec![
                ("state_writable", bool_(!summary.poisoned)),
                ("schema_version", integer(summary.schema_version)),
                ("epoch", integer(summary.epoch)),
                ("journal_seq", integer(summary.audit_seq)),
                ("event_seq", integer(summary.event_seq)),
                ("pending_claims", integer(summary.pending_claims)),
                ("active_grants", integer(summary.active_grants)),
            ]),
        ),
        Err(err) => err_response(&request.id, err.code, err.message),
    }
}

fn summary_val(shared: &Arc<Shared>) -> Result<StateSummary, StateError> {
    let state = shared.lock_state().map_err(|message| StateError {
        code: "state.unavailable",
        message,
    })?;
    state.status_summary()
}

fn method_status(shared: &Arc<Shared>, request: &Request) -> String {
    let summary = match summary_val(shared) {
        Ok(summary) => summary,
        Err(err) => return err_response(&request.id, err.code, err.message),
    };
    let result = object(vec![
        (
            "daemon",
            object(vec![
                ("pid", integer(shared.pid as i64)),
                ("started_at", string(&shared.started_at)),
                ("version", string(crate::PACKAGE_VERSION)),
            ]),
        ),
        (
            "state",
            object(vec![
                ("epoch", integer(summary.epoch)),
                ("journal_seq", integer(summary.audit_seq)),
                ("event_seq", integer(summary.event_seq)),
                ("schema_version", integer(summary.schema_version)),
                ("active_grants", integer(summary.active_grants)),
                ("pending_claims", integer(summary.pending_claims)),
                ("poisoned", bool_(summary.poisoned)),
            ]),
        ),
        ("freshness", string("fresh")),
    ]);
    ok_response(&request.id, result)
}

fn method_state_epoch(shared: &Arc<Shared>, request: &Request) -> String {
    let state = shared
        .lock_state()
        .map_err(|message| err_response(&request.id, "state.unavailable", message));
    match state {
        Ok(state) => match state.epoch_doc() {
            Ok(doc) => ok_response(&request.id, object(vec![("epoch", doc)])),
            Err(err) => err_response(&request.id, err.code, err.message),
        },
        Err(response) => response,
    }
}

// ---------------------------------------------------------------------------
// Control-plane mutations (issue #8): `plan` render + `apply` dispatch
// ---------------------------------------------------------------------------

/// `plan`: render a deterministic `hf-plan/v1` document for one repository
/// issue from typed params (the same offline plan family the read-only CLI
/// renders, validated and digest-bound here so `apply` can bind it).
fn method_plan(_shared: &Arc<Shared>, request: &Request) -> String {
    let params = match &request.params {
        Some(params) => params,
        None => {
            return err_response(
                &request.id,
                "refusal.malformed",
                "plan requires params: repository, issue.number, issue.revision",
            );
        }
    };
    let repository = match params.get("repository").and_then(Val::as_str) {
        Some(value) if crate::formats::is_repository_identity(value) => value.to_string(),
        _ => {
            return err_response(
                &request.id,
                "refusal.malformed",
                "plan requires an owner/name repository identity",
            );
        }
    };
    let (owner, name) = repository.split_once('/').expect("validated identity");
    let issue_number = match params
        .get("issue")
        .and_then(|issue| issue.get("number"))
        .and_then(Val::as_int)
    {
        Some(number) if number > 0 => number,
        _ => {
            return err_response(
                &request.id,
                "refusal.malformed",
                "plan requires a positive issue.number",
            );
        }
    };
    let revision = match params
        .get("issue")
        .and_then(|issue| issue.get("revision"))
        .and_then(Val::as_str)
    {
        Some(value) if crate::formats::is_hex40(value) => value.to_string(),
        _ => {
            return err_response(
                &request.id,
                "refusal.malformed",
                "plan requires an exact 40-hex issue.revision",
            );
        }
    };
    let branch = params
        .get("branch")
        .and_then(Val::as_str)
        .unwrap_or("staging")
        .to_string();
    // Repository origin is not part of a rendered plan document; the
    // synthesized value is never persisted or displayed (identity only).
    let repo = crate::config::Repository {
        key: name.to_string(),
        owner: owner.to_string(),
        name: name.to_string(),
        origin: format!("https://example.invalid/{owner}/{name}"),
        branch: Some(branch),
        enabled: true,
    };
    let input = crate::plan::PlanInput {
        repository: &repo,
        issue_number: issue_number as u64,
        revision: revision.clone(),
        workflow: None,
    };
    match crate::plan::render_plan(&input) {
        Ok(rendered) => ok_response(
            &request.id,
            object(vec![
                ("plan", rendered.doc),
                ("digest", string(&rendered.digest)),
                ("plan_id", string(&rendered.plan_id)),
                ("workflow_id", string(&rendered.workflow_id)),
                ("workflow_hash", string(&rendered.workflow_hash)),
            ]),
        ),
        Err(refusal) => err_response(
            &request.id,
            "refusal.plan.unavailable",
            format!("cannot render plan: {:?}", refusal),
        ),
    }
}

/// Typed apply params (parsed once, then validated under the state lock).
struct ApplyParams {
    /// Plan document (schema-validated and digest-bound by the caller).
    plan: Val,
    /// Step id within the plan.
    step: String,
    /// Route grant id.
    grant_id: String,
    /// Workflow instance id.
    instance_id: String,
    /// Fresh observations (issue revision, policy hash, optional heads).
    issue_revision: String,
    policy_hash: String,
    /// Exact heads observed before the effect (merge gate bindings).
    feature_head: Option<String>,
    integration_base: Option<String>,
    /// Topology: integration branch + production branches + lane paths.
    integration_branch: String,
    production_branches: Vec<String>,
    worktrees_root: std::path::PathBuf,
    integration_repo: std::path::PathBuf,
    /// Daemon-owned archive/salvage root (optional; issue #9 AC7).
    archive_root: Option<std::path::PathBuf>,
    /// Production/hotfix/first-write flag bundle (typed, from the caller's
    /// interactive session — never from recurring automation).
    interactive: bool,
    digest_confirmed: bool,
    scheduled: bool,
    production_confirmation: Option<String>,
    target_scope: Option<String>,
    /// Fan-out admission bundle (issue #9 AC1): concurrency caps, the
    /// attested same-harness lane count, and the fresh host-resource proof.
    admission: Option<AdmissionParams>,
}

/// Typed `flags.admission` bundle for fan-out steps (harness_start/prompt).
#[derive(Clone, Debug, PartialEq, Eq)]
struct AdmissionParams {
    /// Global concurrency cap declared for this fan-out (>= 0).
    global_cap: Option<i64>,
    /// Per-repository concurrency cap declared for this fan-out (>= 0).
    repository_cap: Option<i64>,
    /// Per-harness concurrency cap declared for this fan-out (>= 0).
    harness_cap: Option<i64>,
    /// Client-attested count of active lanes on the same harness key
    /// (>= 0). Harness occupancy is not durable daemon state, so it is
    /// attested like the apply `observed` params (same trust boundary).
    harness_lanes: Option<i64>,
    /// Unix seconds when the host-resource measurement was taken.
    host_proof_at: Option<i64>,
}

fn apply_params(request: &Request) -> Result<ApplyParams, (String, String)> {
    let params = request.params.as_ref().ok_or_else(|| {
        (
            "refusal.malformed".to_string(),
            "apply requires params".to_string(),
        )
    })?;
    let get_str = |key: &str| -> Result<String, (String, String)> {
        params
            .get(key)
            .and_then(Val::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                (
                    "refusal.malformed".to_string(),
                    format!("apply requires params.{key}"),
                )
            })
    };
    let plan = match params.get("plan") {
        Some(Val::Obj(_)) => params.get("plan").expect("checked").clone(),
        _ => {
            return Err((
                "refusal.malformed".to_string(),
                "apply requires params.plan (hf-plan/v1 object)".to_string(),
            ));
        }
    };
    let step = get_str("step")?;
    let grant_id = get_str("grant_id")?;
    if !crate::formats::is_grant_id(&grant_id) {
        return Err((
            "refusal.malformed".to_string(),
            "grant_id must be a gr_ id".to_string(),
        ));
    }
    let instance_id = get_str("instance_id")?;
    // The observed field must be an object when present.
    let observed = match params.get("observed") {
        Some(Val::Obj(_)) => params.get("observed").expect("checked"),
        None | Some(Val::Null) => {
            return Err((
                "refusal.malformed".to_string(),
                "apply requires params.observed with fresh issue_revision/policy_hash".to_string(),
            ));
        }
        Some(_) => {
            return Err((
                "refusal.malformed".to_string(),
                "apply params.observed must be an object".to_string(),
            ));
        }
    };
    let issue_revision = observed
        .get("issue_revision")
        .and_then(Val::as_str)
        .filter(|text| crate::formats::is_hex40(text))
        .map(str::to_string)
        .ok_or_else(|| {
            (
                "refusal.malformed".to_string(),
                "observed.issue_revision must be 40-hex".to_string(),
            )
        })?;
    let policy_hash = observed
        .get("policy_hash")
        .and_then(Val::as_str)
        .filter(|text| crate::formats::is_hex64(text))
        .map(str::to_string)
        .ok_or_else(|| {
            (
                "refusal.malformed".to_string(),
                "observed.policy_hash must be 64-hex".to_string(),
            )
        })?;
    let hex40 = |key: &str| -> Result<Option<String>, (String, String)> {
        match observed.get(key).and_then(Val::as_str) {
            Some(text) if crate::formats::is_hex40(text) => Ok(Some(text.to_string())),
            Some(_) => Err((
                "refusal.malformed".to_string(),
                format!("observed.{key} must be 40-hex when present"),
            )),
            None => Ok(None),
        }
    };
    let feature_head = hex40("feature_head")?;
    let integration_base = hex40("integration_base")?;
    let topology = match params.get("topology") {
        Some(Val::Obj(_)) => params.get("topology").expect("checked"),
        _ => {
            return Err((
                "refusal.malformed".to_string(),
                "apply requires params.topology (integration_branch, lane paths)".to_string(),
            ));
        }
    };
    let integration_branch = topology
        .get("integration_branch")
        .and_then(Val::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            (
                "refusal.malformed".to_string(),
                "topology.integration_branch is required".to_string(),
            )
        })?;
    let production_branches = match topology.get("production_branches") {
        None | Some(Val::Null) => Vec::new(),
        Some(Val::Arr(items)) => items
            .iter()
            .filter_map(Val::as_str)
            .map(str::to_string)
            .collect(),
        Some(_) => {
            return Err((
                "refusal.malformed".to_string(),
                "topology.production_branches must be an array".to_string(),
            ));
        }
    };
    let absolute = |key: &str| -> Result<std::path::PathBuf, (String, String)> {
        let text = topology.get(key).and_then(Val::as_str).ok_or_else(|| {
            (
                "refusal.malformed".to_string(),
                format!("topology.{key} is required"),
            )
        })?;
        let path = std::path::PathBuf::from(text);
        if !path.is_absolute() {
            return Err((
                "refusal.malformed".to_string(),
                format!("topology.{key} must be an absolute path"),
            ));
        }
        Ok(path)
    };
    let worktrees_root = absolute("worktrees_root")?;
    let integration_repo = absolute("integration_repo")?;
    // archive_root is optional but must be absolute when present.
    let archive_root = match topology.get("archive_root") {
        None | Some(Val::Null) => None,
        Some(Val::Str(_)) => Some(absolute("archive_root")?),
        Some(_) => {
            return Err((
                "refusal.malformed".to_string(),
                "topology.archive_root must be a string when present".to_string(),
            ));
        }
    };
    let flags = match params.get("flags") {
        None | Some(Val::Null) => None,
        Some(Val::Obj(_)) => params.get("flags"),
        Some(_) => {
            return Err((
                "refusal.malformed".to_string(),
                "apply params.flags must be an object".to_string(),
            ));
        }
    };
    let flag = |key: &str| -> bool {
        flags
            .and_then(|f| f.get(key))
            .and_then(Val::as_bool)
            .unwrap_or(false)
    };
    // Fan-out admission bundle (issue #9 AC1): flags.admission object with
    // caps, an attested same-harness lane count, and host-resource proof.
    // Optional for non-fan-out steps; REQUIRED for harness_start/prompt
    // (the gate refuses fan-out without it).
    let non_negative = |value: Option<&Val>| -> Option<i64> {
        match value {
            Some(Val::Int(seconds)) if *seconds >= 0 => Some(*seconds),
            _ => None,
        }
    };
    let admission = match flags.and_then(|f| f.get("admission")) {
        None | Some(Val::Null) => None,
        Some(Val::Obj(_)) => {
            let admission = flags.and_then(|f| f.get("admission")).expect("checked");
            let caps = admission.get("caps").and_then(|c| {
                if matches!(c, Val::Obj(_)) {
                    Some(c)
                } else {
                    None
                }
            });
            let int_field =
                |key: &str| -> Option<i64> { caps.and_then(|c| non_negative(c.get(key))) };
            let host_proof_at = admission
                .get("host_proof")
                .and_then(|proof| proof.get("measured_at"))
                .and_then(Val::as_str)
                .and_then(|text| crate::time::unix_from_rfc3339(text));
            Some(AdmissionParams {
                global_cap: int_field("global"),
                repository_cap: int_field("repository"),
                harness_cap: int_field("harness"),
                harness_lanes: non_negative(admission.get("harness_lanes")),
                host_proof_at,
            })
        }
        Some(_) => {
            return Err((
                "refusal.malformed".to_string(),
                "apply flags.admission must be an object".to_string(),
            ));
        }
    };
    Ok(ApplyParams {
        plan,
        step,
        grant_id,
        instance_id,
        issue_revision,
        policy_hash,
        feature_head,
        integration_base,
        integration_branch,
        production_branches,
        worktrees_root,
        integration_repo,
        archive_root,
        interactive: flag("interactive"),
        digest_confirmed: flag("digest_confirmed"),
        scheduled: flag("scheduled"),
        production_confirmation: flags
            .and_then(|f| f.get("production_confirmation"))
            .and_then(Val::as_str)
            .map(str::to_string),
        target_scope: flags
            .and_then(|f| f.get("target_scope"))
            .and_then(Val::as_str)
            .map(str::to_string),
        admission,
    })
}

/// `apply`: bind the plan digest, revalidate every binding freshly under
/// the state lock, journal the intent, execute the typed effect, and
/// resolve with a typed outcome + exact read-back (issue #8 AC1/AC2/AC4).
/// Issue #9 AC1 fan-out admission gate (harness_start/prompt): refuse
/// before any intent is journaled when
/// - the caller omitted `flags.admission` or its host-resource proof
///   (`refusal.admission.proof_missing`) or the proof is stale
///   (`refusal.admission.proof_stale`) — unknown/stale measurements refuse;
/// - any applicable cap is missing (`refusal.admission.cap_missing`);
/// - any declared cap is exhausted (cap_global/cap_repository/cap_harness —
///   global and per-repository counts come from durable instance rows, the
///   per-harness count is client-attested like the apply `observed` params
///   because harness occupancy is not durable daemon state);
/// - the lane's declared scope overlaps a concurrent lane's scope in the
///   same repository (`refusal.admission.monorepo_overlap`).
fn admission_gate(
    shared: &Arc<Shared>,
    request: &Request,
    plan: &crate::mutation::PlanBindings,
    parsed: &ApplyParams,
    params: Option<&Val>,
) -> Result<(), String> {
    let Some(admission) = &parsed.admission else {
        return Err(err_response(
            &request.id,
            crate::lifecycle::code::PROOF_MISSING,
            "fan-out requires flags.admission with caps and a fresh host-resource proof (unknown measurements refuse new work)",
        ));
    };
    let caps = match (
        admission.global_cap,
        admission.repository_cap,
        admission.harness_cap,
    ) {
        (Some(global), Some(repository), Some(harness)) => crate::lifecycle::ConcurrencyCaps {
            global: usize::try_from(global).unwrap_or(usize::MAX),
            per_repository: usize::try_from(repository).unwrap_or(usize::MAX),
            per_harness: usize::try_from(harness).unwrap_or(usize::MAX),
        },
        _ => {
            return Err(err_response(
                &request.id,
                crate::lifecycle::code::CAP_MISSING,
                "fan-out requires flags.admission.caps {global, repository, harness}",
            ));
        }
    };
    // Per-harness axis: the attested active-lane count on this harness key.
    let harness_lanes = admission.harness_lanes.unwrap_or(0);
    if usize::try_from(harness_lanes).unwrap_or(usize::MAX) >= caps.per_harness {
        return Err(err_response(
            &request.id,
            crate::lifecycle::code::CAP_HARNESS,
            format!(
                "the per-harness concurrency cap ({}) is reached ({} attested lanes on this harness); refuse fan-out",
                caps.per_harness, harness_lanes
            ),
        ));
    }
    let harness_key = params
        .and_then(|p| p.get("harness_key"))
        .and_then(Val::as_str)
        .unwrap_or("")
        .to_string();
    // State-derived lanes + the proposed footprint.
    let state = match shared.lock_state() {
        Ok(state) => state,
        Err(message) => return Err(err_response(&request.id, "state.unavailable", message)),
    };
    let instance = match state.instance_by_id(&parsed.instance_id) {
        Ok(Some(instance)) => instance,
        Ok(None) => {
            return Err(err_response(
                &request.id,
                "refusal.instance.state",
                format!("no instance {} exists", parsed.instance_id),
            ));
        }
        Err(err) => return Err(err_response(&request.id, err.code, err.message)),
    };
    let proposed = crate::lifecycle::LaneFootprint {
        repository: plan.repository.clone(),
        harness_key,
        scope: instance.scope.clone(),
    };
    let mut running = Vec::new();
    for row in state
        .list_instances()
        .map_err(|err| err_response(&request.id, err.code, err.message))?
    {
        if row.instance_id == parsed.instance_id {
            continue;
        }
        if matches!(
            row.status.as_str(),
            "new" | "running" | "human_queue" | "blocked"
        ) {
            running.push(crate::lifecycle::LaneFootprint {
                repository: row.repository.clone(),
                harness_key: String::new(),
                scope: row.scope.clone(),
            });
        }
    }
    drop(state);
    let host_proof = admission
        .host_proof_at
        .map(|measured_at_unix| crate::lifecycle::HostProof { measured_at_unix });
    crate::lifecycle::check_fanout_admission(
        &proposed,
        &running,
        &caps,
        host_proof,
        time::unix_now(),
    )
    .map_err(|err| err_response(&request.id, err.code, err.message))
}

fn method_apply(shared: &Arc<Shared>, request: &Request) -> String {
    let parsed = match apply_params(request) {
        Ok(parsed) => parsed,
        Err((code, message)) => return err_response(&request.id, &code, message),
    };
    // Bind the plan digest + content identity BEFORE any journaling
    // (malformed/tampered plans never leave a claim behind).
    let plan = match crate::mutation::bind_plan(&parsed.plan) {
        Ok(plan) => plan,
        Err(err) => return err_response(&request.id, err.code, err.message),
    };
    let step = match crate::mutation::plan_step(&plan, &parsed.step) {
        Ok(step) => step,
        Err(err) => return err_response(&request.id, err.code, err.message),
    };
    let kind = match crate::mutation::step_kind(step) {
        Ok(kind) => kind,
        Err(err) => return err_response(&request.id, err.code, err.message),
    };
    let params = match step.get("params") {
        Some(Val::Obj(_)) => step.get("params"),
        None | Some(Val::Null) => None,
        _ => {
            return err_response(
                &request.id,
                "refusal.malformed",
                "plan step params must be an object or null",
            );
        }
    };
    // Daemon-level risk gates that need no state: destructive/production
    // effects are never schedulable; production-branch effects require a
    // fresh interactive TTY-confirmed digest; real-external effects require
    // the recorded first-write approval (AC10 plumbing).
    let risk = crate::mutation::risk_class(&kind).unwrap_or("production");
    let branch_target = params
        .and_then(|p| p.get("branch").or_else(|| p.get("base")))
        .and_then(Val::as_str)
        .unwrap_or("");
    if crate::mutation::classify_branch(
        branch_target,
        &parsed.integration_branch,
        &parsed.production_branches,
    ) == crate::mutation::BranchKind::Production
        && let Err(err) = crate::mutation::check_production_confirmation(
            parsed.production_confirmation.as_deref(),
            parsed.interactive,
            parsed.digest_confirmed,
            parsed.scheduled,
        )
    {
        return err_response(&request.id, err.code, err.message);
    }
    if parsed.scheduled && matches!(risk, "production" | "destructive") {
        return err_response(
            &request.id,
            "refusal.policy.scheduled",
            "schedules and automation can never carry production or destructive effects",
        );
    }
    let _ = &risk;
    // Issue #9 AC1 fan-out admission: harness_start/prompt spawn lane work.
    // Refuse before any intent is journaled when an applicable cap or a
    // fresh host-resource proof is missing/stale, or when the lane's
    // declared scope overlaps a concurrent lane in the same repository.
    if matches!(kind.as_str(), "harness_start" | "prompt")
        && let Err(response) = admission_gate(shared, request, &plan, &parsed, params)
    {
        return response;
    }
    // Journal the durable intent (pre-action audit record + idempotency
    // claim; action mutate.<kind>, target repo:instance:step).
    let action = format!("mutate.{kind}");
    let target = format!("{}:{}:{}", plan.repository, parsed.instance_id, parsed.step);
    let key = match request
        .params
        .as_ref()
        .and_then(|params| params.get("idempotency_key"))
        .and_then(Val::as_str)
        .map(str::to_string)
    {
        Some(key) => key,
        None => {
            return err_response(
                &request.id,
                "refusal.malformed",
                "apply requires params.idempotency_key",
            );
        }
    };
    let grant_id = parsed.grant_id.clone();
    let journaled = {
        let state = match shared.lock_state() {
            Ok(state) => state,
            Err(message) => {
                return err_response(&request.id, "state.unavailable", message);
            }
        };
        match state.journal_intent(
            &action,
            &target,
            &key,
            &request.id,
            &request.method,
            Some(&plan.digest),
            Some(&grant_id),
            &request.line,
        ) {
            Ok((ClaimAttempt::Claimed, _)) => true,
            Ok((ClaimAttempt::Replay { response }, _)) => return replay(shared, &response),
            Ok((ClaimAttempt::Reused { owner_request_id }, _)) => {
                return err_response(
                    &request.id,
                    "refusal.idempotency",
                    format!(
                        "idempotency key {key:?} already belongs to request {owner_request_id}"
                    ),
                );
            }
            Err(err) => return err_response(&request.id, err.code, err.message),
        }
    };
    let _ = journaled;
    // No publish here: every post-journal terminal path below publishes
    // once after its state change (hub-lock ordering rule).

    // Revalidate plan/grant/instance/epoch against FRESH state, then run
    // kind-specific gates that need durable state (evidence, closure).
    // Everything runs inside one state-guard scope; refusals are returned
    // as values and resolved only AFTER the guard drops (re-locking while
    // the guard is held would self-deadlock).
    type SnapshotOutcome = Result<
        (
            crate::mutation::GrantSnapshot,
            crate::mutation::InstanceSnapshot,
            Option<crate::mutation::EvidenceView>,
            Option<crate::state::ApprovalRow>,
        ),
        (String, String),
    >;
    let snapshot_outcome = (|| -> SnapshotOutcome {
        let state = shared
            .lock_state()
            .map_err(|message| ("state.unavailable".to_string(), message))?;
        let epoch = state
            .current_epoch()
            .map_err(|err| (err.code.to_string(), err.message))?;
        let grant = state
            .grant_by_id(&parsed.grant_id)
            .map_err(|err| (err.code.to_string(), err.message))?
            .ok_or_else(|| {
                (
                    "refusal.grant.inactive".to_string(),
                    format!("no grant {} exists", parsed.grant_id),
                )
            })?;
        let instance = state
            .instance_by_id(&parsed.instance_id)
            .map_err(|err| (err.code.to_string(), err.message))?
            .ok_or_else(|| {
                (
                    "refusal.instance.state".to_string(),
                    format!("no instance {} exists", parsed.instance_id),
                )
            })?;
        let caps = |text: &str| -> Vec<String> {
            Val::parse_json(text)
                .ok()
                .and_then(|value| value.as_array().cloned())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Val::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let grant_snapshot = crate::mutation::GrantSnapshot {
            grant_id: grant.grant_id.clone(),
            repository: grant.repository.clone(),
            issue_number: grant.issue_number,
            issue_revision: grant.issue_revision.clone(),
            workflow_hash: grant.workflow_hash.clone(),
            policy_hash: grant.policy_hash.clone(),
            phase: grant.phase.clone(),
            scope: grant.scope.clone(),
            caps: caps(&grant.caps),
            expires_at: grant.expires_at.clone(),
            status: grant.status.clone(),
            state_epoch: grant.state_epoch,
        };
        let instance_snapshot = crate::mutation::InstanceSnapshot {
            instance_id: instance.instance_id.clone(),
            repository: instance.repository.clone(),
            workflow_id: instance.workflow_id.clone(),
            workflow_hash: instance.workflow_hash.clone(),
            policy_hash: instance.policy_hash.clone(),
            grant_id: instance.grant_id.clone(),
            issue_number: instance.issue_number,
            issue_revision: instance.issue_revision.clone(),
            phase: instance.phase.clone(),
            scope: instance.scope.clone(),
            caps: caps(&instance.caps),
            current_node: instance.current_node.clone(),
            paused: instance.paused,
            status: instance.status.clone(),
            state_epoch: instance.state_epoch,
        };
        let observed = crate::mutation::Observed {
            issue_revision: parsed.issue_revision.clone(),
            policy_hash: parsed.policy_hash.clone(),
            state_epoch: epoch,
            now: time::rfc3339_now(),
        };
        let evidence = state
            .evidence_for_instance(&parsed.instance_id)
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .map(|row| crate::mutation::EvidenceView {
                evidence_id: row.evidence_id,
                feature_head: row.feature_head,
                integration_base: row.integration_base,
                workflow_hash: row.workflow_hash,
                policy_hash: row.policy_hash,
                verdict: row.verdict,
                reviewer: row.reviewer,
                checks: row.checks,
                created_at: row.created_at,
            });
        let approval = state
            .approval_for_scope("first-write-canary")
            .map_err(|err| (err.code.to_string(), err.message))?;
        crate::mutation::revalidate_effect(
            &plan,
            &kind,
            &grant_snapshot,
            &instance_snapshot,
            &observed,
        )
        .map_err(|err| (err.code.to_string(), err.message))?;
        // Kind-specific durable gates.
        match kind.as_str() {
            "merge" => {
                let Some(feature_head) = parsed.feature_head.clone() else {
                    return Err((
                        "refusal.malformed".to_string(),
                        "merge requires observed.feature_head".to_string(),
                    ));
                };
                let Some(integration_base) = parsed.integration_base.clone() else {
                    return Err((
                        "refusal.malformed".to_string(),
                        "merge requires observed.integration_base".to_string(),
                    ));
                };
                crate::mutation::check_merge_evidence(
                    evidence.as_ref(),
                    &feature_head,
                    &integration_base,
                    &instance_snapshot.workflow_hash,
                    &parsed.policy_hash,
                )
                .map_err(|err| (err.code.to_string(), err.message))?;
            }
            "issue_update" => {
                let closing =
                    params.and_then(|p| p.get("action")).and_then(Val::as_str) == Some("close");
                if closing {
                    // The closure gate keys on the post-merge-verify STEP of
                    // THIS plan (step ids are plan-local slugs; the engine
                    // persists the achieved step id as current_node). The
                    // refusal decision itself is the single unit-tested
                    // gate in mutation.rs (check_issue_closure) — the daemon
                    // calls it here and nowhere else.
                    let verify_step = plan
                        .doc
                        .get("steps")
                        .and_then(Val::as_array)
                        .and_then(|steps| {
                            steps.iter().find(|step| {
                                step.get("kind").and_then(Val::as_str) == Some("post_merge_verify")
                            })
                        })
                        .and_then(|step| step.get("id").and_then(Val::as_str))
                        .unwrap_or("")
                        .to_string();
                    if verify_step.is_empty() {
                        return Err((
                            "refusal.malformed".to_string(),
                            "the plan has no post_merge_verify step; closure is not routable"
                                .to_string(),
                        ));
                    }
                    crate::mutation::check_issue_closure(
                        evidence.as_ref(),
                        &instance_snapshot.current_node,
                        &verify_step,
                    )
                    .map_err(|err| (err.code.to_string(), err.message))?;
                }
            }
            _ => {}
        }
        Ok((grant_snapshot, instance_snapshot, evidence, approval))
    })();
    let (grant_snapshot, instance_snapshot, latest_evidence, approval) = match snapshot_outcome {
        Ok(bundle) => bundle,
        Err((code, message)) => {
            return resolve_apply_refusal(shared, request, &key, &code, message);
        }
    };

    // The AC10 gate runs before any effect that declares a real external
    // target scope (fakes only in this slice; the real canary is a later
    // separately approved step).
    if matches!(risk, "production" | "destructive")
        && let Err(err) = crate::mutation::check_first_write_approval(
            approval.as_ref(),
            parsed.target_scope.as_deref(),
        )
    {
        return resolve_apply_refusal(shared, request, &key, err.code, err.message);
    }
    let _ = latest_evidence;

    // Execute the effect OUTSIDE the state lock (bounded subprocesses never
    // stall other daemon work; the claim already journals the intent).
    let effect_env = crate::config::adapter_environment();
    let ctx = crate::mutation::EffectContext {
        plan: &plan,
        step_id: &parsed.step,
        kind: &kind,
        params,
        repository: &plan.repository,
        integration_branch: &parsed.integration_branch,
        production_branches: &parsed.production_branches,
        worktrees_root: &parsed.worktrees_root,
        integration_repo: &parsed.integration_repo,
        archive_root: parsed.archive_root.as_deref(),
        observed_feature_head: parsed.feature_head.as_deref(),
        observed_integration_base: parsed.integration_base.as_deref(),
        env: &effect_env,
    };
    let effect = crate::mutation::execute_step(&ctx);
    let mut result = effect.result;
    let effect_status = effect.status;
    let effect_code = effect.code.clone();
    let effect_message = effect.message.clone();
    if effect_status == "succeeded" {
        match kind.as_str() {
            "review_evidence" => {
                let state = match shared.lock_state() {
                    Ok(state) => state,
                    Err(message) => {
                        return finish_apply_refused(
                            shared,
                            request,
                            &key,
                            &action,
                            &plan.plan_id,
                            &parsed.step,
                            "state.unavailable",
                            message,
                        );
                    }
                };
                let checks = result.get("checks").cloned().unwrap_or_else(null);
                let repository = result
                    .get("repository")
                    .and_then(Val::as_str)
                    .unwrap_or("")
                    .to_string();
                let feature_head = result
                    .get("feature_head")
                    .and_then(Val::as_str)
                    .unwrap_or("")
                    .to_string();
                let integration_base = result
                    .get("integration_base")
                    .and_then(Val::as_str)
                    .unwrap_or("")
                    .to_string();
                let verdict = result
                    .get("verdict")
                    .and_then(Val::as_str)
                    .unwrap_or("")
                    .to_string();
                let reviewer = result
                    .get("reviewer")
                    .and_then(Val::as_str)
                    .unwrap_or("")
                    .to_string();
                match state.record_evidence(
                    &parsed.instance_id,
                    &repository,
                    &feature_head,
                    &integration_base,
                    &plan.workflow_hash,
                    &parsed.policy_hash,
                    &verdict,
                    &reviewer,
                    &checks,
                ) {
                    Ok(row) => {
                        let mut fields = match result {
                            Val::Obj(map) => map,
                            _ => unreachable!(),
                        };
                        fields.insert("evidence_id".to_string(), string(&row.evidence_id));
                        fields.insert("recorded_at".to_string(), string(&row.created_at));
                        result = Val::Obj(fields);
                    }
                    Err(err) => {
                        return finish_apply_refused(
                            shared,
                            request,
                            &key,
                            &action,
                            &plan.plan_id,
                            &parsed.step,
                            err.code,
                            err.message,
                        );
                    }
                }
            }
            "cleanup" => {
                // AC8: after the destructive effect succeeded, preserve the
                // required salvage evidence as a post-deletion audit record
                // (the mutate.cleanup intent is the pre-effect record).
                let state = match shared.lock_state() {
                    Ok(state) => state,
                    Err(message) => {
                        return finish_apply_refused(
                            shared,
                            request,
                            &key,
                            &action,
                            &plan.plan_id,
                            &parsed.step,
                            "state.unavailable",
                            message,
                        );
                    }
                };
                let salvage_target = params
                    .and_then(|p| p.get("worktree"))
                    .and_then(Val::as_str)
                    .unwrap_or("");
                if let Err(err) = state.journal_salvage(
                    &format!("{}:{}", plan.repository, salvage_target),
                    &parsed.instance_id,
                ) {
                    return finish_apply_refused(
                        shared,
                        request,
                        &key,
                        &action,
                        &plan.plan_id,
                        &parsed.step,
                        err.code,
                        err.message,
                    );
                }
            }
            "approve" => {
                let state = match shared.lock_state() {
                    Ok(state) => state,
                    Err(message) => {
                        return finish_apply_refused(
                            shared,
                            request,
                            &key,
                            &action,
                            &plan.plan_id,
                            &parsed.step,
                            "state.unavailable",
                            message,
                        );
                    }
                };
                let digest = result
                    .get("digest")
                    .and_then(Val::as_str)
                    .unwrap_or("")
                    .to_string();
                let interactive = matches!(result.get("interactive"), Some(Val::Bool(true)));
                match state.record_approval("first-write-canary", &digest, interactive) {
                    Ok(row) => {
                        let mut fields = match result {
                            Val::Obj(map) => map,
                            _ => unreachable!(),
                        };
                        fields.insert("approval_id".to_string(), string(&row.approval_id));
                        fields.insert("recorded_at".to_string(), string(&row.recorded_at));
                        result = Val::Obj(fields);
                    }
                    Err(err) => {
                        return finish_apply_refused(
                            shared,
                            request,
                            &key,
                            &action,
                            &plan.plan_id,
                            &parsed.step,
                            err.code,
                            err.message,
                        );
                    }
                }
            }
            _ => {}
        }
    }
    drop(grant_snapshot);
    drop(instance_snapshot);

    // Note the achieved step on the instance (current_node), then resolve
    // the claim with the typed outcome (succeeded/failed/refused/ambiguous).
    let state = match shared.lock_state() {
        Ok(state) => state,
        Err(message) => {
            shared.log.write(
                "error",
                "outcome.journal_failed",
                &format!("state lock lost: {message}"),
            );
            return err_response(
                &request.id,
                "state.unavailable",
                "the mutation effect completed but its outcome could not be journaled (fail closed)",
            );
        }
    };
    if effect_status == "succeeded"
        && let Err(err) = state.advance_instance(
            &parsed.instance_id,
            &parsed.step,
            0,
            0,
            false,
            0,
            &time::rfc3339_now(),
        )
    {
        // The effect already landed; a failed node advance must never
        // silently succeed — journal the drift so the outcome stays
        // auditable (kind-gates re-derive most drift, but the record
        // must exist).
        shared.log.write(
            "error",
            "outcome.advance_failed",
            &format!(
                "instance {} did not advance to {} after a succeeded {}: {}",
                parsed.instance_id, parsed.step, action, err.message
            ),
        );
    }
    let response = resolve_apply_effect(
        &state,
        &shared.log,
        request,
        &key,
        &action,
        &plan.plan_id,
        &parsed.step,
        &crate::mutation::EffectOutcome {
            status: effect_status,
            code: effect_code,
            message: effect_message,
            result: null(),
        },
        result,
    );
    drop(state);
    publish_after_state_change(shared, None);
    response
}

/// Resolve an apply whose preconditions refused before any effect ran.
fn resolve_apply_refusal(
    shared: &Arc<Shared>,
    request: &Request,
    key: &str,
    code: &str,
    message: String,
) -> String {
    let state = match shared.lock_state() {
        Ok(state) => state,
        Err(lock_message) => {
            return err_response(&request.id, "state.unavailable", lock_message);
        }
    };
    let outcome = apply_outcome(
        crate::daemon::DAEMON_PLAN_ID,
        crate::daemon::DAEMON_STEP_ID,
        key,
        "refused",
        null(),
        Some((code, message.clone())),
    );
    let response = err_response(&request.id, code, message);
    let resolved = match state.resolve_claim(
        key,
        &request.method,
        "spent",
        &canonical_text(&outcome),
        Some(&response),
    ) {
        Ok(_) => response,
        Err(err) => {
            shared.log.write(
                "error",
                "outcome.journal_failed",
                &format!("{}: {}", err.code, err.message),
            );
            err_response(
                &request.id,
                err.code,
                format!(
                    "the refusal could not be journaled (fail closed): {}",
                    err.message
                ),
            )
        }
    };
    drop(state);
    publish_after_state_change(shared, None);
    resolved
}

/// Resolve an apply whose post-effect durable record failed (the effect
/// itself must be treated as ambiguous: its external side effects may have
/// happened even though the record could not be stored).
#[allow(clippy::too_many_arguments)]
fn finish_apply_refused(
    shared: &Arc<Shared>,
    request: &Request,
    key: &str,
    action: &str,
    plan_id: &str,
    step_id: &str,
    code: &'static str,
    message: String,
) -> String {
    let state = match shared.lock_state() {
        Ok(state) => state,
        Err(lock_message) => {
            return err_response(&request.id, "state.unavailable", lock_message);
        }
    };
    let outcome = apply_outcome(
        plan_id,
        step_id,
        key,
        "ambiguous",
        null(),
        Some((code, message.clone())),
    );
    let response = err_response(&request.id, code, message);
    let resolved = match state.resolve_claim(
        key,
        action,
        "ambiguous",
        &canonical_text(&outcome),
        Some(&response),
    ) {
        Ok(_) => response,
        Err(err) => {
            shared.log.write(
                "error",
                "outcome.journal_failed",
                &format!("{}: {}", err.code, err.message),
            );
            err_response(
                &request.id,
                err.code,
                format!(
                    "the outcome could not be journaled (fail closed): {}",
                    err.message
                ),
            )
        }
    };
    drop(state);
    publish_after_state_change(shared, None);
    resolved
}

/// Resolve a claimed apply effect with its typed outcome (AC1): the outcome
/// status mirrors the effect status; the response records the exact
/// read-back result on success.
#[allow(clippy::too_many_arguments)]
fn resolve_apply_effect(
    state: &State,
    log: &DaemonLog,
    request: &Request,
    key: &str,
    action: &str,
    plan_id: &str,
    step_id: &str,
    effect: &crate::mutation::EffectOutcome,
    result: Val,
) -> String {
    let succeeded = effect.status == "succeeded";
    let (claim_status, response, outcome) = if succeeded {
        let response = ok_response(&request.id, result);
        (
            "spent",
            response.clone(),
            apply_outcome(plan_id, step_id, key, "succeeded", null(), None),
        )
    } else {
        let code = effect
            .code
            .clone()
            .unwrap_or_else(|| "effect.failed".to_string());
        let message = effect
            .message
            .clone()
            .unwrap_or_else(|| "effect failed".to_string());
        let response = err_response(&request.id, &code, &message);
        let claim_status = if effect.status == "ambiguous" {
            "ambiguous"
        } else {
            "spent"
        };
        (
            claim_status,
            response.clone(),
            apply_outcome(
                plan_id,
                step_id,
                key,
                effect.status,
                null(),
                Some((code.as_str(), message)),
            ),
        )
    };
    match state.resolve_claim(
        key,
        action,
        claim_status,
        &canonical_text(&outcome),
        Some(&response),
    ) {
        Ok(_) => response,
        Err(err) => {
            log.write(
                "error",
                "outcome.journal_failed",
                &format!("{}: {}", err.code, err.message),
            );
            err_response(
                &request.id,
                err.code,
                format!(
                    "the mutation effect completed but its outcome could not be journaled \
                     (fail closed): {}",
                    err.message
                ),
            )
        }
    }
}

/// A typed hf-outcome/v1 document for one applied plan step.
fn apply_outcome(
    plan_id: &str,
    step_id: &str,
    key: &str,
    status: &str,
    result: Val,
    error: Option<(&str, String)>,
) -> Val {
    let failed = matches!(status, "failed" | "refused" | "ambiguous");
    object(vec![
        ("schema", string("hf-outcome/v1")),
        ("plan_id", string(plan_id)),
        ("step_id", string(step_id)),
        ("status", string(status)),
        ("idempotency_key", string(key)),
        ("observed_at", string(&time::rfc3339_now())),
        ("result", if failed { null() } else { result }),
        (
            "error",
            match error {
                Some((code, message)) => error_val(code, &message),
                None => null(),
            },
        ),
    ])
}

fn method_schedules(shared: &Arc<Shared>, request: &Request) -> String {
    match shared.lock_state() {
        Ok(state) => match state.list_schedules() {
            Ok(schedules) => ok_response(
                &request.id,
                object(vec![("schedules", Val::Arr(schedules))]),
            ),
            Err(err) => err_response(&request.id, err.code, err.message),
        },
        Err(message) => err_response(&request.id, "state.unavailable", message),
    }
}

/// `schedules.create`: upsert a validated hf-schedule/v1 document (issue
/// #9). Create (or re-arm) resets the cadence: enabled, due immediately,
/// one fresh evaluation — never a replay of missed windows. The durable
/// intent is journaled before the row write like every daemon mutation.
fn method_schedule_create(shared: &Arc<Shared>, request: &Request) -> String {
    let doc = match request
        .params
        .as_ref()
        .and_then(|params| params.get("schedule"))
    {
        Some(Val::Obj(_)) => request
            .params
            .as_ref()
            .and_then(|params| params.get("schedule"))
            .expect("checked")
            .clone(),
        _ => {
            return err_response(
                &request.id,
                "refusal.malformed",
                "schedules.create requires params.schedule (hf-schedule/v1 document)",
            );
        }
    };
    let verdict = crate::schema::validate_doc(crate::schema::Family::Schedule, &doc);
    if !verdict.is_accepted() {
        return err_response(
            &request.id,
            "refusal.malformed",
            format!("schedule document refused: {}", verdict.message()),
        );
    }
    let schedule_id = doc
        .get("schedule_id")
        .and_then(Val::as_str)
        .unwrap_or_default()
        .to_string();
    let target = format!("schedule:{schedule_id}");
    match journal_mutation(shared, request, "mutate.schedule.create", &target) {
        Intent::Claimed { key } => {
            let outcome = {
                let state = match shared.lock_state() {
                    Ok(state) => state,
                    Err(message) => {
                        return err_response(&request.id, "state.unavailable", message);
                    }
                };
                match state.upsert_schedule(&doc) {
                    Ok(row) => Ok(object(vec![("schedule", crate::state::schedule_val(&row))])),
                    Err(err) => Err((err.code, err.message)),
                }
            };
            match outcome {
                Ok(result) => {
                    finish_mutation(shared, request, &key, "schedule.create", true, result, None)
                }
                Err((code, message)) => finish_mutation(
                    shared,
                    request,
                    &key,
                    "schedule.create",
                    false,
                    null(),
                    Some((code, message)),
                ),
            }
        }
        Intent::Replay { response } => replay(shared, &response),
        Intent::Refused { code, message } => err_response(&request.id, code, message),
    }
}

/// `schedules.pause` / `schedules.resume`: durable pause/resume of a
/// schedule (issue #9 AC2/AC3). A paused schedule stays paused across
/// daemon/service/host restarts; nothing in the evaluation path ever
/// enables it (resume is an explicit human RPC that also resets the window
/// to due-now for one fresh evaluation).
fn method_schedule_pause(shared: &Arc<Shared>, request: &Request) -> String {
    method_schedule_set_enabled(shared, request, false)
}

fn method_schedule_resume(shared: &Arc<Shared>, request: &Request) -> String {
    method_schedule_set_enabled(shared, request, true)
}

fn method_schedule_set_enabled(shared: &Arc<Shared>, request: &Request, enabled: bool) -> String {
    let schedule_id = match request
        .params
        .as_ref()
        .and_then(|params| params.get("schedule_id"))
        .and_then(Val::as_str)
    {
        Some(text) if crate::formats::is_schedule_id(text) => text.to_string(),
        _ => {
            return err_response(
                &request.id,
                "refusal.malformed",
                "schedule_id must be an sd_ id",
            );
        }
    };
    let action = if enabled {
        "mutate.schedule.resume"
    } else {
        "mutate.schedule.pause"
    };
    let target = format!("schedule:{schedule_id}");
    match journal_mutation(shared, request, action, &target) {
        Intent::Claimed { key } => {
            let outcome = {
                let state = match shared.lock_state() {
                    Ok(state) => state,
                    Err(message) => {
                        return err_response(&request.id, "state.unavailable", message);
                    }
                };
                match state.set_schedule_enabled(&schedule_id, enabled, &time::rfc3339_now()) {
                    Ok(row) => Ok(object(vec![("schedule", crate::state::schedule_val(&row))])),
                    Err(err) => Err((err.code, err.message)),
                }
            };
            match outcome {
                Ok(result) => finish_mutation(
                    shared,
                    request,
                    &key,
                    if enabled {
                        "schedule.resume"
                    } else {
                        "schedule.pause"
                    },
                    true,
                    result,
                    None,
                ),
                Err((code, message)) => finish_mutation(
                    shared,
                    request,
                    &key,
                    if enabled {
                        "schedule.resume"
                    } else {
                        "schedule.pause"
                    },
                    false,
                    null(),
                    Some((code, message)),
                ),
            }
        }
        Intent::Replay { response } => replay(shared, &response),
        Intent::Refused { code, message } => err_response(&request.id, code, message),
    }
}

/// `schedules.delete`: delete a schedule row (daemon-mediated; the deletion
/// intent is journaled first — deleting schedule state is itself a
/// journaled destructive daemon-state operation).
fn method_schedule_delete(shared: &Arc<Shared>, request: &Request) -> String {
    let schedule_id = match request
        .params
        .as_ref()
        .and_then(|params| params.get("schedule_id"))
        .and_then(Val::as_str)
    {
        Some(text) if crate::formats::is_schedule_id(text) => text.to_string(),
        _ => {
            return err_response(
                &request.id,
                "refusal.malformed",
                "schedule_id must be an sd_ id",
            );
        }
    };
    let target = format!("schedule:{schedule_id}");
    match journal_mutation(shared, request, "mutate.schedule.delete", &target) {
        Intent::Claimed { key } => {
            let outcome = {
                let state = match shared.lock_state() {
                    Ok(state) => state,
                    Err(message) => {
                        return err_response(&request.id, "state.unavailable", message);
                    }
                };
                match state.delete_schedule(&schedule_id) {
                    Ok(()) => Ok(object(vec![
                        ("schedule_id", string(&schedule_id)),
                        ("deleted", bool_(true)),
                    ])),
                    Err(err) => Err((err.code, err.message)),
                }
            };
            match outcome {
                Ok(result) => {
                    finish_mutation(shared, request, &key, "schedule.delete", true, result, None)
                }
                Err((code, message)) => finish_mutation(
                    shared,
                    request,
                    &key,
                    "schedule.delete",
                    false,
                    null(),
                    Some((code, message)),
                ),
            }
        }
        Intent::Replay { response } => replay(shared, &response),
        Intent::Refused { code, message } => err_response(&request.id, code, message),
    }
}

/// `schedules.evaluate`: run one fresh evaluation tick (issue #9 AC2).
/// Each due schedule fires at most ONCE and persists its next window
/// atomically with its journal record; refused schedules park themselves;
/// a second evaluate in the same tick is a no-op (single-flight). The
/// optional `observed` params attest the live policy hash / issue revision
/// (same client-attestation boundary as apply); a mismatch parks the
/// schedule with `policy_changed`/`issue_changed`. Evaluations are not
/// idempotency-claimed: a crash between window advance and journal leaves
/// at most one extra fresh evaluation, never a backlog replay.
fn method_schedule_evaluate(shared: &Arc<Shared>, request: &Request) -> String {
    let params = request.params.as_ref();
    let schedule_id = match params
        .and_then(|params| params.get("schedule_id"))
        .and_then(Val::as_str)
    {
        Some(text) if crate::formats::is_schedule_id(text) => Some(text.to_string()),
        Some(_) => {
            return err_response(
                &request.id,
                "refusal.malformed",
                "schedule_id must be an sd_ id when present",
            );
        }
        None => None,
    };
    let attest = params
        .and_then(|params| params.get("observed"))
        .and_then(|observed| {
            let policy_hash = observed.get("policy_hash").and_then(Val::as_str);
            let issue_revision = observed.get("issue_revision").and_then(Val::as_str);
            match (policy_hash, issue_revision) {
                (Some(policy_hash), Some(issue_revision))
                    if crate::formats::is_hex64(policy_hash)
                        && crate::formats::is_hex40(issue_revision) =>
                {
                    Some(crate::lifecycle::ScheduleAttest {
                        policy_hash,
                        issue_revision,
                    })
                }
                _ => None,
            }
        });
    let state = match shared.lock_state() {
        Ok(state) => state,
        Err(message) => return err_response(&request.id, "state.unavailable", message),
    };
    let now_unix = time::unix_now();
    let summary = match &schedule_id {
        Some(id) => {
            match crate::lifecycle::reconcile_schedule(&state, id, now_unix, attest.as_ref()) {
                Ok(Some(summary)) => summary,
                Ok(None) => {
                    return err_response(
                        &request.id,
                        "refusal.schedule.not_found",
                        format!("no schedule {id:?} exists"),
                    );
                }
                Err(err) => return err_response(&request.id, err.code, err.message),
            }
        }
        None => match crate::lifecycle::reconcile_schedules(&state, now_unix, attest.as_ref()) {
            Ok(summary) => summary,
            Err(err) => return err_response(&request.id, err.code, err.message),
        },
    };
    drop(state);
    ok_response(
        &request.id,
        object(vec![
            ("evaluated", integer(summary.total() as i64)),
            (
                "ran",
                Val::Arr(summary.ran.iter().map(|id| string(id)).collect()),
            ),
            (
                "paused",
                Val::Arr(
                    summary
                        .paused
                        .iter()
                        .map(|(id, reason)| {
                            object(vec![
                                ("schedule_id", string(id)),
                                ("reason", string(reason)),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("idle", integer(summary.idle.len() as i64)),
        ]),
    )
}

fn method_grants_list(shared: &Arc<Shared>, request: &Request) -> String {
    match shared.lock_state() {
        Ok(state) => match state.list_grants() {
            Ok(rows) => {
                let grants: Vec<Val> = rows.iter().map(grant_doc).collect();
                ok_response(&request.id, object(vec![("grants", Val::Arr(grants))]))
            }
            Err(err) => err_response(&request.id, err.code, err.message),
        },
        Err(message) => err_response(&request.id, "state.unavailable", message),
    }
}

fn grant_doc(row: &crate::state::GrantRow) -> Val {
    object(vec![
        ("schema", string("hf-grant/v1")),
        ("grant_id", string(&row.grant_id)),
        ("repository", string(&row.repository)),
        (
            "issue",
            object(vec![
                ("number", integer(row.issue_number)),
                ("revision", string(&row.issue_revision)),
            ]),
        ),
        ("workflow_hash", string(&row.workflow_hash)),
        ("policy_hash", string(&row.policy_hash)),
        ("phase", string(&row.phase)),
        ("scope", string(&row.scope)),
        ("caps", string(&row.caps)),
        ("expires_at", string(&row.expires_at)),
        ("state_epoch", integer(row.state_epoch)),
        ("status", string(&row.status)),
        ("created_at", string(&row.created_at)),
    ])
}

fn method_journal_tail(shared: &Arc<Shared>, request: &Request) -> String {
    let params = request.params.as_ref();
    let after_seq = params
        .and_then(|params| params.get("after_seq"))
        .and_then(as_non_negative)
        .unwrap_or(-1);
    let limit = params
        .and_then(|params| params.get("limit"))
        .and_then(as_positive)
        .unwrap_or(100)
        .min(JOURNAL_TAIL_LIMIT);
    match shared.lock_state() {
        Ok(state) => match state.journal_tail(after_seq, limit) {
            Ok((first_retained, lines)) => {
                let records: Vec<Val> = lines
                    .iter()
                    .filter_map(|line| Val::parse_json(line).ok())
                    .collect();
                ok_response(
                    &request.id,
                    object(vec![
                        ("first_retained_seq", integer(first_retained)),
                        ("records", Val::Arr(records)),
                    ]),
                )
            }
            Err(err) => err_response(&request.id, err.code, err.message),
        },
        Err(message) => err_response(&request.id, "state.unavailable", message),
    }
}

fn as_non_negative(value: &Val) -> Option<i64> {
    match value {
        Val::Int(int) if *int >= 0 => Some(*int),
        _ => None,
    }
}

fn as_positive(value: &Val) -> Option<i64> {
    match value {
        Val::Int(int) if *int > 0 => Some(*int),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Mutation methods (journaled intent -> effect -> typed outcome; AC4/AC6)
// ---------------------------------------------------------------------------

/// The result of a journaled-intent attempt.
enum Intent {
    /// Claimed; the caller runs the effect with this key.
    Claimed { key: String },
    /// Same request already resolved: replay the recorded response.
    Replay { response: String },
    /// The key belongs to a different request: refuse.
    Refused { code: &'static str, message: String },
}

fn journal_mutation(shared: &Arc<Shared>, request: &Request, action: &str, target: &str) -> Intent {
    let state = match shared.lock_state() {
        Ok(state) => state,
        Err(message) => {
            return Intent::Refused {
                code: "state.unavailable",
                message,
            };
        }
    };
    let intent = journal_mutation_on(&state, request, action, target);
    drop(state);
    if matches!(intent, Intent::Claimed { .. }) {
        // Publish the intent event only after the state guard is dropped
        // (hub locking rule: never publish while holding the state mutex).
        publish_after_state_change(shared, None);
    }
    intent
}

/// Journal an intent on an ALREADY-LOCKED state handle. No locking and no
/// publishing: the restore path holds the state mutex across the whole
/// rename/reopen/swap sequence (review-5-daemon-r2 blocker) and publishes
/// once after dropping the guard.
fn journal_mutation_on(state: &State, request: &Request, action: &str, target: &str) -> Intent {
    let key = match request
        .params
        .as_ref()
        .and_then(|params| params.get("idempotency_key"))
        .and_then(Val::as_str)
    {
        Some(key) if crate::formats::is_idempotency_key(key) => key.to_string(),
        _ => {
            return Intent::Refused {
                code: "refusal.malformed",
                message: "mutating methods require params.idempotency_key (ik_ format)".into(),
            };
        }
    };
    match state.journal_intent(
        action,
        target,
        &key,
        &request.id,
        &request.method,
        None,
        None,
        &request.line,
    ) {
        Ok((ClaimAttempt::Claimed, _)) => Intent::Claimed { key },
        Ok((ClaimAttempt::Replay { response }, _)) => Intent::Replay { response },
        Ok((ClaimAttempt::Reused { owner_request_id }, _)) => Intent::Refused {
            code: "refusal.idempotency",
            message: format!(
                "idempotency key {key:?} already belongs to request {owner_request_id}"
            ),
        },
        Err(err) => Intent::Refused {
            code: err.code,
            message: err.message,
        },
    }
}

/// Publish journal events appended by one state change (called after the
/// state guard is dropped; see the hub locking rule).
fn publish_after_state_change(shared: &Arc<Shared>, audit: Option<AuditRow>) {
    let seq = audit.map(|audit| audit.event_seq);
    publish_events(shared);
    let _ = seq; // the event seq is carried by the row itself
}

fn method_grants_revoke(shared: &Arc<Shared>, request: &Request) -> String {
    let Some(grant_id) = request
        .params
        .as_ref()
        .and_then(|params| params.get("grant_id"))
        .and_then(Val::as_str)
        .filter(|id| crate::formats::is_grant_id(id))
    else {
        return err_response(
            &request.id,
            "refusal.malformed",
            "grants.revoke requires params.grant_id (gr_ format)",
        );
    };
    match journal_mutation(shared, request, "mutate.grant.revoke", grant_id) {
        Intent::Claimed { key } => {
            crash_point("grants.after-intent");
            let guard = match shared.lock_state() {
                Ok(guard) => guard,
                Err(message) => {
                    return err_response(&request.id, "state.unavailable", message);
                }
            };
            let response = match guard.revoke_grant(grant_id, &time::rfc3339_now()) {
                Ok(()) => resolve_mutation_on(
                    &guard,
                    &shared.log,
                    request,
                    &key,
                    "grants.revoke",
                    true,
                    object(vec![
                        ("grant_id", string(grant_id)),
                        ("revoked", bool_(true)),
                    ]),
                    None,
                ),
                Err(err) => resolve_mutation_on(
                    &guard,
                    &shared.log,
                    request,
                    &key,
                    "grants.revoke",
                    false,
                    null(),
                    Some((err.code, err.message)),
                ),
            };
            drop(guard);
            publish_after_state_change(shared, None);
            response
        }
        Intent::Replay { response } => replay(shared, &response),
        Intent::Refused { code, message } => err_response(&request.id, code, message),
    }
}

/// Journaled bounded-retention prune of daemon-owned backups (issue #9
/// AC8): the pruning intent (`mutate.backup.prune`) is journaled with its
/// own derived idempotency key before any pair is removed, and the claim is
/// resolved after the prune. Returns the removed snapshot names.
fn prune_backups_with_journal(
    shared: &Arc<Shared>,
    request: &Request,
) -> Result<Vec<String>, (&'static str, String)> {
    let mut key = format!("ik_backup-prune-{}", request.id);
    key.truncate(64);
    {
        let state = shared
            .lock_state()
            .map_err(|message| ("state.unavailable", message))?;
        match state.journal_intent(
            "mutate.backup.prune",
            "backups:prune",
            &key,
            &request.id,
            &request.method,
            None,
            None,
            &request.line,
        ) {
            Ok((ClaimAttempt::Claimed, _)) => {}
            Ok((ClaimAttempt::Replay { response: _ }, _)) => {
                return Err((
                    "refusal.idempotency",
                    "the prune for this request was already recorded".to_string(),
                ));
            }
            Ok((ClaimAttempt::Reused { owner_request_id }, _)) => {
                return Err((
                    "refusal.idempotency",
                    format!("prune key {key:?} belongs to request {owner_request_id}"),
                ));
            }
            Err(err) => return Err((err.code, err.message)),
        }
    }
    let pruned = backup::prune_backups(&shared.paths.backups_dir, backup::BackupPolicy::default())
        .map_err(|err| (err.code, err.message))?;
    let outcome = daemon_outcome(&key, "succeeded", null());
    {
        let state = shared
            .lock_state()
            .map_err(|message| ("state.unavailable", message))?;
        state
            .resolve_claim(
                &key,
                &request.method,
                "spent",
                &canonical_text(&outcome),
                None,
            )
            .map_err(|err| (err.code, err.message))?;
    }
    publish_after_state_change(shared, None);
    Ok(pruned)
}

fn method_backup_create(shared: &Arc<Shared>, request: &Request) -> String {
    match journal_mutation(shared, request, "mutate.backup.create", "backups") {
        Intent::Claimed { key } => {
            crash_point("backup.after-intent");
            let state = match shared.lock_state() {
                Ok(state) => state,
                Err(message) => {
                    return err_response(&request.id, "state.unavailable", message);
                }
            };
            let epoch = match state.current_epoch() {
                Ok(epoch) => epoch,
                Err(err) => {
                    return finish_mutation(
                        shared,
                        request,
                        &key,
                        "backup.create",
                        false,
                        null(),
                        Some((err.code, err.message)),
                    );
                }
            };
            drop(state);
            let (snapshot_path, _manifest_path) =
                backup::new_backup_paths(&shared.paths.backups_dir, epoch);
            let (digest, bytes) =
                match backup::create_snapshot(&shared.paths.db_path, &snapshot_path) {
                    Ok(result) => result,
                    Err(err) => {
                        return finish_mutation(
                            shared,
                            request,
                            &key,
                            "backup.create",
                            false,
                            null(),
                            Some((err.code, err.message)),
                        );
                    }
                };
            crash_point("backup.after-snapshot");
            let (journal_seq, event_seq) = match summary_tuple(shared) {
                Ok(values) => values,
                Err(err) => {
                    return finish_mutation(
                        shared,
                        request,
                        &key,
                        "backup.create",
                        false,
                        null(),
                        Some((err.code, err.message)),
                    );
                }
            };
            let manifest =
                backup::manifest_for(epoch, journal_seq, event_seq, digest, bytes, &snapshot_path);
            if let Err(err) = backup::write_manifest(&snapshot_path, &manifest)
                .and_then(|()| backup::verify_backup(&snapshot_path, &manifest))
            {
                return finish_mutation(
                    shared,
                    request,
                    &key,
                    "backup.create",
                    false,
                    null(),
                    Some((err.code, err.message)),
                );
            }
            crash_point("backup.after-manifest");
            // Bounded retention (issue #9 AC8): prune verified backup pairs
            // outside the default policy. The pruning intent is journaled
            // with its own idempotency key before any pair is removed and
            // resolved after — deleting retention records is itself a
            // journaled daemon-state operation.
            let pruned = match prune_backups_with_journal(shared, request) {
                Ok(pruned) => pruned,
                Err((code, message)) => {
                    return finish_mutation(
                        shared,
                        request,
                        &key,
                        "backup.create",
                        false,
                        null(),
                        Some((code, message)),
                    );
                }
            };
            let pruned_names: Vec<Val> = pruned.iter().map(|name| string(name)).collect();
            let result = object(vec![(
                "backup",
                object(vec![
                    ("snapshot", string(&manifest.snapshot_name)),
                    ("epoch", integer(epoch)),
                    ("journal_seq", integer(journal_seq)),
                    ("event_seq", integer(event_seq)),
                    ("db_sha256", string(&manifest.db_sha256)),
                    ("db_bytes", integer(manifest.db_bytes)),
                    ("created_at", string(&manifest.created_at)),
                    ("pruned", Val::Arr(pruned_names)),
                ]),
            )]);
            finish_mutation(shared, request, &key, "backup.create", true, result, None)
        }
        Intent::Replay { response } => replay(shared, &response),
        Intent::Refused { code, message } => err_response(&request.id, code, message),
    }
}

fn summary_tuple(shared: &Arc<Shared>) -> Result<(i64, i64), StateError> {
    let state = shared.lock_state().map_err(|message| StateError {
        code: "state.unavailable",
        message,
    })?;
    let (_, journal_seq, event_seq, _) = state.summary()?;
    Ok((journal_seq, event_seq))
}

fn method_restore_begin(shared: &Arc<Shared>, request: &Request) -> String {
    let target = request
        .params
        .as_ref()
        .and_then(|params| params.get("backup"))
        .and_then(Val::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("latest");
    let backups = match backup::list_backups(&shared.paths.backups_dir) {
        Ok(backups) => backups,
        Err(err) => return err_response(&request.id, err.code, err.message),
    };
    let selected = if target == "latest" {
        backups.last().cloned()
    } else {
        backups
            .into_iter()
            .find(|manifest| manifest.snapshot_name == target)
    };
    let Some(manifest) = selected else {
        return err_response(
            &request.id,
            "refusal.backup.not_found",
            format!("no verified backup {target:?} is available"),
        );
    };
    let snapshot_path = shared.paths.backups_dir.join(&manifest.snapshot_name);
    if let Err(err) = backup::verify_backup(&snapshot_path, &manifest) {
        return err_response(&request.id, err.code, err.message);
    }

    // EXCLUSIVE restore window (review-5-daemon-r2 blocker): the state
    // mutex is held across intent -> rename -> reopen -> rotate/void ->
    // swap -> re-journal -> resolve. No other handler can journal into the
    // unlinked old file or observe a half-swapped state, because every
    // state access (all mutation methods) takes this same mutex. Events
    // are published only after the guard is dropped (hub locking rule).
    let mut guard = match shared.lock_state() {
        Ok(guard) => guard,
        Err(message) => return err_response(&request.id, "state.unavailable", message),
    };
    // A restore touches the whole database; refuse while interrupted claims
    // are pending (they must reconcile through a restart first, AC4/AC6).
    match guard.claims_in_flight() {
        Ok(claims) if claims.is_empty() => {}
        Ok(_) => {
            return err_response(
                &request.id,
                "refusal.restore.pending_claims",
                "restore refused while interrupted claims are pending; restart the daemon to reconcile them first",
            );
        }
        Err(err) => return err_response(&request.id, err.code, err.message),
    }
    // Journal the durable intent on the CURRENT state handle (still under
    // the exclusive guard, so no other writer can interleave).
    let key = match request
        .params
        .as_ref()
        .and_then(|params| params.get("idempotency_key"))
        .and_then(Val::as_str)
    {
        Some(key) if crate::formats::is_idempotency_key(key) => key.to_string(),
        _ => {
            return err_response(
                &request.id,
                "refusal.malformed",
                "restore.begin requires params.idempotency_key (ik_ format)",
            );
        }
    };
    match guard.journal_intent(
        "mutate.restore.begin",
        &format!("restore:{}", manifest.snapshot_name),
        &key,
        &request.id,
        &request.method,
        None,
        None,
        &request.line,
    ) {
        Ok((ClaimAttempt::Claimed, _)) => {}
        Ok((ClaimAttempt::Replay { response }, _)) => return replay(shared, &response),
        Ok((ClaimAttempt::Reused { owner_request_id }, _)) => {
            return err_response(
                &request.id,
                "refusal.idempotency",
                format!("idempotency key {key:?} already belongs to request {owner_request_id}"),
            );
        }
        Err(err) => return err_response(&request.id, err.code, err.message),
    }
    crash_point("restore.after-intent");
    if let Err(err) = backup::restore_snapshot(&snapshot_path, &shared.paths.db_path) {
        // The live file was NOT replaced: resolve the failure on the current
        // state and publish after the guard drops.
        let response = resolve_mutation_on(
            &guard,
            &shared.log,
            request,
            &key,
            "restore.begin",
            false,
            null(),
            Some((err.code, err.message)),
        );
        drop(guard);
        publish_after_state_change(shared, None);
        return response;
    }
    crash_point("restore.after-rename");
    // The DB file was replaced under the old connection. Reopen the
    // restored database and swap the handle while the exclusive guard is
    // still held, so nothing can journal into the unlinked old file.
    let reopened = match State::open(&shared.paths.db_path, crate::state::Retention::default()) {
        Ok(state) => state,
        Err(err) => {
            shared.log.write(
                "error",
                "outcome.journal_failed",
                &format!("restore reopen failed: {}: {}", err.code, err.message),
            );
            drop(guard);
            publish_after_state_change(shared, None);
            return err_response(
                &request.id,
                err.code,
                format!(
                    "restore renamed the database but reopening it failed; restart the daemon: {}",
                    err.message
                ),
            );
        }
    };
    let rotated_epoch = reopened
        .rotate_epoch("restore")
        .and_then(|epoch| reopened.invalidate_grants_below_current().map(|_| epoch))
        .and_then(|epoch| {
            // The snapshot predates the resolutions of any claims that were
            // in flight when it was taken; void them so a resurrected claim
            // can never dispatch or replay.
            reopened.void_in_flight_claims("restore").map(|_| epoch)
        });
    // Swap unconditionally: the live file is the restored one now, so every
    // later state access (including the re-journal below) must target it.
    *guard = reopened;
    // The pre-effect claim was journaled into the *old* DB, which the rename
    // replaced. Journal the intent again in the restored DB so the outcome
    // resolves durably here (a crash between the rename and this re-journal
    // simply re-executes the same restore on retry — idempotent).
    let response = match journal_mutation_on(
        &guard,
        request,
        "mutate.restore.begin",
        &format!("restore:{}", manifest.snapshot_name),
    ) {
        Intent::Claimed { key } => match rotated_epoch {
            Ok(epoch) => {
                let outcome = object(vec![
                    ("restored_epoch", integer(epoch)),
                    ("snapshot", string(&manifest.snapshot_name)),
                    ("prior_epoch", integer(epoch - 1)),
                ]);
                resolve_mutation_on(
                    &guard,
                    &shared.log,
                    request,
                    &key,
                    "restore.begin",
                    true,
                    outcome,
                    None,
                )
            }
            Err(err) => resolve_mutation_on(
                &guard,
                &shared.log,
                request,
                &key,
                "restore.begin",
                false,
                null(),
                Some((err.code, err.message)),
            ),
        },
        Intent::Replay { response } => replay(shared, &response),
        Intent::Refused { code, message } => err_response(&request.id, code, message),
    };
    drop(guard);
    publish_after_state_change(shared, None);
    response
}

/// Finish a journaled mutation: lock the state, resolve the claim with a
/// typed outcome and the recorded response in one transaction, drop the
/// guard, then publish the outcome event and refresh the bounded event
/// mirror. Returns the response line.
fn finish_mutation(
    shared: &Arc<Shared>,
    request: &Request,
    key: &str,
    method: &str,
    success: bool,
    result: Val,
    error: Option<(&'static str, String)>,
) -> String {
    let response = match shared.lock_state() {
        Ok(state) => resolve_mutation_on(
            &state,
            &shared.log,
            request,
            key,
            method,
            success,
            result,
            error,
        ),
        Err(message) => {
            shared.log.write(
                "error",
                "outcome.journal_failed",
                &format!("state lock lost: {message}"),
            );
            return err_response(
                &request.id,
                "state.unavailable",
                format!(
                    "the mutation effect completed but its outcome could not be journaled \
                     (fail closed): {message}"
                ),
            );
        }
    };
    publish_after_state_change(shared, None);
    response
}

/// Resolve a claim on an ALREADY-LOCKED state handle (no locking, no
/// publishing). Used by the restore path, which holds the state mutex
/// across the rename/reopen/swap sequence and publishes only after the
/// guard is dropped.
#[allow(clippy::too_many_arguments)]
fn resolve_mutation_on(
    state: &State,
    log: &DaemonLog,
    request: &Request,
    key: &str,
    method: &str,
    success: bool,
    result: Val,
    error: Option<(&'static str, String)>,
) -> String {
    let (status, response, outcome) = if success {
        let response = ok_response(&request.id, result);
        (
            "spent",
            response.clone(),
            daemon_outcome(key, "succeeded", null()),
        )
    } else {
        let (code, message) = error.unwrap_or(("state.unavailable", "effect failed".to_string()));
        let response = err_response(&request.id, code, &message);
        (
            "spent",
            response.clone(),
            daemon_outcome(key, "failed", error_val(code, &message)),
        )
    };
    match state.resolve_claim(
        key,
        method,
        status,
        &canonical_text(&outcome),
        Some(&response),
    ) {
        Ok(_audit) => response,
        Err(err) => {
            log.write(
                "error",
                "outcome.journal_failed",
                &format!("{}: {}", err.code, err.message),
            );
            err_response(
                &request.id,
                err.code,
                format!(
                    "the mutation effect completed but its outcome could not be journaled \
                     (fail closed): {}",
                    err.message
                ),
            )
        }
    }
}

/// Return a recorded replay response (the recorded line is canonical and was
/// schema-validated when stored).
fn replay(shared: &Arc<Shared>, response: &str) -> String {
    shared
        .log
        .write("info", "request.replay", "recorded response returned");
    response.to_string()
}

/// A typed hf-outcome/v1 document for daemon-owned operations.
fn daemon_outcome(key: &str, status: &str, error: Val) -> Val {
    let failed = status == "failed" || status == "refused";
    object(vec![
        ("schema", string("hf-outcome/v1")),
        ("plan_id", string(DAEMON_PLAN_ID)),
        ("step_id", string(DAEMON_STEP_ID)),
        ("status", string(status)),
        ("idempotency_key", string(key)),
        ("observed_at", string(&time::rfc3339_now())),
        ("result", if failed { null() } else { object(vec![]) }),
        ("error", if failed { error } else { null() }),
    ])
}

fn error_val(code: &str, message: &str) -> Val {
    object(vec![
        ("code", string(code)),
        ("message", string(message)),
        ("retryable", bool_(false)),
    ])
}

// ---------------------------------------------------------------------------
// Event fan-out (AC7: ordering, replay, snapshot, bounded backpressure)
// ---------------------------------------------------------------------------

/// Publish every journal event committed since the last fan-out. Runs after
/// the state guard is dropped (hub-lock ordering rule above); also refreshes
/// the bounded events mirror file when anything was published.
fn publish_events(shared: &Arc<Shared>) {
    let mut hub = match shared.hub.lock() {
        Ok(hub) => hub,
        Err(_) => return,
    };
    let state = match shared.lock_state() {
        Ok(state) => state,
        Err(_) => return,
    };
    let events = match state.events_after(hub.last_published, REPLAY_MAX_LINES) {
        Ok(events) => events,
        Err(_) => return,
    };
    let mut last = hub.last_published;
    for event in &events {
        let seq = event.get("seq").and_then(as_non_negative);
        if let Some(seq) = seq {
            if seq <= last {
                continue;
            }
            last = last.max(seq);
            hub.publish(&canonical_text(event));
        }
    }
    hub.last_published = last;
    drop(hub);
    drop(state);
    if last > 0
        && let Ok(state) = shared.lock_state()
        && let Err(err) = state.rebuild_events_mirror(&shared.paths.events_mirror_path)
    {
        shared.log.write(
            "warn",
            "events.mirror.write_failed",
            &format!("{}: {}", err.code, err.message),
        );
    }
}

fn subscribe_request(_shared: &Arc<Shared>, request: &Request) -> HandleOutcome {
    let cursor = request
        .params
        .as_ref()
        .and_then(|params| params.get("cursor"))
        .and_then(as_non_negative);
    let response = ok_response(
        &request.id,
        object(vec![
            ("event_stream", bool_(true)),
            ("schema", string("hf-event/v1")),
            ("cursor", cursor.map(integer).unwrap_or_else(null)),
        ]),
    );
    HandleOutcome::Subscribed { response, cursor }
}

/// Event-stream mode after the subscribe response: snapshot/replay first
/// (computed under the hub lock while registering), then live events from
/// the bounded queue. The client socket becomes push-only.
fn serve_event_stream(
    shared: &Arc<Shared>,
    writer: &mut UnixStream,
    cursor: Option<i64>,
) -> Result<(), String> {
    let (receiver, snapshot_lines, replay_lines) = {
        let (sender, receiver) = sync_channel::<String>(SUBSCRIBER_QUEUE_CAP);
        let mut hub = shared.hub.lock().map_err(|_| "hub poisoned".to_string())?;
        let (snapshot, replay) = compute_replay(shared, cursor)?;
        hub.subscribers.push(Subscriber { sender });
        (receiver, snapshot, replay)
    };
    for line in snapshot_lines.iter().chain(replay_lines.iter()) {
        writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.write_all(b"\n"))
            .and_then(|()| writer.flush())
            .map_err(|err| format!("write event: {err}"))?;
    }
    for line in receiver {
        writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.write_all(b"\n"))
            .and_then(|()| writer.flush())
            .map_err(|err| format!("write event: {err}"))?;
    }
    Ok(())
}

/// Compute (snapshot lines, replay lines) for a subscriber cursor. Must be
/// called with the hub lock held so no event can fall between the replay
/// query and the registration (either it lands in the replay list or it is
/// queued after registration — never both, never neither).
fn compute_replay(
    shared: &Arc<Shared>,
    cursor: Option<i64>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let state = shared.lock_state()?;
    let (max_seq, min_seq) = state
        .event_bounds()
        .map_err(|err| format!("{}: {}", err.code, err.message))?;
    let need_snapshot = match (cursor, max_seq) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(n), Some(max)) => {
            n > max
                || match min_seq {
                    Some(min) => n + 1 < min,
                    None => true,
                }
        }
    };
    let mut snapshot = Vec::new();
    let mut replay = Vec::new();
    if need_snapshot {
        let doc = state
            .snapshot_event()
            .map_err(|err| format!("{}: {}", err.code, err.message))?;
        snapshot.push(canonical_text(&doc));
    } else if let Some(n) = cursor {
        let events = state
            .events_after(n, REPLAY_MAX_LINES)
            .map_err(|err| format!("{}: {}", err.code, err.message))?;
        for event in events {
            replay.push(canonical_text(&event));
        }
    }
    Ok((snapshot, replay))
}

// ---------------------------------------------------------------------------
// Restart reconciliation (AC4: restart reconciles before retry)
// ---------------------------------------------------------------------------

/// Mark every claim left `claimed` by an interrupted run as ambiguous with a
/// typed outcome and a `reconcile.*` journal record. Returns the count.
fn reconcile_claims(state: &State, log: &DaemonLog) -> Result<usize, DaemonError> {
    let pending = state.claims_in_flight()?;
    let mut reconciled = 0usize;
    for claim in pending {
        let outcome = daemon_outcome(
            &claim.key,
            "ambiguous",
            error_val(
                "state.interrupted",
                "the operation was interrupted before its outcome was journaled; \
                 restart reconciliation marks it ambiguous — external review is required \
                 before retrying with a new key",
            ),
        );
        match state.journal_reconcile(
            &claim.key,
            &claim.method,
            "ambiguous",
            &canonical_text(&outcome),
        ) {
            Ok(_) => {
                reconciled += 1;
                log.write(
                    "warn",
                    "reconcile.ambiguous",
                    &format!(
                        "claim {} (request {}) marked ambiguous",
                        claim.key, claim.request_id
                    ),
                );
            }
            Err(err) => {
                return Err(daemon_error(
                    "daemon.reconcile",
                    format!("{}: {}", err.code, err.message),
                ));
            }
        }
    }
    Ok(reconciled)
}

// ---------------------------------------------------------------------------
// Crash-point injection (debug builds only; release ignores the env var)
// ---------------------------------------------------------------------------

/// Abort the daemon at a named journal boundary. Honored only when
/// `cfg!(debug_assertions)` — release binaries never crash from this hook.
fn crash_point(point: &str) {
    if !cfg!(debug_assertions) {
        return;
    }
    if std::env::var("HERDR_FLEET_CRASH_POINT").as_deref() == Ok(point) {
        eprintln!("herdr-fleet: crash point {point:?} reached (debug-only test hook)");
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_summary_truncates_at_char_boundary() {
        assert_eq!(bounded("hello", 10), "hello");
        let text = "日本語の長いメッセージです";
        let cut = bounded(text, 10);
        assert!(cut.ends_with('…'));
        assert!(cut.len() <= 10 + "…".len(), "cap plus the ellipsis");
        assert!(text.starts_with(&cut[..cut.len() - "…".len()]));
        assert_eq!(bounded(text, 1000), text);
    }

    #[test]
    fn refusal_codes_are_stable() {
        assert_eq!(refusal_code(Refusal::Parse), "refusal.parse");
        assert_eq!(refusal_code(Refusal::Schema), "refusal.schema");
        assert_eq!(refusal_code(Refusal::Version), "refusal.version");
        assert_eq!(refusal_code(Refusal::Malformed), "refusal.malformed");
        assert_eq!(refusal_code(Refusal::Noncanonical), "refusal.noncanonical");
    }

    #[test]
    fn response_lines_validate_as_rpc_responses() {
        let line = ok_response("0123456789abcdef", object(vec![("ok", bool_(true))]));
        let parsed = Val::parse_json(&line).expect("parse ok response");
        assert!(validate_doc(Family::RpcResponse, &parsed).is_accepted());
        let line = err_response("0123456789abcdef", "refusal.test", "nope");
        let parsed = Val::parse_json(&line).expect("parse error response");
        assert!(validate_doc(Family::RpcResponse, &parsed).is_accepted());
    }

    #[test]
    fn daemon_outcome_docs_validate() {
        for (status, error) in [
            ("ambiguous", error_val("state.interrupted", "msg")),
            ("failed", error_val("refusal.effect", "msg")),
            ("succeeded", null()),
        ] {
            let doc = daemon_outcome("ik_outcome-00000001", status, error);
            let verdict = validate_doc(Family::Outcome, &doc);
            assert!(verdict.is_accepted(), "{status}: {}", verdict.message());
        }
    }

    #[test]
    fn fallback_request_id_is_schema_valid() {
        assert!(crate::formats::is_request_id(FALLBACK_REQUEST_ID));
    }

    #[test]
    fn event_queue_cap_is_bounded() {
        assert!((1..=256).contains(&SUBSCRIBER_QUEUE_CAP));
    }
}
