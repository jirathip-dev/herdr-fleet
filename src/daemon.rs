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
        let id = doc
            .get("id")
            .and_then(Val::as_str)
            .unwrap_or(FALLBACK_REQUEST_ID);
        let class = verdict
            .refusal()
            .map(|refusal| refusal_code(refusal).to_string())
            .unwrap_or_else(|| "refusal.malformed".to_string());
        shared.log.write(
            "warn",
            "request.refused",
            &format!("{class}: {}", verdict.message()),
        );
        return HandleOutcome::Response(response_line(id, false, None, &class, verdict.message()));
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
        "grants.list" => method_grants_list(shared, request),
        "journal.tail" => method_journal_tail(shared, request),
        "grants.revoke" => method_grants_revoke(shared, request),
        "backup.create" => method_backup_create(shared, request),
        "restore.begin" => method_restore_begin(shared, request),
        "plan" => err_response(
            &request.id,
            "refusal.plan.unavailable",
            "plan execution arrives with the workflow-engine slice; plans render with the read-only CLI",
        ),
        "apply" => err_response(
            &request.id,
            "refusal.effect.unregistered",
            "no plan-step effect handlers are registered in this slice; apply cannot dispatch a plan yet",
        ),
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
    let outcome = match shared.lock_state() {
        Ok(state) => state.journal_intent(
            action,
            target,
            &key,
            &request.id,
            &request.method,
            None,
            None,
            &request.line,
        ),
        Err(message) => {
            return Intent::Refused {
                code: "state.unavailable",
                message,
            };
        }
    };
    match outcome {
        Ok((ClaimAttempt::Claimed, audit)) => {
            publish_after_state_change(shared, audit);
            Intent::Claimed { key }
        }
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
            let state = match shared.lock_state() {
                Ok(state) => state,
                Err(message) => {
                    return err_response(&request.id, "state.unavailable", message);
                }
            };
            match state.revoke_grant(grant_id, &time::rfc3339_now()) {
                Ok(()) => finish_mutation(
                    shared,
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
                Err(err) => finish_mutation(
                    shared,
                    request,
                    &key,
                    "grants.revoke",
                    false,
                    null(),
                    Some((err.code, err.message)),
                ),
            }
        }
        Intent::Replay { response } => replay(shared, &response),
        Intent::Refused { code, message } => err_response(&request.id, code, message),
    }
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
    // A restore touches the whole database; refuse while interrupted claims
    // are pending (they must reconcile through a restart first, AC4/AC6).
    match shared.lock_state() {
        Ok(state) => match state.claims_in_flight() {
            Ok(claims) if claims.is_empty() => {}
            Ok(_) => {
                return err_response(
                    &request.id,
                    "refusal.restore.pending_claims",
                    "restore refused while interrupted claims are pending; restart the daemon to reconcile them first",
                );
            }
            Err(err) => return err_response(&request.id, err.code, err.message),
        },
        Err(message) => return err_response(&request.id, "state.unavailable", message),
    }
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
    match journal_mutation(
        shared,
        request,
        "mutate.restore.begin",
        &format!("restore:{}", manifest.snapshot_name),
    ) {
        Intent::Claimed { key } => {
            crash_point("restore.after-intent");
            if let Err(err) = backup::restore_snapshot(&snapshot_path, &shared.paths.db_path) {
                return finish_mutation(
                    shared,
                    request,
                    &key,
                    "restore.begin",
                    false,
                    null(),
                    Some((err.code, err.message)),
                );
            }
            crash_point("restore.after-rename");
            // The live DB file was replaced under the old connection: reopen
            // the restored database and swap the state handle so no writer
            // can journal into the unlinked old file. While the outer state
            // mutex is held, no other handler can touch the old handle.
            let reopened =
                match State::open(&shared.paths.db_path, crate::state::Retention::default()) {
                    Ok(state) => state,
                    Err(err) => {
                        return finish_mutation(
                            shared,
                            request,
                            &key,
                            "restore.begin",
                            false,
                            null(),
                            Some((err.code, err.message)),
                        );
                    }
                };
            let rotated_epoch = reopened
                .rotate_epoch("restore")
                .and_then(|epoch| reopened.invalidate_grants_below_current().map(|_| epoch))
                .and_then(|epoch| {
                    // The snapshot predates the resolutions of any claims
                    // that were in flight when it was taken; void them so a
                    // resurrected claim can never dispatch or replay.
                    reopened.void_in_flight_claims("restore").map(|_| epoch)
                });
            match rotated_epoch {
                Ok(epoch) => {
                    // Swap: after this point every handler uses the restored
                    // database (the handle above validated it and rotated the
                    // epoch on it). While the outer state mutex is held here
                    // no other handler can journal into the unlinked old file.
                    let mut guard = match shared.state.lock() {
                        Ok(guard) => guard,
                        Err(message) => {
                            return finish_mutation(
                                shared,
                                request,
                                &key,
                                "restore.begin",
                                false,
                                null(),
                                Some(("state.unavailable", message.to_string())),
                            );
                        }
                    };
                    *guard = reopened;
                    drop(guard);
                    // The pre-effect claim was journaled into the *old* DB,
                    // which the rename replaced. Journal the intent again in
                    // the restored DB so the outcome resolves durably here
                    // (a crash between the rename and this re-journal simply
                    // re-executes the same restore on retry — idempotent).
                    let outcome = object(vec![
                        ("restored_epoch", integer(epoch)),
                        ("snapshot", string(&manifest.snapshot_name)),
                        ("prior_epoch", integer(epoch - 1)),
                    ]);
                    match journal_mutation(
                        shared,
                        request,
                        "mutate.restore.begin",
                        &format!("restore:{}", manifest.snapshot_name),
                    ) {
                        Intent::Claimed { key } => finish_mutation(
                            shared,
                            request,
                            &key,
                            "restore.begin",
                            true,
                            outcome,
                            None,
                        ),
                        Intent::Replay { response } => replay(shared, &response),
                        Intent::Refused { code, message } => {
                            err_response(&request.id, code, message)
                        }
                    }
                }
                Err(err) => finish_mutation(
                    shared,
                    request,
                    &key,
                    "restore.begin",
                    false,
                    null(),
                    Some((err.code, err.message)),
                ),
            }
        }
        Intent::Replay { response } => replay(shared, &response),
        Intent::Refused { code, message } => err_response(&request.id, code, message),
    }
}

/// Finish a journaled mutation: resolve the claim with a typed outcome and
/// the recorded response in one transaction, publish the outcome event, and
/// refresh the bounded event mirror. Returns the response line.
fn finish_mutation(
    shared: &Arc<Shared>,
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
    let audit = match shared.lock_state() {
        Ok(state) => state.resolve_claim(
            key,
            method,
            status,
            &canonical_text(&outcome),
            Some(&response),
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
    match audit {
        Ok(audit) => {
            publish_after_state_change(shared, Some(audit));
            response
        }
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
    let events = match state.events_after(hub.last_published, 8192) {
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
        .and_then(|params| params.get("last_seq"))
        .and_then(as_non_negative);
    let response = ok_response(
        &request.id,
        object(vec![
            ("event_stream", bool_(true)),
            ("schema", string("hf-event/v1")),
            ("last_seq", cursor.map(integer).unwrap_or_else(null)),
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
            .events_after(n, 8192)
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
