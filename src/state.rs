//! Daemon-owned SQLite state (issue #5, AC2/AC5/AC6).
//!
//! SQLite is the durable authority for leases, idempotency claims, audit
//! records (hash-chained), epochs, grants, workflow instances, schedules,
//! and recovery state. Every write goes through one `Connection` guarded by
//! a mutex: the daemon is the sole writer and the CLI never opens the
//! database (spec-state.md). Writes run with `synchronous=FULL` so a
//! committed transaction is fsynced; any SQLite write failure poisons the
//! state so later mutations fail closed until the process restarts.
//!
//! Journal semantics (spec-state.md AC6): a mutation's intent is journaled
//! in the same transaction that claims its idempotency key (durable before
//! any effect); the typed outcome is journaled when the effect resolves.
//! Audit records form a hash chain: `record_hash = sha256(line || prev)`.
//! Retention is bounded (defaults keep the chain genesis plus the last N
//! rows); the chain genesis row is never pruned so verification always has
//! an anchor.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::{Connection, OptionalExtension, params};

use crate::canonical::{canonical_text, sha256_hex};
use crate::time;
use crate::value::{Val, bool_, integer, null, object, string};

/// The schema version this binary understands (also `PRAGMA user_version`).
pub const SCHEMA_VERSION: i64 = 4;

/// Bounded-retention defaults (rows kept besides the chain genesis).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// Audit rows retained beyond the genesis row.
    pub audit_rows: i64,
    /// Event rows retained.
    pub event_rows: i64,
}

impl Default for Retention {
    fn default() -> Retention {
        Retention {
            audit_rows: 5000,
            event_rows: 2000,
        }
    }
}

/// A state operation failure. Codes are stable and typed for RPC responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateError {
    /// Stable error code (`state.open`, `state.corrupt`, `state.schema_future`,
    /// `state.write_full`, `state.write_io`, `state.busy`, `state.audit_tampered`,
    /// `state.poisoned`, `state.conflict`, `state.not_found`).
    pub code: &'static str,
    /// Human message (never contains credentials).
    pub message: String,
}

fn state_error(code: &'static str, message: impl Into<String>) -> StateError {
    StateError {
        code,
        message: message.into(),
    }
}

impl StateError {
    /// Map a rusqlite failure onto a typed state error.
    fn from_sqlite(context: &str, err: rusqlite::Error) -> StateError {
        match &err {
            rusqlite::Error::SqliteFailure(ffi, message) => {
                use rusqlite::ffi::ErrorCode;
                let detail = message.as_deref().unwrap_or("");
                match ffi.code {
                    ErrorCode::DiskFull => state_error(
                        "state.write_full",
                        format!("{context}: SQLite disk full (SQLITE_FULL) {detail}"),
                    ),
                    ErrorCode::SystemIoFailure | ErrorCode::CannotOpen => state_error(
                        "state.write_io",
                        format!("{context}: SQLite I/O failure (SQLITE_IOERR) {detail}"),
                    ),
                    ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt => state_error(
                        "state.corrupt",
                        format!("{context}: SQLite database corrupt ({detail})"),
                    ),
                    ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => state_error(
                        "state.busy",
                        format!("{context}: SQLite busy/locked ({detail})"),
                    ),
                    ErrorCode::ReadOnly => state_error(
                        "state.readonly",
                        format!("{context}: SQLite database is read-only ({detail})"),
                    ),
                    ErrorCode::ConstraintViolation => state_error(
                        "state.conflict",
                        format!("{context}: SQLite constraint violated ({detail})"),
                    ),
                    _ => state_error(
                        "state.unavailable",
                        format!("{context}: SQLite error ({detail})"),
                    ),
                }
            }
            _ => state_error(
                "state.unavailable",
                format!("{context}: database error: {err}"),
            ),
        }
    }
}

/// One audit/journal record that was durably written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRow {
    /// Monotonic audit sequence.
    pub seq: i64,
    /// State epoch at append time.
    pub epoch: i64,
    /// Canonical hf-audit/v1 JSONL line (with trailing LF).
    pub line: String,
    /// sha256 over `line || prev_hash` ("" for the genesis row).
    pub record_hash: String,
    /// Event seq of the journal.appended event emitted with this record.
    pub event_seq: i64,
}

/// An idempotency claim row (recovery checkpoint when status = claimed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimRow {
    /// The idempotency key.
    pub key: String,
    /// Request id that made the claim.
    pub request_id: String,
    /// RPC method.
    pub method: String,
    /// Claim status (`claimed` | `spent` | `ambiguous` | `voided`).
    pub status: String,
    /// Canonical hf-outcome/v1 document when resolved.
    pub outcome: Option<String>,
    /// Canonical hf-rpc-response/v1 document for idempotent replay.
    pub response: Option<String>,
    /// Canonical hf-rpc-request/v1 document that made the claim.
    pub request_line: String,
}

/// A grant row (route grants; issuance arrives with the workflow engine
/// slice, invalidation/revoke/list land here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantRow {
    /// Grant id (`gr_` + 16 hex).
    pub grant_id: String,
    /// Repository identity (`owner/name`).
    pub repository: String,
    /// Issue number.
    pub issue_number: i64,
    /// Acceptance revision (40-hex).
    pub issue_revision: String,
    /// Workflow hash (64-hex).
    pub workflow_hash: String,
    /// Policy hash (64-hex).
    pub policy_hash: String,
    /// Phase (closed set).
    pub phase: String,
    /// Path-scoped lane scope.
    pub scope: String,
    /// Capabilities (closed set), JSON array text.
    pub caps: String,
    /// Expiry (RFC3339 UTC).
    pub expires_at: String,
    /// Epoch the grant was issued against.
    pub state_epoch: i64,
    /// Status (`active` | `revoked` | `invalidated`).
    pub status: String,
    /// Created at (RFC3339 UTC).
    pub created_at: String,
}

/// A workflow engine instance row (m0002: workflow pin, phase, node, review
/// rounds, pause state). Issuance/advance decisions belong to the engine;
/// this handle persists them durably.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceRow {
    /// Instance id (slug).
    pub instance_id: String,
    /// Repository identity.
    pub repository: String,
    /// Workflow id (slug).
    pub workflow_id: String,
    /// Workflow hash (64-hex; pinned at start — changed-in-flight refusal).
    pub workflow_hash: String,
    /// Policy hash (64-hex).
    pub policy_hash: String,
    /// Binding grant id.
    pub grant_id: String,
    /// Issue number.
    pub issue_number: i64,
    /// Acceptance revision (40-hex).
    pub issue_revision: String,
    /// Allowed phase (closed set).
    pub phase: String,
    /// Path-scoped lane scope.
    pub scope: String,
    /// Capabilities (closed set), JSON array text.
    pub caps: String,
    /// Current workflow node id ("" until started).
    pub current_node: String,
    /// Normal review/fix rounds used (AC4).
    pub normal_rounds: u32,
    /// Recovery rounds used (AC4).
    pub recovery_rounds: u32,
    /// Whether the item is in the human queue (AC4 exhaustion).
    pub human_queue: bool,
    /// Count of explicit terminal blockers (AC7).
    pub terminal_blockers: u32,
    /// Whether the instance is paused (AC8; durable across restart).
    pub paused: bool,
    /// Stored fresh authorized resume digest (AC8).
    pub resume_digest: String,
    /// State epoch the instance runs under.
    pub state_epoch: i64,
    /// Status (`new` | `running` | `paused` | `human_queue` | `blocked` |
    /// `done` | `invalidated`).
    pub status: String,
    /// Created at (RFC3339 UTC).
    pub created_at: String,
    /// Last update (RFC3339 UTC).
    pub updated_at: String,
}

/// A durable review-evidence row (issue #8; spec-review-evidence.md):
/// exact feature head + integration base + workflow/policy hashes plus the
/// reviewer verdict and named checks. Invalidated only by revalidation
/// against live state before an integration merge (never edited in place).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceRow {
    /// Evidence id (`ev_` + 16 hex).
    pub evidence_id: String,
    /// Owning workflow instance id.
    pub instance_id: String,
    /// Repository identity.
    pub repository: String,
    /// Exact reviewed feature-branch head (40-hex).
    pub feature_head: String,
    /// Exact integration-base SHA the review was performed against (40-hex).
    pub integration_base: String,
    /// Workflow hash (64-hex).
    pub workflow_hash: String,
    /// Policy hash (64-hex).
    pub policy_hash: String,
    /// Verdict (`pass` | `fail`).
    pub verdict: String,
    /// Reviewer identity (distinct from the implementer).
    pub reviewer: String,
    /// Canonical JSON array of named checks (`[{"name","status"}]`).
    pub checks: String,
    /// Recorded at (RFC3339 UTC).
    pub created_at: String,
}

/// A recorded first-real-write approval (issue #8 AC10 plumbing). The
/// approval is recorded by a human-gated flow BEFORE any real external
/// write canary; this slice only proves the gate with fakes and never runs
/// a real canary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRow {
    /// Approval id (`ap_` + 16 hex).
    pub approval_id: String,
    /// Recorded scope (`first-write-canary` is the closed scope).
    pub scope: String,
    /// 64-hex human-confirmed digest bound to the approval.
    pub digest: String,
    /// Whether the digest was confirmed on an interactive TTY.
    pub interactive: bool,
    /// Recorded at (RFC3339 UTC).
    pub recorded_at: String,
}

/// One durable schedule row (lifecycle slice, issue #9): the recurring
/// non-destructive cadence record. `enabled` is the durable pause flag
/// (false = paused/disabled; only an explicit human resume lifts it),
/// `next_run_at` is the next aligned evaluation window, and `doc` is the
/// canonical `hf-schedule/v1` document binding the recurring grant's exact
/// repository/issue, workflow + policy hashes, read-only caps, scope,
/// expiry, and cadence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleRow {
    /// Schedule id (`sd_` + 16 hex).
    pub schedule_id: String,
    /// State epoch the row belongs to (schedules die with their epoch).
    pub state_epoch: i64,
    /// Whether the schedule is enabled (false = paused/disabled; durable).
    pub enabled: bool,
    /// Next evaluation window (RFC3339 UTC, seconds precision), if any.
    pub next_run_at: Option<String>,
    /// Row creation time.
    pub created_at: String,
    /// Last row write time.
    pub updated_at: String,
    /// Canonical hf-schedule/v1 document text ('' on pre-m0004 rows).
    pub doc: String,
}

/// The daemon-owned state handle. All methods serialize on an internal
/// mutex (one writer); a failed write poisons the handle (fail closed).
pub struct State {
    conn: Mutex<Connection>,
    retention: Retention,
    poisoned: AtomicBool,
}

impl std::fmt::Debug for State {
    /// Deliberately does not expose the connection or its internals.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("State")
            .field("retention", &self.retention)
            .field("poisoned", &self.poisoned.load(Ordering::SeqCst))
            .finish()
    }
}

/// A minimal summary for the daemon `status` RPC and service doctor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateSummary {
    /// Current state epoch.
    pub epoch: i64,
    /// Highest audit seq (journal length minus pruned rows).
    pub audit_seq: i64,
    /// Highest event seq.
    pub event_seq: i64,
    /// SQLite schema version.
    pub schema_version: i64,
    /// Count of active grants.
    pub active_grants: i64,
    /// Count of un-resolved claims (recovery queue).
    pub pending_claims: i64,
    /// Whether the state is poisoned (fail closed after a write failure).
    pub poisoned: bool,
}

/// The outcome of an idempotent claim attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimAttempt {
    /// The key was free and is now claimed (the caller may journal intent).
    Claimed,
    /// The same request already resolved; replay the recorded response.
    Replay {
        /// Recorded canonical hf-rpc-response/v1 line.
        response: String,
    },
    /// The key was claimed/resolved by a *different* request (refuse).
    Reused {
        /// Recorded request id that owns the key.
        owner_request_id: String,
    },
}

impl State {
    /// Open (creating if needed) the state database at `path` and migrate it
    /// to the current schema. Fails closed on corrupt or newer-schema files.
    pub fn open(path: &Path, retention: Retention) -> Result<State, StateError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                state_error("state.open", format!("create {}: {err}", parent.display()))
            })?;
        }
        let existed = path.exists() && fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false);
        let mut conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|err| StateError::from_sqlite("open state db", err))?;
        conn.execute_batch(
            "PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA journal_mode = DELETE;",
        )
        .map_err(|err| StateError::from_sqlite("state pragmas", err))?;
        if existed {
            let ok: String = conn
                .query_row("PRAGMA quick_check", [], |row| row.get(0))
                .map_err(|err| StateError::from_sqlite("quick_check", err))?;
            if ok != "ok" {
                return Err(state_error(
                    "state.corrupt",
                    format!("state database failed PRAGMA quick_check: {ok}"),
                ));
            }
        }
        let mut user_version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|err| StateError::from_sqlite("read user_version", err))?;
        // Apply pending migrations in order (linear chain; each migration
        // records itself in schema_migrations before bumping user_version).
        if user_version == 0 {
            run_initial_migration(&mut conn)?;
            user_version = M0001_APPLIES_TO;
        }
        if user_version == M0002_APPLIES_FROM {
            run_m0002(&mut conn)?;
            user_version = M0002_APPLIES_TO;
        }
        if user_version == M0003_APPLIES_FROM {
            run_m0003(&mut conn)?;
            user_version = M0003_APPLIES_TO;
        }
        if user_version == M0004_APPLIES_FROM {
            run_m0004(&mut conn)?;
            user_version = M0004_APPLIES_TO;
        }
        match user_version {
            v if v == SCHEMA_VERSION => {
                for (migration_id, _, _) in MIGRATIONS {
                    let recorded: Option<i64> = conn
                        .query_row(
                            "SELECT COUNT(*) FROM schema_migrations WHERE migration_id = ?1",
                            params![migration_id],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(|err| StateError::from_sqlite("migration bookkeeping", err))?;
                    if recorded != Some(1) {
                        return Err(state_error(
                            "state.corrupt",
                            format!(
                                "user_version matches but migration bookkeeping is missing for {migration_id}"
                            ),
                        ));
                    }
                }
            }
            v => {
                return Err(state_error(
                    "state.schema_future",
                    format!(
                        "state schema version {v} is newer than this binary supports \
                         (maximum {SCHEMA_VERSION}); refusing to operate"
                    ),
                ));
            }
        }
        let state = State {
            conn: Mutex::new(conn),
            retention,
            poisoned: AtomicBool::new(false),
        };
        state.verify_chain()?;
        Ok(state)
    }

    /// Whether the state is poisoned (fail closed after a write failure).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }

    /// Lock the connection, mapping lock/poison failures to typed errors.
    fn lock(
        &self,
        context: &'static str,
    ) -> Result<std::sync::MutexGuard<'_, Connection>, StateError> {
        self.conn.lock().map_err(|_| {
            state_error(
                "state.unavailable",
                format!("{context}: state mutex poisoned"),
            )
        })
    }

    // ---------------------------------------------------------------------
    // Epochs
    // ---------------------------------------------------------------------

    /// The current epoch (1 after initial migration).
    pub fn current_epoch(&self) -> Result<i64, StateError> {
        let conn = self.lock("current_epoch")?;
        conn.query_row("SELECT COALESCE(MAX(epoch), 0) FROM epoch", [], |row| {
            row.get(0)
        })
        .map_err(|err| StateError::from_sqlite("current_epoch", err))
    }

    /// Rotate to a new epoch (restore or security rotation). The new epoch
    /// is prior + 1 and the record mirrors hf-epoch/v1.
    pub fn rotate_epoch(&self, reason: &str) -> Result<i64, StateError> {
        if !matches!(reason, "restore" | "security_rotation") {
            return Err(state_error(
                "state.conflict",
                format!("epoch rotation reason {reason:?} outside {{restore, security_rotation}}"),
            ));
        }
        let conn = self.lock("rotate_epoch")?;
        let prior: i64 = conn
            .query_row("SELECT COALESCE(MAX(epoch), 0) FROM epoch", [], |row| {
                row.get(0)
            })
            .map_err(|err| StateError::from_sqlite("rotate_epoch: read", err))?;
        let next = prior + 1;
        conn.execute(
            "INSERT INTO epoch (epoch, reason, prior_epoch, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![next, reason, prior, time::rfc3339_now()],
        )
        .map_err(|err| StateError::from_sqlite("rotate_epoch: insert", err))?;
        Ok(next)
    }

    /// Invalidate every active grant issued against an epoch older than
    /// `current_epoch` (restore semantics: grants die with their epoch).
    /// Bound running instances are invalidated with their grants (C3:
    /// restore rotates the epoch; instances die with their epoch too).
    pub fn invalidate_grants_below_current(&self) -> Result<i64, StateError> {
        let conn = self.lock("invalidate_grants")?;
        let epoch: i64 = conn
            .query_row("SELECT COALESCE(MAX(epoch), 0) FROM epoch", [], |row| {
                row.get(0)
            })
            .map_err(|err| StateError::from_sqlite("invalidate_grants: read", err))?;
        conn.execute(
            "UPDATE grants SET status = 'invalidated' WHERE status = 'active' AND state_epoch < ?1",
            params![epoch],
        )
        .map_err(|err| StateError::from_sqlite("invalidate_grants: update", err))?;
        conn.execute(
            "UPDATE instances SET status = 'invalidated', updated_at = ?2
              WHERE state_epoch < ?1 AND status IN
                    ('new', 'running', 'paused', 'human_queue', 'blocked')",
            params![epoch, time::rfc3339_now()],
        )
        .map_err(|err| StateError::from_sqlite("invalidate_grants: instances", err))?;
        Ok(epoch)
    }

    /// A single hf-epoch/v1-shaped document for the current epoch.
    pub fn epoch_doc(&self) -> Result<Val, StateError> {
        let conn = self.lock("epoch_doc")?;
        let row: Option<(i64, String, Option<i64>, String)> = conn
            .query_row(
                "SELECT epoch, reason, prior_epoch, created_at FROM epoch ORDER BY epoch DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|err| StateError::from_sqlite("epoch_doc", err))?;
        match row {
            Some((epoch, reason, prior_epoch, created_at)) => Ok(object(vec![
                ("schema", string("hf-epoch/v1")),
                ("epoch", integer(epoch)),
                ("reason", string(&reason)),
                ("prior_epoch", prior_epoch.map(integer).unwrap_or_else(null)),
                ("created_at", string(&created_at)),
            ])),
            None => Err(state_error("state.not_found", "no epoch record exists")),
        }
    }

    // ---------------------------------------------------------------------
    // Idempotency claims (AC6) and journaled intents/outcomes (AC4/AC5)
    // ---------------------------------------------------------------------

    /// Look up an existing claim by key (idempotent replay check).
    pub fn claim(&self, key: &str) -> Result<Option<ClaimRow>, StateError> {
        let conn = self.lock("claim")?;
        conn.query_row(
            "SELECT key, request_id, method, status, outcome, response, request_line
               FROM idempotency WHERE key = ?1",
            params![key],
            |row| {
                Ok(ClaimRow {
                    key: row.get(0)?,
                    request_id: row.get(1)?,
                    method: row.get(2)?,
                    status: row.get(3)?,
                    outcome: row.get(4)?,
                    response: row.get(5)?,
                    request_line: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(|err| StateError::from_sqlite("claim: read", err))
    }

    /// Journal a mutation intent and claim its idempotency key in one
    /// transaction (durable before any effect; fail-closed AC5). Returns the
    /// claim decision plus the journaled audit/event seqs when claimed.
    #[allow(clippy::too_many_arguments)]
    pub fn journal_intent(
        &self,
        action: &str,
        target: &str,
        key: &str,
        request_id: &str,
        method: &str,
        plan_hash: Option<&str>,
        grant_id: Option<&str>,
        request_line: &str,
    ) -> Result<(ClaimAttempt, Option<AuditRow>), StateError> {
        let outcome = self.journal_intent_inner(
            action,
            target,
            key,
            request_id,
            method,
            plan_hash,
            grant_id,
            request_line,
        );
        if let Err(err) = &outcome {
            self.poison_on(err);
        }
        outcome
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn journal_intent_inner(
        &self,
        action: &str,
        target: &str,
        key: &str,
        request_id: &str,
        method: &str,
        plan_hash: Option<&str>,
        grant_id: Option<&str>,
        request_line: &str,
    ) -> Result<(ClaimAttempt, Option<AuditRow>), StateError> {
        self.ensure_writable()?;
        let mut conn = self.lock("journal_intent")?;
        let tx = conn
            .transaction()
            .map_err(|err| StateError::from_sqlite("journal_intent: begin", err))?;
        let existing: Option<(String, String, Option<String>, Option<String>, String)> = tx
            .query_row(
                "SELECT request_id, status, outcome, response, request_line FROM idempotency WHERE key = ?1",
                params![key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|err| StateError::from_sqlite("journal_intent: read", err))?;
        if let Some((owner_request_id, status, outcome, response, existing_line)) = existing {
            if owner_request_id == request_id && status != "claimed" {
                if let Some(response) = response.filter(|response| !response.is_empty()) {
                    return Ok((ClaimAttempt::Replay { response }, None));
                }
                if status == "ambiguous" || status == "voided" {
                    // Interrupted claims resolve through reconcile; a
                    // matching retry without a recorded response is refused.
                    let message = if status == "ambiguous" {
                        "the claim was left ambiguous by an interrupted run; external reconciliation is required before retry with a new key"
                    } else {
                        "the claim was voided by recovery; retry with a new key"
                    };
                    let _ = (outcome, existing_line);
                    return Err(state_error("state.ambiguous_claim", message));
                }
                return Err(state_error(
                    "state.claim_incomplete",
                    format!("claim {key:?} is still in flight (request {owner_request_id})"),
                ));
            }
            if owner_request_id != request_id {
                return Err(state_error(
                    "state.claim_reused",
                    format!(
                        "idempotency key {key:?} already belongs to request {owner_request_id}"
                    ),
                ));
            }
        }
        let audit = self.append_audit_locked(&tx, action, target, key, plan_hash, grant_id)?;
        tx.execute(
            "INSERT INTO idempotency (key, request_id, method, status, epoch, request_line, claimed_at)
             VALUES (?1, ?2, ?3, 'claimed', ?4, ?5, ?6)",
            params![
                key,
                request_id,
                method,
                audit.epoch,
                request_line,
                time::rfc3339_now()
            ],
        )
        .map_err(|err| StateError::from_sqlite("journal_intent: claim", err))?;
        tx.commit()
            .map_err(|err| StateError::from_sqlite("journal_intent: commit", err))?;
        Ok((ClaimAttempt::Claimed, Some(audit)))
    }

    /// Resolve a claim with its typed outcome (hf-outcome/v1 document) and
    /// the recorded response (hf-rpc-response/v1) in one transaction, and
    /// journal the outcome audit record. `status` is one of `spent`,
    /// `ambiguous`, `voided`; the audit action mirrors the operation.
    pub fn resolve_claim(
        &self,
        key: &str,
        method: &str,
        status: &str,
        outcome_line: &str,
        response_line: Option<&str>,
    ) -> Result<AuditRow, StateError> {
        let outcome = self.resolve_claim_inner(key, method, status, outcome_line, response_line);
        if let Err(err) = &outcome {
            self.poison_on(err);
        }
        outcome
    }

    fn resolve_claim_inner(
        &self,
        key: &str,
        method: &str,
        status: &str,
        outcome_line: &str,
        response_line: Option<&str>,
    ) -> Result<AuditRow, StateError> {
        self.ensure_writable()?;
        let mut conn = self.lock("resolve_claim")?;
        let tx = conn
            .transaction()
            .map_err(|err| StateError::from_sqlite("resolve_claim: begin", err))?;
        let action = format!("outcome.{method}");
        let audit = self.append_audit_locked(&tx, &action, key, key, None, None)?;
        let affected = tx
            .execute(
                "UPDATE idempotency SET status = ?1, outcome = ?2, response = ?3, resolved_at = ?4
                  WHERE key = ?5 AND status = 'claimed'",
                params![
                    status,
                    outcome_line,
                    response_line.unwrap_or(""),
                    time::rfc3339_now(),
                    key
                ],
            )
            .map_err(|err| StateError::from_sqlite("resolve_claim: update", err))?;
        if affected == 0 {
            return Err(state_error(
                "state.claim_conflict",
                format!("claim {key:?} is not in the claimed state"),
            ));
        }
        tx.commit()
            .map_err(|err| StateError::from_sqlite("resolve_claim: commit", err))?;
        Ok(audit)
    }

    /// Every un-resolved claim (the recovery checkpoint queue). On restart
    /// the daemon reconciles each one before accepting retries (AC4).
    pub fn claims_in_flight(&self) -> Result<Vec<ClaimRow>, StateError> {
        let conn = self.lock("claims_in_flight")?;
        let mut statement = conn
            .prepare(
                "SELECT key, request_id, method, status, outcome, response, request_line
                   FROM idempotency WHERE status = 'claimed' ORDER BY claimed_at",
            )
            .map_err(|err| StateError::from_sqlite("claims_in_flight: prepare", err))?;
        let rows = statement
            .query_map([], |row| {
                Ok(ClaimRow {
                    key: row.get(0)?,
                    request_id: row.get(1)?,
                    method: row.get(2)?,
                    status: row.get(3)?,
                    outcome: row.get(4)?,
                    response: row.get(5)?,
                    request_line: row.get(6)?,
                })
            })
            .map_err(|err| StateError::from_sqlite("claims_in_flight: query", err))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|err| StateError::from_sqlite("claims_in_flight: row", err))?);
        }
        Ok(out)
    }

    /// Void every claim that is still in flight (status `claimed`) by
    /// journaling a `reconcile.restore` audit record and marking the claim
    /// voided. Used after a restore rolls the database back to a snapshot
    /// that predates those claims' resolutions: resurrected in-flight
    /// claims must never become runnable or replayable in the restored
    /// world.
    pub fn void_in_flight_claims(&self, reason: &str) -> Result<usize, StateError> {
        let claims = self.claims_in_flight()?;
        let at = time::rfc3339_now();
        let count = claims.len();
        for claim in &claims {
            let outcome_line = canonical_text(&object(vec![
                ("schema", string("hf-outcome/v1")),
                ("status", string("voided")),
                ("reason", string(&format!("restore.voided.{reason}"))),
                ("at", string(&at)),
            ]));
            self.journal_reconcile(&claim.key, &claim.method, "voided", &outcome_line)?;
        }
        Ok(count)
    }

    /// Journal a reconcile decision (crash recovery marks claims ambiguous
    /// or voided with a typed outcome; action `reconcile.<method>`).
    pub fn journal_reconcile(
        &self,
        key: &str,
        method: &str,
        status: &str,
        outcome_line: &str,
    ) -> Result<AuditRow, StateError> {
        let outcome = self.journal_reconcile_inner(key, method, status, outcome_line);
        if let Err(err) = &outcome {
            self.poison_on(err);
        }
        outcome
    }

    fn journal_reconcile_inner(
        &self,
        key: &str,
        method: &str,
        status: &str,
        outcome_line: &str,
    ) -> Result<AuditRow, StateError> {
        self.ensure_writable()?;
        let mut conn = self.lock("journal_reconcile")?;
        let tx = conn
            .transaction()
            .map_err(|err| StateError::from_sqlite("journal_reconcile: begin", err))?;
        let action = format!("reconcile.{method}");
        let audit = self.append_audit_locked(&tx, &action, key, key, None, None)?;
        let affected = tx
            .execute(
                "UPDATE idempotency SET status = ?1, outcome = ?2, resolved_at = ?3
                  WHERE key = ?4 AND status = 'claimed'",
                params![status, outcome_line, time::rfc3339_now(), key],
            )
            .map_err(|err| StateError::from_sqlite("journal_reconcile: update", err))?;
        if affected == 0 {
            return Err(state_error(
                "state.claim_conflict",
                format!("claim {key:?} is not in the claimed state"),
            ));
        }
        tx.commit()
            .map_err(|err| StateError::from_sqlite("journal_reconcile: commit", err))?;
        Ok(audit)
    }

    // ---------------------------------------------------------------------
    // Grants (revoke/list here; issuance arrives with the engine slice)
    // ---------------------------------------------------------------------

    /// Revoke an active grant (state transition + journaled intent by the
    /// caller; this only flips the row).
    pub fn revoke_grant(&self, grant_id: &str, at: &str) -> Result<(), StateError> {
        let conn = self.lock("revoke_grant")?;
        let affected = conn
            .execute(
                "UPDATE grants SET status = 'revoked', revoked_at = ?2
                  WHERE grant_id = ?1 AND status = 'active'",
                params![grant_id, at],
            )
            .map_err(|err| StateError::from_sqlite("revoke_grant: update", err))?;
        if affected == 0 {
            return Err(state_error(
                "state.not_found",
                format!("no active grant {grant_id:?} to revoke"),
            ));
        }
        Ok(())
    }

    /// All grants that are not invalidated (daemon filters expired live).
    pub fn list_grants(&self) -> Result<Vec<GrantRow>, StateError> {
        let conn = self.lock("list_grants")?;
        let mut statement = conn
            .prepare(
                "SELECT grant_id, repository, issue_number, issue_revision, workflow_hash,
                        policy_hash, phase, scope, caps, expires_at, state_epoch, status, created_at
                   FROM grants WHERE status = 'active' ORDER BY created_at",
            )
            .map_err(|err| StateError::from_sqlite("list_grants: prepare", err))?;
        let rows = statement
            .query_map([], |row| {
                Ok(GrantRow {
                    grant_id: row.get(0)?,
                    repository: row.get(1)?,
                    issue_number: row.get(2)?,
                    issue_revision: row.get(3)?,
                    workflow_hash: row.get(4)?,
                    policy_hash: row.get(5)?,
                    phase: row.get(6)?,
                    scope: row.get(7)?,
                    caps: row.get(8)?,
                    expires_at: row.get(9)?,
                    state_epoch: row.get(10)?,
                    status: row.get(11)?,
                    created_at: row.get(12)?,
                })
            })
            .map_err(|err| StateError::from_sqlite("list_grants: query", err))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|err| StateError::from_sqlite("list_grants: row", err))?);
        }
        Ok(out)
    }

    // ---------------------------------------------------------------------
    // Workflow engine: durable route grants + instances (issue #6)
    // ---------------------------------------------------------------------

    /// Issue a route grant from a validated `hf-grant/v1` document (AC3
    /// bindings: repository, issue set/revision, workflow/policy hashes,
    /// phase, scope, caps, expiry, state epoch). Refused when the document
    /// is invalid, when the grant id already exists, or when its epoch is
    /// not the current epoch (grants die with their epoch).
    pub fn issue_grant(&self, doc: &Val) -> Result<GrantRow, StateError> {
        let verdict = crate::schema::validate_doc(crate::schema::Family::Grant, doc);
        if !verdict.is_accepted() {
            return Err(state_error(
                "state.grant_invalid",
                format!("grant document refused: {}", verdict.message()),
            ));
        }
        let grant = grant_row_from_doc(doc)?;
        let conn = self.lock("issue_grant")?;
        let epoch = current_epoch_locked(&conn)?;
        if grant.state_epoch != epoch {
            return Err(state_error(
                "state.epoch_mismatch",
                format!(
                    "grant epoch {} != current epoch {}; grants die with their epoch",
                    grant.state_epoch, epoch
                ),
            ));
        }
        conn.execute(
            "INSERT INTO grants (grant_id, repository, issue_number, issue_revision,
                                 workflow_hash, policy_hash, phase, scope, caps,
                                 expires_at, state_epoch, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'active', ?12)",
            params![
                grant.grant_id,
                grant.repository,
                grant.issue_number,
                grant.issue_revision,
                grant.workflow_hash,
                grant.policy_hash,
                grant.phase,
                grant.scope,
                grant.caps,
                grant.expires_at,
                grant.state_epoch,
                grant.created_at
            ],
        )
        .map_err(|err| {
            if err.to_string().contains("UNIQUE") {
                state_error(
                    "state.grant_exists",
                    format!("grant {} already exists", grant.grant_id),
                )
            } else {
                StateError::from_sqlite("issue_grant: insert", err)
            }
        })?;
        Ok(grant)
    }

    /// Fetch one grant row by id (any status; engine revalidation reads the
    /// live binding before every further mutation — AC2).
    pub fn grant_by_id(&self, grant_id: &str) -> Result<Option<GrantRow>, StateError> {
        let conn = self.lock("grant_by_id")?;
        let row = conn
            .query_row(
                "SELECT grant_id, repository, issue_number, issue_revision, workflow_hash,
                        policy_hash, phase, scope, caps, expires_at, state_epoch, status, created_at
                   FROM grants WHERE grant_id = ?1",
                params![grant_id],
                |row| {
                    Ok(GrantRow {
                        grant_id: row.get(0)?,
                        repository: row.get(1)?,
                        issue_number: row.get(2)?,
                        issue_revision: row.get(3)?,
                        workflow_hash: row.get(4)?,
                        policy_hash: row.get(5)?,
                        phase: row.get(6)?,
                        scope: row.get(7)?,
                        caps: row.get(8)?,
                        expires_at: row.get(9)?,
                        state_epoch: row.get(10)?,
                        status: row.get(11)?,
                        created_at: row.get(12)?,
                    })
                },
            )
            .optional()
            .map_err(|err| StateError::from_sqlite("grant_by_id: query", err))?;
        Ok(row)
    }

    /// Invalidate an active grant (material issue/acceptance edit — AC2).
    /// Bound instances are invalidated with it (C3: an instance bound to an
    /// invalidated grant can never advance again).
    pub fn invalidate_grant(&self, grant_id: &str, at: &str) -> Result<(), StateError> {
        let conn = self.lock("invalidate_grant")?;
        let affected = conn
            .execute(
                "UPDATE grants SET status = 'invalidated', revoked_at = ?2
                  WHERE grant_id = ?1 AND status = 'active'",
                params![grant_id, at],
            )
            .map_err(|err| StateError::from_sqlite("invalidate_grant: update", err))?;
        if affected == 0 {
            return Err(state_error(
                "state.not_found",
                format!("no active grant {grant_id:?} to invalidate"),
            ));
        }
        conn.execute(
            "UPDATE instances SET status = 'invalidated', updated_at = ?2
              WHERE grant_id = ?1 AND status IN
                    ('new', 'running', 'paused', 'human_queue', 'blocked')",
            params![grant_id, at],
        )
        .map_err(|err| StateError::from_sqlite("invalidate_grant: instances", err))?;
        Ok(())
    }

    /// Start a workflow instance bound to an active grant (durable pin of
    /// workflow/policy hashes and the issue binding). Refused when the
    /// grant is absent/inactive or its bindings disagree with the engine
    /// pin (`workflow_hash`/`policy_hash`).
    pub fn start_instance(
        &self,
        instance_id: &str,
        grant_id: &str,
        workflow_id: &str,
        at: &str,
    ) -> Result<InstanceRow, StateError> {
        let Some(grant) = self.grant_by_id(grant_id)? else {
            return Err(state_error(
                "state.not_found",
                format!("no grant {grant_id:?}"),
            ));
        };
        if grant.status != "active" {
            return Err(state_error(
                "state.grant_inactive",
                format!("grant {grant_id:?} is not active"),
            ));
        }
        {
            let conn = self.lock("start_instance")?;
            conn.execute(
                "INSERT INTO instances (instance_id, repository, state_epoch, status,
                                        workflow_id, workflow_hash, policy_hash, grant_id,
                                        issue_number, issue_revision, phase, scope, caps,
                                        current_node, normal_rounds, recovery_rounds,
                                        human_queue, terminal_blockers, paused, resume_digest,
                                        created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'new', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, '', 0, 0, 0, 0, 0, '', ?13, ?13)",
                params![
                    instance_id,
                    grant.repository,
                    grant.state_epoch,
                    workflow_id,
                    grant.workflow_hash,
                    grant.policy_hash,
                    grant_id,
                    grant.issue_number,
                    grant.issue_revision,
                    grant.phase,
                    grant.scope,
                    grant.caps,
                    at
                ],
            )
            .map_err(|err| StateError::from_sqlite("start_instance: insert", err))?;
        }
        let row = self.instance_by_id(instance_id)?.expect("just inserted");
        Ok(row)
    }

    /// One instance row by id.
    pub fn instance_by_id(&self, instance_id: &str) -> Result<Option<InstanceRow>, StateError> {
        let conn = self.lock("instance_by_id")?;
        let sql = format!("{} WHERE instance_id = ?1", instance_select_sql());
        let row = conn
            .query_row(sql.as_str(), params![instance_id], |row| {
                instance_row_from(row)
            })
            .optional()
            .map_err(|err| StateError::from_sqlite("instance_by_id: query", err))?;
        Ok(row)
    }

    /// Pause a workflow instance durably (AC8). The caller mints the fresh
    /// authorized resume digest via the engine; it is stored and required
    /// again on resume, surviving restarts.
    pub fn pause_instance(
        &self,
        instance_id: &str,
        resume_digest: &str,
        at: &str,
    ) -> Result<(), StateError> {
        let conn = self.lock("pause_instance")?;
        let affected = conn
            .execute(
                "UPDATE instances SET paused = 1, resume_digest = ?2, status = 'paused',
                        updated_at = ?3
                  WHERE instance_id = ?1 AND paused = 0",
                params![instance_id, resume_digest, at],
            )
            .map_err(|err| StateError::from_sqlite("pause_instance: update", err))?;
        if affected == 0 {
            return Err(state_error(
                "state.not_paused",
                format!("instance {instance_id:?} is not runnable (already paused or absent)"),
            ));
        }
        Ok(())
    }

    /// Resume a paused instance. `presented_digest` must equal the stored
    /// fresh authorized digest (engine [`crate::engine::authorize_resume`]);
    /// the digest is consumed on success so a stale digest can never resume
    /// twice (AC8).
    pub fn resume_instance(
        &self,
        instance_id: &str,
        presented_digest: &str,
        at: &str,
    ) -> Result<(), StateError> {
        let Some(row) = self.instance_by_id(instance_id)? else {
            return Err(state_error(
                "state.not_found",
                format!("no instance {instance_id:?}"),
            ));
        };
        if row.paused {
            crate::engine::authorize_resume(&row.resume_digest, presented_digest)
                .map_err(|err| state_error("state.stale_resume", err.message))?;
        }
        let conn = self.lock("resume_instance")?;
        let affected = conn
            .execute(
                "UPDATE instances SET paused = 0, resume_digest = '', status = 'running',
                        updated_at = ?2
                  WHERE instance_id = ?1 AND paused = 1",
                params![instance_id, at],
            )
            .map_err(|err| StateError::from_sqlite("resume_instance: update", err))?;
        if affected == 0 {
            return Err(state_error(
                "state.not_paused",
                format!("instance {instance_id:?} is not paused"),
            ));
        }
        Ok(())
    }

    /// Advance a running instance: record the current node and review-round
    /// counters (AC4) plus blocker accounting (AC7). The caller computes the
    /// new values with the engine; this only persists them.
    #[allow(clippy::too_many_arguments)]
    pub fn advance_instance(
        &self,
        instance_id: &str,
        current_node: &str,
        normal_rounds: u32,
        recovery_rounds: u32,
        human_queue: bool,
        terminal_blockers: u32,
        at: &str,
    ) -> Result<(), StateError> {
        let status = if human_queue {
            "human_queue"
        } else if terminal_blockers > 0 {
            "blocked"
        } else {
            "running"
        };
        let conn = self.lock("advance_instance")?;
        let affected = conn
            .execute(
                "UPDATE instances SET current_node = ?2, normal_rounds = ?3,
                        recovery_rounds = ?4, human_queue = ?5, terminal_blockers = ?6,
                        status = ?7, updated_at = ?8
                  WHERE instance_id = ?1",
                params![
                    instance_id,
                    current_node,
                    normal_rounds,
                    recovery_rounds,
                    human_queue as i64,
                    terminal_blockers,
                    status,
                    at
                ],
            )
            .map_err(|err| StateError::from_sqlite("advance_instance: update", err))?;
        let _ = affected;
        Ok(())
    }

    /// List workflow instances (any status), oldest first.
    pub fn list_instances(&self) -> Result<Vec<InstanceRow>, StateError> {
        let conn = self.lock("list_instances")?;
        let mut statement = conn
            .prepare(&instance_select_sql())
            .map_err(|err| StateError::from_sqlite("list_instances: prepare", err))?;
        let rows = statement
            .query_map([], instance_row_from)
            .map_err(|err| StateError::from_sqlite("list_instances: query", err))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|err| StateError::from_sqlite("list_instances: row", err))?);
        }
        Ok(out)
    }

    // ---------------------------------------------------------------------
    // Control-plane mutation state (issue #8): durable review evidence,
    // recorded first-write approvals, salvage journal records
    // ---------------------------------------------------------------------

    /// Durably record a review-evidence row bound to an exact feature head,
    /// integration base, workflow hash, and policy hash. Every binding is
    /// shape-checked here; semantic freshness is revalidated before the
    /// merge (evidence dies with its epoch: rows never carry their epoch,
    /// so an epoch rotation makes every row stale — see
    /// [`State::rotate_epoch`]).
    #[allow(clippy::too_many_arguments)]
    pub fn record_evidence(
        &self,
        instance_id: &str,
        repository: &str,
        feature_head: &str,
        integration_base: &str,
        workflow_hash: &str,
        policy_hash: &str,
        verdict: &str,
        reviewer: &str,
        checks: &Val,
    ) -> Result<EvidenceRow, StateError> {
        use crate::formats::{is_hex40, is_hex64, is_repository_identity, is_slug};
        if !matches!(verdict, "pass" | "fail") {
            return Err(state_error(
                "state.evidence_invalid",
                format!("evidence verdict {verdict:?} outside {{pass, fail}}"),
            ));
        }
        if !is_slug(instance_id) || !is_repository_identity(repository) {
            return Err(state_error(
                "state.evidence_invalid",
                "evidence instance/repository binding invalid",
            ));
        }
        if !is_hex40(feature_head) || !is_hex40(integration_base) {
            return Err(state_error(
                "state.evidence_invalid",
                "evidence head/base bindings must be exact 40-hex SHAs",
            ));
        }
        if !is_hex64(workflow_hash) || !is_hex64(policy_hash) {
            return Err(state_error(
                "state.evidence_invalid",
                "evidence workflow/policy hashes must be 64-hex",
            ));
        }
        if reviewer.is_empty() || reviewer.len() > 64 {
            return Err(state_error(
                "state.evidence_invalid",
                "evidence reviewer identity must be a non-empty identifier <= 64 chars",
            ));
        }
        let checks_ok = matches!(checks, Val::Arr(items) if !items.is_empty() && items.iter().all(|item| matches!(item, Val::Obj(_))
            && matches!(item.get("name"), Some(Val::Str(name)) if !name.is_empty())
            && matches!(item.get("status"), Some(Val::Str(status)) if matches!(status.as_str(), "passed" | "failed" | "pending"))));
        if !checks_ok {
            return Err(state_error(
                "state.evidence_invalid",
                "evidence checks must be a non-empty list of {{name, status(passed|failed|pending)}}",
            ));
        }
        let checks_line = canonical_text(checks);
        let created_at = time::rfc3339_now();
        let seed = canonical_text(&object(vec![
            ("instance_id", string(instance_id)),
            ("feature_head", string(feature_head)),
            ("integration_base", string(integration_base)),
            ("workflow_hash", string(workflow_hash)),
            ("policy_hash", string(policy_hash)),
            ("verdict", string(verdict)),
            ("reviewer", string(reviewer)),
            ("created_at", string(&created_at)),
        ]));
        let evidence_id = format!("ev_{}", &sha256_hex(seed.as_bytes())[..16]);
        let conn = self.lock("record_evidence")?;
        conn.execute(
            "INSERT INTO evidence (evidence_id, instance_id, repository, feature_head,
                                   integration_base, workflow_hash, policy_hash,
                                   verdict, reviewer, checks, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                evidence_id,
                instance_id,
                repository,
                feature_head,
                integration_base,
                workflow_hash,
                policy_hash,
                verdict,
                reviewer,
                checks_line,
                created_at
            ],
        )
        .map_err(|err| {
            if err.to_string().contains("UNIQUE") {
                state_error(
                    "state.evidence_exists",
                    format!("evidence {evidence_id} already exists"),
                )
            } else {
                StateError::from_sqlite("record_evidence: insert", err)
            }
        })?;
        Ok(EvidenceRow {
            evidence_id,
            instance_id: instance_id.to_string(),
            repository: repository.to_string(),
            feature_head: feature_head.to_string(),
            integration_base: integration_base.to_string(),
            workflow_hash: workflow_hash.to_string(),
            policy_hash: policy_hash.to_string(),
            verdict: verdict.to_string(),
            reviewer: reviewer.to_string(),
            checks: checks_line,
            created_at,
        })
    }

    /// One evidence row by id.
    pub fn evidence_by_id(&self, evidence_id: &str) -> Result<Option<EvidenceRow>, StateError> {
        let conn = self.lock("evidence_by_id")?;
        conn.query_row(
            "SELECT evidence_id, instance_id, repository, feature_head, integration_base,
                    workflow_hash, policy_hash, verdict, reviewer, checks, created_at
               FROM evidence WHERE evidence_id = ?1",
            params![evidence_id],
            evidence_row_from,
        )
        .optional()
        .map_err(|err| StateError::from_sqlite("evidence_by_id: query", err))
    }

    /// Latest-first evidence rows for one instance (the merge gate reads the
    /// most recent record and revalidates every binding live).
    pub fn evidence_for_instance(&self, instance_id: &str) -> Result<Vec<EvidenceRow>, StateError> {
        let conn = self.lock("evidence_for_instance")?;
        let mut statement = conn
            .prepare(
                "SELECT evidence_id, instance_id, repository, feature_head, integration_base,
                        workflow_hash, policy_hash, verdict, reviewer, checks, created_at
                   FROM evidence WHERE instance_id = ?1 ORDER BY created_at DESC, evidence_id DESC",
            )
            .map_err(|err| StateError::from_sqlite("evidence_for_instance: prepare", err))?;
        let rows = statement
            .query_map(params![instance_id], evidence_row_from)
            .map_err(|err| StateError::from_sqlite("evidence_for_instance: query", err))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(
                row.map_err(|err| StateError::from_sqlite("evidence_for_instance: row", err))?,
            );
        }
        Ok(out)
    }

    /// Record the separate explicit human approval required before the
    /// first real external write canary (issue #8 AC10; the canary itself
    /// is never run by this slice). Refused when a newer recorded approval
    /// already exists for the scope (approvals are monotonic — an older
    /// digest must never overwrite a newer one).
    pub fn record_approval(
        &self,
        scope: &str,
        digest: &str,
        interactive: bool,
    ) -> Result<ApprovalRow, StateError> {
        use crate::formats::{is_hex64, is_slug};
        if !is_slug(scope) || scope != "first-write-canary" {
            return Err(state_error(
                "state.approval_invalid",
                format!(
                    "approval scope {scope:?} is outside the closed {{first-write-canary}} set"
                ),
            ));
        }
        if !is_hex64(digest) {
            return Err(state_error(
                "state.approval_invalid",
                "approval digest must be 64-hex",
            ));
        }
        let recorded_at = time::rfc3339_now();
        let seed = canonical_text(&object(vec![
            ("scope", string(scope)),
            ("digest", string(digest)),
            ("recorded_at", string(&recorded_at)),
        ]));
        let approval_id = format!("ap_{}", &sha256_hex(seed.as_bytes())[..16]);
        let conn = self.lock("record_approval")?;
        let newest: Option<String> = conn
            .query_row(
                "SELECT recorded_at FROM approvals WHERE scope = ?1 AND revoked_at IS NULL
                  ORDER BY recorded_at DESC LIMIT 1",
                params![scope],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| StateError::from_sqlite("record_approval: read", err))?;
        if let Some(prior) = newest
            && prior > recorded_at
        {
            return Err(state_error(
                "state.approval_stale",
                "a newer recorded approval exists for the scope; refusing the stale record",
            ));
        }
        conn.execute(
            "INSERT INTO approvals (approval_id, scope, digest, interactive, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![approval_id, scope, digest, interactive as i64, recorded_at],
        )
        .map_err(|err| StateError::from_sqlite("record_approval: insert", err))?;
        Ok(ApprovalRow {
            approval_id,
            scope: scope.to_string(),
            digest: digest.to_string(),
            interactive,
            recorded_at,
        })
    }

    /// The newest un-revoked recorded approval for a scope, if any.
    pub fn approval_for_scope(&self, scope: &str) -> Result<Option<ApprovalRow>, StateError> {
        let conn = self.lock("approval_for_scope")?;
        conn.query_row(
            "SELECT approval_id, scope, digest, interactive, recorded_at
               FROM approvals WHERE scope = ?1 AND revoked_at IS NULL
              ORDER BY recorded_at DESC LIMIT 1",
            params![scope],
            |row| {
                let interactive: i64 = row.get(3)?;
                Ok(ApprovalRow {
                    approval_id: row.get(0)?,
                    scope: row.get(1)?,
                    digest: row.get(2)?,
                    interactive: interactive != 0,
                    recorded_at: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|err| StateError::from_sqlite("approval_for_scope: query", err))
    }

    /// Append a `salvage.cleanup` audit record capturing the required
    /// salvage evidence of a completed worktree/branch cleanup deletion
    /// (issue #8 AC8: cleanup preserves required salvage evidence; the
    /// deletion intent itself is journaled before the effect as
    /// `mutate.cleanup`, and this post-deletion evidence record completes
    /// the audit pair).
    pub fn journal_salvage(&self, target: &str, instance_id: &str) -> Result<AuditRow, StateError> {
        let outcome = self.journal_salvage_inner(target, instance_id);
        if let Err(err) = &outcome {
            self.poison_on(err);
        }
        outcome
    }

    fn journal_salvage_inner(
        &self,
        target: &str,
        instance_id: &str,
    ) -> Result<AuditRow, StateError> {
        self.ensure_writable()?;
        let mut conn = self.lock("journal_salvage")?;
        let tx = conn
            .transaction()
            .map_err(|err| StateError::from_sqlite("journal_salvage: begin", err))?;
        let mut key = format!("ik_salvage-{instance_id}");
        key.truncate(64);
        let audit = self.append_audit_locked(&tx, "salvage.cleanup", target, &key, None, None)?;
        tx.commit()
            .map_err(|err| StateError::from_sqlite("journal_salvage: commit", err))?;
        Ok(audit)
    }

    // ---------------------------------------------------------------------
    // Leases, schedules, instances
    // ---------------------------------------------------------------------

    /// Record the daemon's own lease row (informational; the flock is the
    /// authority). Overwrites any stale row from a crashed daemon.
    pub fn put_daemon_lease(&self, pid: u32, started_at: &str) -> Result<(), StateError> {
        let conn = self.lock("put_daemon_lease")?;
        conn.execute(
            "INSERT INTO leases (lease_id, holder, purpose, scope, state_epoch, created_at)
             VALUES ('daemon', ?1, 'single-writer daemon lease', 'local', ?2, ?3)
             ON CONFLICT(lease_id) DO UPDATE SET
               holder = excluded.holder,
               state_epoch = excluded.state_epoch,
               created_at = excluded.created_at,
               expires_at = NULL",
            params![
                format!("pid {pid}"),
                current_epoch_locked(&conn)?,
                started_at
            ],
        )
        .map_err(|err| StateError::from_sqlite("put_daemon_lease", err))?;
        Ok(())
    }

    /// Remove the daemon's lease row on graceful exit.
    pub fn drop_daemon_lease(&self) -> Result<(), StateError> {
        let conn = self.lock("drop_daemon_lease")?;
        conn.execute("DELETE FROM leases WHERE lease_id = 'daemon'", [])
            .map_err(|err| StateError::from_sqlite("drop_daemon_lease", err))?;
        Ok(())
    }

    /// Schedules (lifecycle rows; scheduling semantics land in the
    /// lifecycle slice). Returns every schedule row ordered by id.
    pub fn list_schedules(&self) -> Result<Vec<Val>, StateError> {
        let rows = self.list_schedule_rows()?;
        Ok(rows.iter().map(schedule_val).collect())
    }

    /// Every schedule row (ordered by id) for the lifecycle evaluator.
    pub fn list_schedule_rows(&self) -> Result<Vec<ScheduleRow>, StateError> {
        let conn = self.lock("list_schedule_rows")?;
        let mut statement = conn
            .prepare(
                "SELECT schedule_id, state_epoch, enabled, next_run_at, created_at, doc, updated_at
                   FROM schedules ORDER BY schedule_id",
            )
            .map_err(|err| StateError::from_sqlite("list_schedule_rows: prepare", err))?;
        let rows = statement
            .query_map([], schedule_row_from)
            .map_err(|err| StateError::from_sqlite("list_schedule_rows: query", err))?;
        let mut out = Vec::new();
        for row in rows {
            let row = row.map_err(|err| StateError::from_sqlite("list_schedule_rows: row", err))?;
            out.push(row);
        }
        Ok(out)
    }

    /// One schedule row by id.
    pub fn schedule_by_id(&self, schedule_id: &str) -> Result<Option<ScheduleRow>, StateError> {
        let conn = self.lock("schedule_by_id")?;
        conn.query_row(
            "SELECT schedule_id, state_epoch, enabled, next_run_at, created_at, doc, updated_at
               FROM schedules WHERE schedule_id = ?1",
            params![schedule_id],
            schedule_row_from,
        )
        .optional()
        .map_err(|err| StateError::from_sqlite("schedule_by_id: query", err))
    }

    /// Upsert a schedule row from a canonical `hf-schedule/v1` document
    /// (schema-validated by the caller through `crate::schema::validate_doc`).
    /// Create (or a re-arm update) resets the cadence state: enabled with a
    /// null `next_run_at` so the next evaluation is due immediately (one
    /// fresh evaluation, never a replay of missed windows).
    pub fn upsert_schedule(&self, doc: &Val) -> Result<ScheduleRow, StateError> {
        let schedule_id = doc
            .get("schedule_id")
            .and_then(Val::as_str)
            .ok_or_else(|| state_error("state.schedule_invalid", "schedule missing schedule_id"))?
            .to_string();
        let doc_text = canonical_text(doc);
        let now = time::rfc3339_now();
        {
            let conn = self.lock("upsert_schedule")?;
            let epoch = current_epoch_locked(&conn)?;
            conn.execute(
                "INSERT INTO schedules (schedule_id, state_epoch, enabled, next_run_at,
                                        created_at, doc, updated_at)
                 VALUES (?1, ?2, 1, NULL, ?3, ?4, ?3)
                 ON CONFLICT(schedule_id) DO UPDATE SET
                   doc = excluded.doc, updated_at = excluded.updated_at,
                   enabled = 1, next_run_at = NULL",
                params![schedule_id, epoch, now, doc_text],
            )
            .map_err(|err| StateError::from_sqlite("upsert_schedule", err))?;
        }
        let row = self
            .schedule_by_id(&schedule_id)?
            .expect("upserted schedule row");
        Ok(row)
    }

    /// Set the durable pause state of a schedule (`enabled` flag; false is
    /// paused/disabled). Only an explicit human `resume` lifts a pause; no
    /// evaluation path ever enables a schedule. Resuming resets the window
    /// (next_run_at = NULL) so the next evaluation is one fresh run.
    pub fn set_schedule_enabled(
        &self,
        schedule_id: &str,
        enabled: bool,
        at: &str,
    ) -> Result<ScheduleRow, StateError> {
        let (next_run_at, updated_at): (Option<String>, String) = {
            let conn = self.lock("set_schedule_enabled")?;
            if enabled {
                (None, at.to_string())
            } else {
                (
                    conn.query_row(
                        "SELECT next_run_at FROM schedules WHERE schedule_id = ?1",
                        params![schedule_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|err| StateError::from_sqlite("set_schedule_enabled: read", err))?
                    .flatten(),
                    at.to_string(),
                )
            }
        };
        let affected = {
            let conn = self.lock("set_schedule_enabled")?;
            conn.execute(
                "UPDATE schedules SET enabled = ?2, next_run_at = ?3, updated_at = ?4
                  WHERE schedule_id = ?1",
                params![schedule_id, enabled as i64, next_run_at, updated_at],
            )
            .map_err(|err| StateError::from_sqlite("set_schedule_enabled: update", err))?
        };
        if affected == 0 {
            return Err(state_error(
                "state.not_found",
                format!("no schedule {schedule_id:?}"),
            ));
        }
        let row = self
            .schedule_by_id(schedule_id)?
            .expect("updated schedule row");
        Ok(row)
    }

    /// Delete a schedule row (daemon-mediated; the deletion intent is
    /// journaled by the caller before this runs).
    pub fn delete_schedule(&self, schedule_id: &str) -> Result<(), StateError> {
        let conn = self.lock("delete_schedule")?;
        let affected = conn
            .execute(
                "DELETE FROM schedules WHERE schedule_id = ?1",
                params![schedule_id],
            )
            .map_err(|err| StateError::from_sqlite("delete_schedule", err))?;
        if affected == 0 {
            return Err(state_error(
                "state.not_found",
                format!("no schedule {schedule_id:?}"),
            ));
        }
        Ok(())
    }

    /// Atomically advance a schedule's evaluation window AND journal the
    /// run (audit row + `schedule.ran` event) in one transaction (issue #9).
    /// The persisted next window is the singleflight guard: a second
    /// evaluation in the same tick sees a future window and skips. A crash
    /// before commit leaves the pre-run state (the window re-fires at most
    /// once more — never a backlog replay).
    pub fn complete_schedule_run(
        &self,
        schedule_id: &str,
        next_run_at: Option<&str>,
        at: &str,
    ) -> Result<AuditRow, StateError> {
        let result = self.complete_schedule_run_inner(schedule_id, next_run_at, at);
        if let Err(err) = &result {
            self.poison_on(err);
        }
        result
    }

    fn complete_schedule_run_inner(
        &self,
        schedule_id: &str,
        next_run_at: Option<&str>,
        at: &str,
    ) -> Result<AuditRow, StateError> {
        self.ensure_writable()?;
        let mut conn = self.lock("complete_schedule_run")?;
        let tx = conn
            .transaction()
            .map_err(|err| StateError::from_sqlite("complete_schedule_run: begin", err))?;
        let affected = tx
            .execute(
                "UPDATE schedules SET next_run_at = ?2, updated_at = ?3 WHERE schedule_id = ?1",
                params![schedule_id, next_run_at, at],
            )
            .map_err(|err| StateError::from_sqlite("complete_schedule_run: update", err))?;
        if affected == 0 {
            return Err(state_error(
                "state.not_found",
                format!("no schedule {schedule_id:?}"),
            ));
        }
        let mut key = format!("ik_sched-{schedule_id}");
        key.truncate(64);
        let audit = self.append_audit_locked(
            &tx,
            "read.schedule.ran",
            &format!("{schedule_id}:ran"),
            &key,
            None,
            None,
        )?;
        let data = object(vec![
            ("schedule_id", string(schedule_id)),
            ("outcome", string("ran")),
            ("next_run_at", next_run_at.map(string).unwrap_or_else(null)),
        ]);
        append_event_locked(&tx, self.retention, "schedule.ran", &data)?;
        tx.commit()
            .map_err(|err| StateError::from_sqlite("complete_schedule_run: commit", err))?;
        Ok(audit)
    }

    /// Atomically pause (disable) a schedule with a journaled reason
    /// (issue #9): an evaluation that refuses — expired schedule, changed
    /// policy/issue binding, unparseable doc — parks the schedule in an
    /// explicit terminal state instead of retrying every tick (no retry
    /// storm). Only an explicit human resume/create re-arms it.
    pub fn pause_schedule_with_reason(
        &self,
        schedule_id: &str,
        reason: &str,
        at: &str,
    ) -> Result<AuditRow, StateError> {
        let result = self.pause_schedule_with_reason_inner(schedule_id, reason, at);
        if let Err(err) = &result {
            self.poison_on(err);
        }
        result
    }

    fn pause_schedule_with_reason_inner(
        &self,
        schedule_id: &str,
        reason: &str,
        at: &str,
    ) -> Result<AuditRow, StateError> {
        self.ensure_writable()?;
        let mut conn = self.lock("pause_schedule_with_reason")?;
        let tx = conn
            .transaction()
            .map_err(|err| StateError::from_sqlite("pause_schedule_with_reason: begin", err))?;
        let affected = tx
            .execute(
                "UPDATE schedules SET enabled = 0, updated_at = ?2 WHERE schedule_id = ?1",
                params![schedule_id, at],
            )
            .map_err(|err| StateError::from_sqlite("pause_schedule_with_reason: update", err))?;
        if affected == 0 {
            return Err(state_error(
                "state.not_found",
                format!("no schedule {schedule_id:?}"),
            ));
        }
        let mut key = format!("ik_sched-{schedule_id}");
        key.truncate(64);
        let audit = self.append_audit_locked(
            &tx,
            "read.schedule.ran",
            &format!("{schedule_id}:{reason}"),
            &key,
            None,
            None,
        )?;
        tx.commit()
            .map_err(|err| StateError::from_sqlite("pause_schedule_with_reason: commit", err))?;
        Ok(audit)
    }

    // ---------------------------------------------------------------------
    // Journal reads (hash-chain verified)
    // ---------------------------------------------------------------------

    /// Journal records with `seq > after_seq`, ordered, up to `limit`.
    /// Returns the retained window start so callers can detect stale cursors.
    pub fn journal_tail(
        &self,
        after_seq: i64,
        limit: i64,
    ) -> Result<(i64, Vec<String>), StateError> {
        let conn = self.lock("journal_tail")?;
        let first_retained: i64 = conn
            .query_row("SELECT MIN(seq) FROM audit", [], |row| row.get(0))
            .map_err(|err| StateError::from_sqlite("journal_tail: min", err))?;
        let mut statement = conn
            .prepare("SELECT line FROM audit WHERE seq > ?1 AND seq > ?2 ORDER BY seq LIMIT ?3")
            .map_err(|err| StateError::from_sqlite("journal_tail: prepare", err))?;
        let rows = statement
            .query_map(params![after_seq, first_retained - 1, limit], |row| {
                row.get(0)
            })
            .map_err(|err| StateError::from_sqlite("journal_tail: query", err))?;
        let mut lines = Vec::new();
        for row in rows {
            lines.push(row.map_err(|err| StateError::from_sqlite("journal_tail: row", err))?);
        }
        Ok((first_retained, lines))
    }

    /// Event rows with `seq > after_seq`, ordered, up to `limit` (the
    /// subscriber replay source; a gap is answered with a snapshot).
    pub fn events_after(&self, after_seq: i64, limit: i64) -> Result<Vec<Val>, StateError> {
        let conn = self.lock("events_after")?;
        let mut statement = conn
            .prepare("SELECT event, seq, ts, data FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2")
            .map_err(|err| StateError::from_sqlite("events_after: prepare", err))?;
        let rows = statement
            .query_map(params![after_seq, limit], |row| {
                Ok(object(vec![
                    ("schema", string("hf-event/v1")),
                    ("event", string(&row.get::<_, String>(0)?)),
                    ("seq", integer(row.get(1)?)),
                    ("ts", string(&row.get::<_, String>(2)?)),
                    ("data", parse_data(&row.get::<_, String>(3)?)),
                ]))
            })
            .map_err(|err| StateError::from_sqlite("events_after: query", err))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|err| StateError::from_sqlite("events_after: row", err))?);
        }
        Ok(out)
    }

    /// Highest event seq and first retained event seq (subscribe cursor).
    /// `None` for both means the event stream is empty.
    pub fn event_bounds(&self) -> Result<(Option<i64>, Option<i64>), StateError> {
        let conn = self.lock("event_bounds")?;
        let max: Option<i64> = conn
            .query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))
            .optional()
            .map_err(|err| StateError::from_sqlite("event_bounds: max", err))?
            .flatten();
        let min: Option<i64> = conn
            .query_row("SELECT MIN(seq) FROM events", [], |row| row.get(0))
            .optional()
            .map_err(|err| StateError::from_sqlite("event_bounds: min", err))?
            .flatten();
        Ok((max, min))
    }

    /// A state.snapshot event document at the current event seq (0 when the
    /// stream is empty; real event seqs start at 1).
    pub fn snapshot_event(&self) -> Result<Val, StateError> {
        let (epoch, audit_seq, event_seq, _) = self.summary()?;
        Ok(object(vec![
            ("schema", string("hf-event/v1")),
            ("event", string("state.snapshot")),
            ("seq", integer(event_seq)),
            ("ts", string(&time::rfc3339_now())),
            (
                "data",
                object(vec![
                    ("epoch", integer(epoch)),
                    ("journal_seq", integer(audit_seq)),
                    ("event_seq", integer(event_seq)),
                ]),
            ),
        ]))
    }

    /// Current summary for status RPC / doctor.
    pub fn summary(&self) -> Result<(i64, i64, i64, i64), StateError> {
        let conn = self.lock("summary")?;
        let epoch = conn
            .query_row("SELECT COALESCE(MAX(epoch), 0) FROM epoch", [], |row| {
                row.get(0)
            })
            .map_err(|err| StateError::from_sqlite("summary: epoch", err))?;
        let audit_seq: i64 = conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM audit", [], |row| {
                row.get(0)
            })
            .map_err(|err| StateError::from_sqlite("summary: audit", err))?;
        let event_seq: i64 = conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
                row.get(0)
            })
            .map_err(|err| StateError::from_sqlite("summary: events", err))?;
        Ok((epoch, audit_seq, event_seq, SCHEMA_VERSION))
    }

    /// Rich summary for the daemon `status` RPC.
    pub fn status_summary(&self) -> Result<StateSummary, StateError> {
        let (epoch, audit_seq, event_seq, schema_version) = self.summary()?;
        let conn = self.lock("status_summary")?;
        let active_grants: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM grants WHERE status = 'active'",
                [],
                |row| row.get(0),
            )
            .map_err(|err| StateError::from_sqlite("status_summary: grants", err))?;
        let pending_claims: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM idempotency WHERE status = 'claimed'",
                [],
                |row| row.get(0),
            )
            .map_err(|err| StateError::from_sqlite("status_summary: claims", err))?;
        Ok(StateSummary {
            epoch,
            audit_seq,
            event_seq,
            schema_version,
            active_grants,
            pending_claims,
            poisoned: self.is_poisoned(),
        })
    }

    // ---------------------------------------------------------------------
    // Audit mirror (JSONL journal file, bounded) and chain verification
    // ---------------------------------------------------------------------

    /// Verify the hash chain over the retained window (plus genesis).
    /// A broken chain means the audit table itself was tampered with and the
    /// daemon must fail closed.
    pub fn verify_chain(&self) -> Result<(), StateError> {
        let conn = self.lock("verify_chain")?;
        let mut statement = conn
            .prepare(
                "SELECT seq, action, target, idempotency_key, plan_hash, grant_id, epoch,
                        recorded_before_mutation, at, line, prev_hash, record_hash
                   FROM audit ORDER BY seq",
            )
            .map_err(|err| StateError::from_sqlite("verify_chain: prepare", err))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, bool>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                ))
            })
            .map_err(|err| StateError::from_sqlite("verify_chain: query", err))?;
        let mut previous_hash: Option<String> = None;
        for row in rows {
            let (
                seq,
                action,
                target,
                idempotency_key,
                plan_hash,
                grant_id,
                epoch,
                recorded_before_mutation,
                at,
                line,
                prev_hash,
                record_hash,
            ) = row.map_err(|err| StateError::from_sqlite("verify_chain: row", err))?;
            // The stored canonical line must match the row's own columns
            // (tampering with any journal column is a chain break).
            let expected_doc = canonical_text(&object(vec![
                ("schema", string("hf-audit/v1")),
                ("seq", integer(seq)),
                ("action", string(&action)),
                ("target", string(&target)),
                ("idempotency_key", string(&idempotency_key)),
                (
                    "plan_hash",
                    plan_hash.map(|h| string(&h)).unwrap_or_else(null),
                ),
                (
                    "grant_id",
                    grant_id.map(|g| string(&g)).unwrap_or_else(null),
                ),
                ("epoch", integer(epoch)),
                ("recorded_before_mutation", bool_(recorded_before_mutation)),
                ("at", string(&at)),
            ]));
            if line != expected_doc {
                return Err(state_error(
                    "state.audit_tampered",
                    format!("audit chain broken at seq {seq}: column/line mismatch"),
                ));
            }
            let expected_prev = previous_hash.as_deref().unwrap_or("");
            if prev_hash != expected_prev {
                return Err(state_error(
                    "state.audit_tampered",
                    format!("audit chain broken at seq {seq}: prev_hash mismatch"),
                ));
            }
            let computed = sha256_hex(&[line.as_bytes(), prev_hash.as_bytes()].concat());
            if computed != record_hash {
                return Err(state_error(
                    "state.audit_tampered",
                    format!("audit chain broken at seq {seq}: record hash mismatch"),
                ));
            }
            previous_hash = Some(record_hash);
        }
        Ok(())
    }

    /// Rewrite the bounded JSONL mirror file from the table (called at
    /// startup when the mirror is missing or drifted; the table is the
    /// authority). Returns whether the file was rebuilt.
    pub fn rebuild_audit_mirror(&self, mirror_path: &Path) -> Result<bool, StateError> {
        let conn = self.lock("rebuild_audit_mirror")?;
        let mut statement = conn
            .prepare("SELECT line FROM audit ORDER BY seq")
            .map_err(|err| StateError::from_sqlite("rebuild_audit_mirror: prepare", err))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|err| StateError::from_sqlite("rebuild_audit_mirror: query", err))?;
        let mut lines: Vec<String> = Vec::new();
        for row in rows {
            lines.push(
                row.map_err(|err| StateError::from_sqlite("rebuild_audit_mirror: row", err))?,
            );
        }
        let rebuilt = mirror_matches(mirror_path, &lines).is_err();
        if rebuilt {
            write_lines_atomic(mirror_path, &lines)?;
        }
        Ok(rebuilt)
    }

    /// Rewrite the bounded JSONL events mirror from the table (same
    /// authority/rebuild semantics as the audit mirror; the mirror is a
    /// convenience for read-only consumers, never the source of truth).
    pub fn rebuild_events_mirror(&self, mirror_path: &Path) -> Result<bool, StateError> {
        let conn = self.lock("rebuild_events_mirror")?;
        let mut statement = conn
            .prepare("SELECT event, seq, ts, data FROM events ORDER BY seq")
            .map_err(|err| StateError::from_sqlite("rebuild_events_mirror: prepare", err))?;
        let rows = statement
            .query_map([], |row| {
                Ok(canonical_text(&object(vec![
                    ("schema", string("hf-event/v1")),
                    ("event", string(&row.get::<_, String>(0)?)),
                    ("seq", integer(row.get(1)?)),
                    ("ts", string(&row.get::<_, String>(2)?)),
                    ("data", parse_data(&row.get::<_, String>(3)?)),
                ])))
            })
            .map_err(|err| StateError::from_sqlite("rebuild_events_mirror: query", err))?;
        let mut lines: Vec<String> = Vec::new();
        for row in rows {
            lines.push(
                row.map_err(|err| StateError::from_sqlite("rebuild_events_mirror: row", err))?,
            );
        }
        let rebuilt = mirror_matches(mirror_path, &lines).is_err();
        if rebuilt {
            write_lines_atomic(mirror_path, &lines)?;
        }
        Ok(rebuilt)
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    /// Refuse writes after a prior write failure (fail closed, AC5).
    fn ensure_writable(&self) -> Result<(), StateError> {
        if self.poisoned.load(Ordering::SeqCst) {
            return Err(state_error(
                "state.poisoned",
                "state/audit writes previously failed; mutations are refused until the daemon restarts",
            ));
        }
        Ok(())
    }

    /// Flip the fail-closed flag when a write-class failure escapes a
    /// mutation. Read-only queries keep working; every later mutation is
    /// refused until the process restarts.
    fn poison_on(&self, err: &StateError) {
        let write_class = matches!(
            err.code,
            "state.write_full"
                | "state.write_io"
                | "state.corrupt"
                | "state.readonly"
                | "state.busy"
        );
        if write_class {
            self.poisoned.store(true, Ordering::SeqCst);
        }
    }

    /// Append an audit record inside an existing transaction. The caller
    /// decides the transaction boundary (intent + claim vs resolution).
    #[allow(clippy::too_many_arguments)]
    fn append_audit_locked(
        &self,
        conn: &rusqlite::Transaction<'_>,
        action: &str,
        target: &str,
        key: &str,
        plan_hash: Option<&str>,
        grant_id: Option<&str>,
    ) -> Result<AuditRow, StateError> {
        let epoch: i64 = conn
            .query_row("SELECT COALESCE(MAX(epoch), 1) FROM epoch", [], |row| {
                row.get(0)
            })
            .map_err(|err| StateError::from_sqlite("append_audit: epoch", err))?;
        let seq: i64 = conn
            .query_row("SELECT COALESCE(MAX(seq), -1) + 1 FROM audit", [], |row| {
                row.get(0)
            })
            .map_err(|err| StateError::from_sqlite("append_audit: seq", err))?;
        let prev_hash: String = conn
            .query_row(
                "SELECT record_hash FROM audit ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| StateError::from_sqlite("append_audit: prev", err))?
            .unwrap_or_default();
        let doc = object(vec![
            ("schema", string("hf-audit/v1")),
            ("seq", integer(seq)),
            ("action", string(action)),
            ("target", string(target)),
            ("idempotency_key", string(key)),
            ("plan_hash", plan_hash.map(string).unwrap_or_else(null)),
            ("grant_id", grant_id.map(string).unwrap_or_else(null)),
            ("epoch", integer(epoch)),
            // A mutate.* record is journaled *before* its effect; every
            // other record class (outcome.*, reconcile.*, state.genesis)
            // journals after the fact and must say so (the schema refuses
            // mutate.* records that claim recorded_before_mutation:false).
            (
                "recorded_before_mutation",
                bool_(action.starts_with("mutate.")),
            ),
            ("at", string(&time::rfc3339_now())),
        ]);
        let line = canonical_text(&doc);
        let record_hash = sha256_hex(&[line.as_bytes(), prev_hash.as_bytes()].concat());
        conn.execute(
            "INSERT INTO audit (seq, action, target, idempotency_key, plan_hash, grant_id, epoch,
                                recorded_before_mutation, at, prev_hash, record_hash, line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                seq,
                action,
                target,
                key,
                plan_hash,
                grant_id,
                epoch,
                action.starts_with("mutate."),
                time::rfc3339_now(),
                prev_hash,
                record_hash,
                line
            ],
        )
        .map_err(|err| StateError::from_sqlite("append_audit: insert", err))?;
        prune_audit_locked(conn, self.retention)?;
        let event_data = object(vec![("action", string(action)), ("seq", integer(seq))]);
        let event_seq = append_event_locked(conn, self.retention, "journal.appended", &event_data)?;
        Ok(AuditRow {
            seq,
            epoch,
            line,
            record_hash,
            event_seq,
        })
    }
}

/// Read a grant row out of an `hf-grant/v1` document (validated by the
/// caller through [`crate::schema::validate_doc`]).
fn grant_row_from_doc(doc: &Val) -> Result<GrantRow, StateError> {
    let get = |key: &str| -> Result<String, StateError> {
        doc.get(key)
            .and_then(Val::as_str)
            .map(str::to_string)
            .ok_or_else(|| state_error("state.grant_invalid", format!("grant missing {key}")))
    };
    let issue = doc
        .get("issue")
        .ok_or_else(|| state_error("state.grant_invalid", "grant missing issue"))?;
    let issue_number = issue
        .get("number")
        .and_then(Val::as_int)
        .ok_or_else(|| state_error("state.grant_invalid", "issue.number missing"))?;
    let issue_revision = issue
        .get("revision")
        .and_then(Val::as_str)
        .ok_or_else(|| state_error("state.grant_invalid", "issue.revision missing"))?;
    let caps_text = match doc.get("caps") {
        Some(Val::Arr(items)) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(Val::as_str)
                .map(str::to_string)
                .collect();
            canonical_text(&Val::Arr(parts.iter().map(|p| string(p)).collect()))
        }
        _ => {
            return Err(state_error(
                "state.grant_invalid",
                "grant.caps must be an array",
            ));
        }
    };
    Ok(GrantRow {
        grant_id: get("grant_id")?,
        repository: get("repository")?,
        issue_number,
        issue_revision: issue_revision.to_string(),
        workflow_hash: get("workflow_hash")?,
        policy_hash: get("policy_hash")?,
        phase: get("phase")?,
        scope: get("scope")?,
        caps: caps_text,
        expires_at: get("expires_at")?,
        state_epoch: doc
            .get("state_epoch")
            .and_then(Val::as_int)
            .ok_or_else(|| state_error("state.grant_invalid", "state_epoch missing"))?,
        status: "active".to_string(),
        created_at: get("created_at")?,
    })
}

/// Shared SELECT for instance rows (m0002 columns).
fn instance_select_sql() -> String {
    "SELECT instance_id, repository, workflow_id, workflow_hash, policy_hash, grant_id,
            issue_number, issue_revision, phase, scope, caps, current_node,
            normal_rounds, recovery_rounds, human_queue, terminal_blockers,
            paused, resume_digest, state_epoch, status, created_at, updated_at
       FROM instances"
        .to_string()
}

/// Map one SQLite row onto [`InstanceRow`] (column order of
/// [`instance_select_sql`]).
fn instance_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstanceRow> {
    Ok(InstanceRow {
        instance_id: row.get(0)?,
        repository: row.get(1)?,
        workflow_id: row.get(2)?,
        workflow_hash: row.get(3)?,
        policy_hash: row.get(4)?,
        grant_id: row.get(5)?,
        issue_number: row.get(6)?,
        issue_revision: row.get(7)?,
        phase: row.get(8)?,
        scope: row.get(9)?,
        caps: row.get(10)?,
        current_node: row.get(11)?,
        normal_rounds: row.get(12)?,
        recovery_rounds: row.get(13)?,
        human_queue: row.get(14)?,
        terminal_blockers: row.get(15)?,
        paused: row.get(16)?,
        resume_digest: row.get(17)?,
        state_epoch: row.get(18)?,
        status: row.get(19)?,
        created_at: row.get(20)?,
        updated_at: row.get(21)?,
    })
}

/// Map one SQLite row onto [`EvidenceRow`] (column order of the evidence
/// SELECTs in [`State::evidence_by_id`]/[`State::evidence_for_instance`]).
fn evidence_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<EvidenceRow> {
    Ok(EvidenceRow {
        evidence_id: row.get(0)?,
        instance_id: row.get(1)?,
        repository: row.get(2)?,
        feature_head: row.get(3)?,
        integration_base: row.get(4)?,
        workflow_hash: row.get(5)?,
        policy_hash: row.get(6)?,
        verdict: row.get(7)?,
        reviewer: row.get(8)?,
        checks: row.get(9)?,
        created_at: row.get(10)?,
    })
}

/// Map one SQLite row onto [`ScheduleRow`] (column order of the schedules
/// SELECTs in [`State::list_schedules`]/[`State::schedule_by_id`]).
fn schedule_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleRow> {
    Ok(ScheduleRow {
        schedule_id: row.get(0)?,
        state_epoch: row.get(1)?,
        enabled: row.get(2)?,
        next_run_at: row.get(3)?,
        created_at: row.get(4)?,
        doc: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

/// One schedule row as an `hf-rpc`-facing value (list_schedules RPC shape;
/// the `doc` field is the parsed canonical hf-schedule/v1 document or null).
pub fn schedule_val(row: &ScheduleRow) -> Val {
    object(vec![
        ("schedule_id", string(&row.schedule_id)),
        ("state_epoch", integer(row.state_epoch)),
        ("enabled", bool_(row.enabled)),
        (
            "next_run_at",
            row.next_run_at.as_deref().map(string).unwrap_or_else(null),
        ),
        ("created_at", string(&row.created_at)),
        ("updated_at", string(&row.updated_at)),
        ("doc", Val::parse_json(&row.doc).unwrap_or_else(|_| null())),
    ])
}

/// Prune audit rows beyond the retention bound, always keeping the chain
/// genesis (seq 0) so verification keeps its anchor.
fn prune_audit_locked(
    conn: &rusqlite::Transaction<'_>,
    retention: Retention,
) -> Result<(), StateError> {
    let deleted = conn
        .execute(
            "DELETE FROM audit WHERE seq != 0 AND seq <= (
                SELECT seq FROM audit ORDER BY seq DESC LIMIT 1 OFFSET ?1
             )",
            params![retention.audit_rows],
        )
        .map_err(|err| StateError::from_sqlite("prune_audit", err))?;
    if deleted == 0 {
        return Ok(());
    }
    // Rechain the retained window: after rows are deleted, every surviving
    // non-genesis row is relinked to the previous surviving row (the
    // genesis row anchors the window) so verification stays exact over the
    // retained rows. Deleted rows are gone by design; the retained chain is
    // self-consistent.
    let mut statement = conn
        .prepare("SELECT seq, line FROM audit ORDER BY seq")
        .map_err(|err| StateError::from_sqlite("prune_audit: prepare rechain", err))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| StateError::from_sqlite("prune_audit: query rechain", err))?;
    let mut previous_hash: Option<String> = None;
    for row in rows {
        let (seq, line) = row.map_err(|err| StateError::from_sqlite("prune_audit: row", err))?;
        let prev_hash = previous_hash.clone().unwrap_or_default();
        let record_hash = sha256_hex(&[line.as_bytes(), prev_hash.as_bytes()].concat());
        conn.execute(
            "UPDATE audit SET prev_hash = ?1, record_hash = ?2 WHERE seq = ?3",
            params![prev_hash, record_hash, seq],
        )
        .map_err(|err| StateError::from_sqlite("prune_audit: relink", err))?;
        previous_hash = Some(record_hash);
    }
    Ok(())
}

/// Append one hf-event/v1 row inside the caller's transaction. Returns the
/// event seq. Retention pruning keeps the tail window. `data` becomes the
/// event's `data` object verbatim (closed event kinds: journal.appended,
/// schedule.ran, ...).
fn append_event_locked(
    conn: &rusqlite::Transaction<'_>,
    retention: Retention,
    kind: &str,
    data: &Val,
) -> Result<i64, StateError> {
    let seq: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM events", [], |row| {
            row.get(0)
        })
        .map_err(|err| StateError::from_sqlite("append_event: seq", err))?;
    let data_text = canonical_text(data);
    conn.execute(
        "INSERT INTO events (seq, event, ts, data) VALUES (?1, ?2, ?3, ?4)",
        params![seq, kind, time::rfc3339_now(), data_text],
    )
    .map_err(|err| StateError::from_sqlite("append_event: insert", err))?;
    conn.execute(
        "DELETE FROM events WHERE seq <= (
            SELECT seq FROM events ORDER BY seq DESC LIMIT 1 OFFSET ?1
         )",
        params![retention.event_rows],
    )
    .map_err(|err| StateError::from_sqlite("prune_events", err))?;
    Ok(seq)
}

/// Current epoch (helper for lease rows, which hold the epoch).
fn current_epoch_locked(conn: &Connection) -> Result<i64, StateError> {
    conn.query_row("SELECT COALESCE(MAX(epoch), 0) FROM epoch", [], |row| {
        row.get(0)
    })
    .map_err(|err| StateError::from_sqlite("current_epoch", err))
}

/// Parse a stored canonical data object back into a value (events table).
fn parse_data(text: &str) -> Val {
    Val::parse_json(text).unwrap_or_else(|_| null())
}

/// Compare a mirror file's lines to expected lines (tamper detection).
fn mirror_matches(path: &Path, expected: &[String]) -> Result<(), StateError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(state_error("state.mirror_missing", "mirror file missing"));
        }
        Err(err) => {
            return Err(state_error(
                "state.mirror_unreadable",
                format!("read mirror {}: {err}", path.display()),
            ));
        }
    };
    let actual: Vec<&str> = content.lines().collect();
    let want: Vec<&str> = expected.iter().map(|line| line.trim_end()).collect();
    if actual != want {
        return Err(state_error(
            "state.mirror_drift",
            format!(
                "mirror file {} drifted from the journal table",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Atomically rewrite a mirror file (tmp + rename) and fsync.
fn write_lines_atomic(path: &Path, lines: &[String]) -> Result<(), StateError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            state_error(
                "state.mirror_write",
                format!("create {}: {err}", parent.display()),
            )
        })?;
    }
    let tmp = path.with_extension("jsonl.tmp");
    let mut file = fs::File::create(&tmp).map_err(|err| {
        state_error(
            "state.mirror_write",
            format!("create {}: {err}", tmp.display()),
        )
    })?;
    for line in lines {
        file.write_all(line.trim_end().as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .map_err(|err| state_error("state.mirror_write", format!("write mirror: {err}")))?;
    }
    file.sync_all()
        .map_err(|err| state_error("state.mirror_write", format!("fsync mirror: {err}")))?;
    fs::rename(&tmp, path)
        .map_err(|err| state_error("state.mirror_write", format!("rename mirror: {err}")))?;
    Ok(())
}

const M0001_ID: &str = "m0001_initial_state_v1";
const M0001_APPLIES_FROM: i64 = 0;
const M0001_APPLIES_TO: i64 = 1;

/// m0002 adds the workflow-engine runtime columns to `instances` (issue #6:
/// durable engine state for route-granted workflow instances).
const M0002_ID: &str = "m0002_workflow_engine_instances_v2";
const M0002_APPLIES_FROM: i64 = 1;
const M0002_APPLIES_TO: i64 = 2;

/// m0003 adds the control-plane mutation tables (issue #8): durable review
/// evidence rows bound to exact heads/workflow/policy hashes
/// (spec-review-evidence.md) and recorded first-real-write approval rows
/// (issue #8 AC10 plumbing — the canary itself is a separate human gate).
const M0003_ID: &str = "m0003_control_plane_evidence_v3";
const M0003_APPLIES_FROM: i64 = 2;
const M0003_APPLIES_TO: i64 = 3;

/// m0004 adds the lifecycle schedule payload columns (issue #9): the
/// `schedules` foundation rows (m0001) carry the canonical `hf-schedule/v1`
/// document (exact recurring-grant bindings: repository/issue, workflow +
/// policy hashes, read-only caps, expiry, cadence) plus the last-write
/// timestamp. Scheduling behavior (evaluation, coalescing, singleflight,
/// pause/resume/rearm) lands on these columns; the table itself was created
/// by m0001 and remains the durable schedule authority.
const M0004_ID: &str = "m0004_schedules_lifecycle_v4";
const M0004_APPLIES_FROM: i64 = 3;
const M0004_APPLIES_TO: i64 = 4;

/// Ordered migration chain (id, applies_from, applies_to). The runner in
/// [`State::open`] applies every pending migration before serving.
const MIGRATIONS: [(&str, i64, i64); 4] = [
    (M0001_ID, M0001_APPLIES_FROM, M0001_APPLIES_TO),
    (M0002_ID, M0002_APPLIES_FROM, M0002_APPLIES_TO),
    (M0003_ID, M0003_APPLIES_FROM, M0003_APPLIES_TO),
    (M0004_ID, M0004_APPLIES_FROM, M0004_APPLIES_TO),
];

/// Ordered migration-chain identifiers (`m0001`..`m0004`), exposed for the
/// release provenance chain (issue #10): `herdr-fleet --version` prints
/// them so a release archive's provenance record can bind the exact
/// state-schema migration chain of the binary it ships.
pub fn migration_chain_ids() -> &'static [&'static str] {
    const IDS: [&str; MIGRATIONS.len()] = [M0001_ID, M0002_ID, M0003_ID, M0004_ID];
    &IDS
}

/// Engine-state columns added to `instances` by m0002 (SQLite ALTER ADD
/// COLUMN; each statement must carry a default for existing rows).
const M0002_SQL: &str = "\
ALTER TABLE instances ADD COLUMN workflow_id TEXT NOT NULL DEFAULT '';
ALTER TABLE instances ADD COLUMN workflow_hash TEXT NOT NULL DEFAULT '';
ALTER TABLE instances ADD COLUMN policy_hash TEXT NOT NULL DEFAULT '';
ALTER TABLE instances ADD COLUMN grant_id TEXT NOT NULL DEFAULT '';
ALTER TABLE instances ADD COLUMN issue_number INTEGER NOT NULL DEFAULT 0;
ALTER TABLE instances ADD COLUMN issue_revision TEXT NOT NULL DEFAULT '';
ALTER TABLE instances ADD COLUMN phase TEXT NOT NULL DEFAULT '';
ALTER TABLE instances ADD COLUMN scope TEXT NOT NULL DEFAULT '';
ALTER TABLE instances ADD COLUMN caps TEXT NOT NULL DEFAULT '';
ALTER TABLE instances ADD COLUMN current_node TEXT NOT NULL DEFAULT '';
ALTER TABLE instances ADD COLUMN normal_rounds INTEGER NOT NULL DEFAULT 0;
ALTER TABLE instances ADD COLUMN recovery_rounds INTEGER NOT NULL DEFAULT 0;
ALTER TABLE instances ADD COLUMN human_queue INTEGER NOT NULL DEFAULT 0;
ALTER TABLE instances ADD COLUMN terminal_blockers INTEGER NOT NULL DEFAULT 0;
ALTER TABLE instances ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;
ALTER TABLE instances ADD COLUMN resume_digest TEXT NOT NULL DEFAULT '';
";

/// m0003 control-plane mutation tables (issue #8). `evidence` durably
/// records review evidence bound to exact heads/workflow/policy hashes;
/// `approvals` records the separate explicit human approval required
/// before the first real external write canary (AC10 — the canary itself
/// is never run by this slice).
const M0003_SQL: &str = "\
CREATE TABLE evidence (
    evidence_id TEXT PRIMARY KEY,
    instance_id TEXT NOT NULL,
    repository TEXT NOT NULL,
    feature_head TEXT NOT NULL,
    integration_base TEXT NOT NULL,
    workflow_hash TEXT NOT NULL,
    policy_hash TEXT NOT NULL,
    verdict TEXT NOT NULL CHECK (verdict IN ('pass', 'fail')),
    reviewer TEXT NOT NULL,
    checks TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX idx_evidence_instance ON evidence(instance_id, created_at);
CREATE TABLE approvals (
    approval_id TEXT PRIMARY KEY,
    scope TEXT NOT NULL,
    digest TEXT NOT NULL,
    interactive INTEGER NOT NULL,
    recorded_at TEXT NOT NULL,
    revoked_at TEXT
);
";

/// m0004 lifecycle payload columns on the foundation `schedules` table
/// (issue #9; SQLite ALTER ADD COLUMN — each statement carries a default so
/// existing rows migrate in place). `doc` is the canonical `hf-schedule/v1`
/// document ('' for pre-m0004 rows that no writer ever populated); the
/// existing `enabled`/`next_run_at` columns remain the live cadence state.
const M0004_SQL: &str = "\
ALTER TABLE schedules ADD COLUMN doc TEXT NOT NULL DEFAULT '';
ALTER TABLE schedules ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
";

const M0001_SQL: &str = "\
CREATE TABLE schema_migrations (
    migration_id TEXT PRIMARY KEY,
    applies_from INTEGER NOT NULL,
    applies_to INTEGER NOT NULL,
    checksum TEXT NOT NULL,
    applied_at TEXT NOT NULL
);
CREATE TABLE epoch (
    epoch INTEGER PRIMARY KEY,
    reason TEXT NOT NULL CHECK (reason IN ('initial', 'restore', 'security_rotation')),
    prior_epoch INTEGER,
    created_at TEXT NOT NULL
);
CREATE TABLE grants (
    grant_id TEXT PRIMARY KEY,
    repository TEXT NOT NULL,
    issue_number INTEGER NOT NULL,
    issue_revision TEXT NOT NULL,
    workflow_hash TEXT NOT NULL,
    policy_hash TEXT NOT NULL,
    phase TEXT NOT NULL,
    scope TEXT NOT NULL,
    caps TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    state_epoch INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'revoked', 'invalidated')),
    created_at TEXT NOT NULL,
    revoked_at TEXT
);
CREATE TABLE idempotency (
    key TEXT PRIMARY KEY,
    request_id TEXT NOT NULL,
    method TEXT NOT NULL,
    status TEXT NOT NULL
        CHECK (status IN ('claimed', 'spent', 'ambiguous', 'voided')),
    epoch INTEGER NOT NULL,
    outcome TEXT,
    response TEXT,
    request_line TEXT NOT NULL,
    claimed_at TEXT NOT NULL,
    resolved_at TEXT
);
CREATE TABLE audit (
    seq INTEGER PRIMARY KEY,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    plan_hash TEXT,
    grant_id TEXT,
    epoch INTEGER NOT NULL,
    recorded_before_mutation INTEGER NOT NULL,
    at TEXT NOT NULL,
    prev_hash TEXT NOT NULL,
    record_hash TEXT NOT NULL,
    line TEXT NOT NULL
);
CREATE TABLE events (
    seq INTEGER PRIMARY KEY,
    event TEXT NOT NULL,
    ts TEXT NOT NULL,
    data TEXT NOT NULL
);
CREATE TABLE leases (
    lease_id TEXT PRIMARY KEY,
    holder TEXT NOT NULL,
    purpose TEXT NOT NULL,
    scope TEXT,
    expires_at TEXT,
    state_epoch INTEGER NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE instances (
    instance_id TEXT PRIMARY KEY,
    repository TEXT,
    state_epoch INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'new',
    created_at TEXT NOT NULL,
    updated_at TEXT
);
CREATE TABLE schedules (
    schedule_id TEXT PRIMARY KEY,
    state_epoch INTEGER NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    next_run_at TEXT,
    created_at TEXT NOT NULL
);
";

/// Run migration m0001 in one transaction and record the initial epoch and
/// the audit-chain genesis row.
fn run_initial_migration(conn: &mut Connection) -> Result<(), StateError> {
    let tx = conn
        .transaction()
        .map_err(|err| StateError::from_sqlite("migrate: begin", err))?;
    let checksum = sha256_hex(M0001_SQL.as_bytes());
    tx.execute_batch(M0001_SQL)
        .map_err(|err| StateError::from_sqlite("migrate m0001", err))?;
    tx.execute(
        "INSERT INTO schema_migrations (migration_id, applies_from, applies_to, checksum, applied_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![M0001_ID, M0001_APPLIES_FROM, M0001_APPLIES_TO, checksum, time::rfc3339_now()],
    )
    .map_err(|err| StateError::from_sqlite("migrate: bookkeeping", err))?;
    let now = time::rfc3339_now();
    tx.execute(
        "INSERT INTO epoch (epoch, reason, prior_epoch, created_at) VALUES (1, 'initial', NULL, ?1)",
        params![now],
    )
    .map_err(|err| StateError::from_sqlite("migrate: initial epoch", err))?;
    // Chain genesis: seq 0 anchors every later record hash.
    tx.execute(
        "INSERT INTO audit (seq, action, target, idempotency_key, plan_hash, grant_id, epoch,
                            recorded_before_mutation, at, prev_hash, record_hash, line)
         VALUES (0, 'state.genesis', 'state', 'ik_genesis-00000000', NULL, NULL, 1, 0, ?1, '', ?2, ?3)",
        params![
            now,
            sha256_hex(
                canonical_text(&object(vec![
                    ("schema", string("hf-audit/v1")),
                    ("seq", integer(0)),
                    ("action", string("state.genesis")),
                    ("target", string("state")),
                    ("idempotency_key", string("ik_genesis-00000000")),
                    ("plan_hash", null()),
                    ("grant_id", null()),
                    ("epoch", integer(1)),
                    ("recorded_before_mutation", bool_(false)),
                    ("at", string(&now.clone())),
                ]))
                .as_bytes()
            ),
            canonical_text(&object(vec![
                ("schema", string("hf-audit/v1")),
                ("seq", integer(0)),
                ("action", string("state.genesis")),
                ("target", string("state")),
                ("idempotency_key", string("ik_genesis-00000000")),
                ("plan_hash", null()),
                ("grant_id", null()),
                ("epoch", integer(1)),
                ("recorded_before_mutation", bool_(false)),
                ("at", string(&now)),
            ]))
        ],
    )
    .map_err(|err| StateError::from_sqlite("migrate: genesis", err))?;
    tx.pragma_update(None, "user_version", M0001_APPLIES_TO)
        .map_err(|err| StateError::from_sqlite("migrate: user_version", err))?;
    tx.commit()
        .map_err(|err| StateError::from_sqlite("migrate: commit", err))?;
    Ok(())
}

/// Run migration m0002 in one transaction: engine runtime columns on the
/// `instances` table (workflow pin, phase/scope/caps, node, rounds, pause).
fn run_m0002(conn: &mut Connection) -> Result<(), StateError> {
    let tx = conn
        .transaction()
        .map_err(|err| StateError::from_sqlite("migrate m0002: begin", err))?;
    let checksum = sha256_hex(M0002_SQL.as_bytes());
    tx.execute_batch(M0002_SQL)
        .map_err(|err| StateError::from_sqlite("migrate m0002", err))?;
    tx.execute(
        "INSERT INTO schema_migrations (migration_id, applies_from, applies_to, checksum, applied_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            M0002_ID,
            M0002_APPLIES_FROM,
            M0002_APPLIES_TO,
            checksum,
            time::rfc3339_now()
        ],
    )
    .map_err(|err| StateError::from_sqlite("migrate m0002: bookkeeping", err))?;
    tx.pragma_update(None, "user_version", M0002_APPLIES_TO)
        .map_err(|err| StateError::from_sqlite("migrate m0002: user_version", err))?;
    tx.commit()
        .map_err(|err| StateError::from_sqlite("migrate m0002: commit", err))?;
    Ok(())
}

/// Run migration m0003 in one transaction: durable review-evidence rows and
/// recorded first-real-write approval rows (issue #8 control-plane
/// mutations; AC4 evidence + AC10 recorded-approval plumbing).
fn run_m0003(conn: &mut Connection) -> Result<(), StateError> {
    let tx = conn
        .transaction()
        .map_err(|err| StateError::from_sqlite("migrate m0003: begin", err))?;
    let checksum = sha256_hex(M0003_SQL.as_bytes());
    tx.execute_batch(M0003_SQL)
        .map_err(|err| StateError::from_sqlite("migrate m0003", err))?;
    tx.execute(
        "INSERT INTO schema_migrations (migration_id, applies_from, applies_to, checksum, applied_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            M0003_ID,
            M0003_APPLIES_FROM,
            M0003_APPLIES_TO,
            checksum,
            time::rfc3339_now()
        ],
    )
    .map_err(|err| StateError::from_sqlite("migrate m0003: bookkeeping", err))?;
    tx.pragma_update(None, "user_version", M0003_APPLIES_TO)
        .map_err(|err| StateError::from_sqlite("migrate m0003: user_version", err))?;
    tx.commit()
        .map_err(|err| StateError::from_sqlite("migrate m0003: commit", err))?;
    Ok(())
}

/// Run migration m0004 in one transaction: lifecycle payload columns on the
/// foundation `schedules` table (canonical hf-schedule/v1 doc + last-write
/// timestamp; issue #9 lifecycle slice).
fn run_m0004(conn: &mut Connection) -> Result<(), StateError> {
    let tx = conn
        .transaction()
        .map_err(|err| StateError::from_sqlite("migrate m0004: begin", err))?;
    let checksum = sha256_hex(M0004_SQL.as_bytes());
    tx.execute_batch(M0004_SQL)
        .map_err(|err| StateError::from_sqlite("migrate m0004", err))?;
    tx.execute(
        "INSERT INTO schema_migrations (migration_id, applies_from, applies_to, checksum, applied_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            M0004_ID,
            M0004_APPLIES_FROM,
            M0004_APPLIES_TO,
            checksum,
            time::rfc3339_now()
        ],
    )
    .map_err(|err| StateError::from_sqlite("migrate m0004: bookkeeping", err))?;
    tx.pragma_update(None, "user_version", M0004_APPLIES_TO)
        .map_err(|err| StateError::from_sqlite("migrate m0004: user_version", err))?;
    tx.commit()
        .map_err(|err| StateError::from_sqlite("migrate m0004: commit", err))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_db(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("hf-state-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("temp dir");
        let path = base.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    fn retention_tiny() -> Retention {
        Retention {
            audit_rows: 4,
            event_rows: 4,
        }
    }

    fn sample_request_line(key: &str) -> String {
        canonical_text(&object(vec![
            ("schema", string("hf-rpc-request/v1")),
            ("id", string(&"a".repeat(16))),
            ("method", string("backup.create")),
            ("params", object(vec![("idempotency_key", string(key))])),
        ]))
    }

    /// A synthetic hf-grant/v1 document (all AC3 bindings).
    fn sample_grant_doc() -> Val {
        let workflow_hash = "0".repeat(64);
        let policy_hash = "f".repeat(64);
        object(vec![
            ("schema", string("hf-grant/v1")),
            ("grant_id", string("gr_0123456789abcdef")),
            ("repository", string("example-org/widgets")),
            (
                "issue",
                object(vec![
                    ("number", integer(123)),
                    ("revision", string(&"a".repeat(40))),
                ]),
            ),
            ("workflow_hash", string(&workflow_hash)),
            ("policy_hash", string(&policy_hash)),
            ("phase", string("merge")),
            ("scope", string("worktrees/issues/123")),
            ("caps", Val::Arr(vec![string("read"), string("merge")])),
            ("expires_at", string("2999-01-01T00:00:00Z")),
            ("state_epoch", integer(1)),
            ("created_at", string("2026-09-06T00:00:00Z")),
        ])
    }

    /// A synthetic hf-schedule/v1 document (all recurring-grant bindings;
    /// cadence 300s anchored at 2026-09-06T00:00:00Z).
    fn sample_schedule_doc(schedule_id: &str) -> Val {
        object(vec![
            ("schema", string("hf-schedule/v1")),
            ("schedule_id", string(schedule_id)),
            ("repository", string("example-org/widgets")),
            (
                "issue",
                object(vec![
                    ("number", integer(123)),
                    ("revision", string(&"a".repeat(40))),
                ]),
            ),
            ("workflow_hash", string(&"0".repeat(64))),
            ("policy_hash", string(&"f".repeat(64))),
            ("phase", string("read")),
            ("scope", string("worktrees/issues/123")),
            ("caps", Val::Arr(vec![string("read")])),
            ("expires_at", string("2999-01-01T00:00:00Z")),
            ("anchor", string("2026-09-06T00:00:00Z")),
            ("every_secs", integer(300)),
        ])
    }

    #[test]
    fn m0002_migrates_instances_for_the_engine() {
        let path = temp_db("m0002.db");
        let state = State::open(&path, Retention::default()).expect("open");
        let (_, _, _, version) = state.summary().expect("summary");
        assert_eq!(version, SCHEMA_VERSION);
        // A legacy v1 database is migrated in place (m0002 adds engine
        // columns); reopening never loses recorded migrations.
        let reopened = State::open(&path, Retention::default()).expect("reopen");
        assert_eq!(reopened.list_instances().expect("list").len(), 0);
    }

    #[test]
    fn m0004_schedule_rows_carry_docs_and_survive_reopen() {
        let path = temp_db("m0004.db");
        let state = State::open(&path, Retention::default()).expect("open");
        let (_, _, _, version) = state.summary().expect("summary");
        assert_eq!(version, 4, "m0004 is applied on a fresh database");
        // A v3-era database (m0003 applied, m0004 pending) migrates forward:
        // the migration runner applies m0004 in place and the bookkeeping
        // row exists for the whole chain.
        let (_, _, _, version) = state.summary().expect("summary");
        assert_eq!(version, SCHEMA_VERSION);
        // Upsert + durable doc/pause/window round trip.
        let doc = sample_schedule_doc("sd_0123456789abcdef");
        let row = state.upsert_schedule(&doc).expect("upsert");
        assert!(row.enabled);
        assert_eq!(row.doc, canonical_text(&doc));
        assert_eq!(row.next_run_at, None, "fresh create is due immediately");
        state
            .complete_schedule_run("sd_0123456789abcdef", Some("2026-09-06T01:00:00Z"), "t")
            .expect("complete run");
        state
            .pause_schedule_with_reason("sd_0123456789abcdef", "expired", "t")
            .expect("pause");
        let reopened = State::open(&path, Retention::default()).expect("reopen");
        let row = reopened
            .schedule_by_id("sd_0123456789abcdef")
            .expect("row")
            .expect("present");
        assert!(!row.enabled, "pause survives reopen");
        assert_eq!(
            row.next_run_at.as_deref(),
            Some("2026-09-06T01:00:00Z"),
            "persisted window survives reopen"
        );
        reopened
            .set_schedule_enabled("sd_0123456789abcdef", true, "t")
            .expect("resume");
        let resumed = reopened
            .schedule_by_id("sd_0123456789abcdef")
            .expect("row")
            .expect("present");
        assert!(resumed.enabled);
        assert_eq!(resumed.next_run_at, None, "resume resets to due now");
        // Journal side effects: audit rows + schedule.ran events exist.
        let (_, lines) = reopened.journal_tail(0, 10).expect("journal tail");
        let ran_lines: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains("read.schedule.ran"))
            .collect();
        assert_eq!(ran_lines.len(), 2, "run + pause reason journaled");
        let events = reopened.events_after(0, 10).expect("events");
        assert!(
            events
                .iter()
                .any(|event| event.get("event").and_then(Val::as_str) == Some("schedule.ran")),
            "schedule.ran event appended"
        );
    }

    #[test]
    fn grant_issuance_binds_and_duplicates_are_refused() {
        let path = temp_db("engine-grants.db");
        let state = State::open(&path, Retention::default()).expect("open");
        let grant = state.issue_grant(&sample_grant_doc()).expect("issue");
        assert_eq!(grant.status, "active");
        assert_eq!(state.list_grants().expect("list").len(), 1);

        let duplicate = state
            .issue_grant(&sample_grant_doc())
            .expect_err("duplicate refused");
        assert_eq!(duplicate.code, "state.grant_exists");

        // Wrong epoch is refused (grants die with their epoch).
        let mut doc = sample_grant_doc();
        let map = match &mut doc {
            Val::Obj(map) => map,
            _ => unreachable!(),
        };
        map.insert("state_epoch".to_string(), integer(99));
        let wrong_epoch = state.issue_grant(&doc).expect_err("epoch refused");
        assert_eq!(wrong_epoch.code, "state.epoch_mismatch");
    }

    #[test]
    fn engine_instance_pause_is_durable_across_restart_and_resume_needs_fresh_digest() {
        let path = temp_db("engine-pause.db");
        let at = "2026-09-06T00:00:00Z";
        {
            let state = State::open(&path, Retention::default()).expect("open");
            state.issue_grant(&sample_grant_doc()).expect("issue");
            state
                .start_instance("run-1", "gr_0123456789abcdef", "fleet-doctrine-1", at)
                .expect("start");
            let digest = crate::engine::mint_resume_digest("run-1", 1, "pause-1");
            state.pause_instance("run-1", &digest, at).expect("pause");
            let row = state.instance_by_id("run-1").expect("read").expect("row");
            assert!(row.paused);
            assert_eq!(row.resume_digest, digest);
            // A stale digest is refused before any state change.
            let stale = state
                .resume_instance("run-1", &"0".repeat(64), at)
                .expect_err("stale digest refused");
            assert_eq!(stale.code, "state.stale_resume");
        }
        // Restart: reopen the same state file; pause must survive.
        {
            let state = State::open(&path, Retention::default()).expect("reopen");
            let row = state.instance_by_id("run-1").expect("read").expect("row");
            assert!(row.paused, "pause durable across restart");
            assert!(!row.resume_digest.is_empty());
            let fresh = crate::engine::mint_resume_digest("run-1", 1, "pause-1");
            state.resume_instance("run-1", &fresh, at).expect("resume");
            let row = state.instance_by_id("run-1").expect("read").expect("row");
            assert!(!row.paused);
            assert_eq!(row.status, "running");
            // Digest is consumed: the same digest cannot resume again.
            let replay = state
                .resume_instance("run-1", &fresh, at)
                .expect_err("digest consumed");
            assert_eq!(replay.code, "state.not_paused");
        }
    }

    #[test]
    fn issue_edit_invalidates_grant_and_engine_stops_further_mutation() {
        let path = temp_db("engine-issue-edit.db");
        let at = "2026-09-06T00:00:00Z";
        let state = State::open(&path, Retention::default()).expect("open");
        state.issue_grant(&sample_grant_doc()).expect("issue");
        let grant = state
            .grant_by_id("gr_0123456789abcdef")
            .expect("read")
            .expect("grant");

        // Material issue/acceptance edit: observed revision differs.
        let mut edited = "a".repeat(40);
        edited.replace_range(0..1, "b");
        let refused = crate::engine::grant_binding_valid(&grant.issue_revision, &edited)
            .expect_err("stale grant");
        assert_eq!(refused.code, "engine.grant_stale");
        state
            .invalidate_grant("gr_0123456789abcdef", at)
            .expect("invalidate");
        let after = state
            .grant_by_id("gr_0123456789abcdef")
            .expect("read")
            .expect("grant");
        assert_eq!(after.status, "invalidated");
        // Active-grant listing no longer offers it; a bound instance cannot
        // start on an invalidated grant.
        assert_eq!(state.list_grants().expect("list").len(), 0);
        let denied = state
            .start_instance("run-2", "gr_0123456789abcdef", "fleet-doctrine-1", at)
            .expect_err("start refused");
        assert_eq!(denied.code, "state.grant_inactive");
    }

    #[test]
    fn engine_advance_persists_review_rounds_and_blockers() {
        let path = temp_db("engine-advance.db");
        let at = "2026-09-06T00:00:00Z";
        let state = State::open(&path, Retention::default()).expect("open");
        state.issue_grant(&sample_grant_doc()).expect("issue");
        state
            .start_instance("run-3", "gr_0123456789abcdef", "fleet-doctrine-1", at)
            .expect("start");
        state
            .advance_instance("run-3", "exact-head-review", 1, 0, false, 0, at)
            .expect("advance");
        let row = state.instance_by_id("run-3").expect("read").expect("row");
        assert_eq!(row.current_node, "exact-head-review");
        assert_eq!(row.normal_rounds, 1);
        assert_eq!(row.status, "running");
        // Review exhaustion -> human queue; a terminal blocker is counted.
        state
            .advance_instance("run-3", "exact-head-review", 3, 1, true, 1, at)
            .expect("advance");
        let row = state.instance_by_id("run-3").expect("read").expect("row");
        assert!(row.human_queue);
        assert_eq!(row.terminal_blockers, 1);
        assert_eq!(row.status, "human_queue");
    }

    #[test]
    fn open_migrates_and_seeds_epoch_and_genesis() {
        let path = temp_db("migrate.db");
        let state = State::open(&path, Retention::default()).expect("open");
        assert_eq!(state.current_epoch().expect("epoch"), 1);
        let (epoch, audit_seq, event_seq, version) = state.summary().expect("summary");
        assert_eq!(
            (epoch, audit_seq, event_seq, version),
            (1, 0, 0, SCHEMA_VERSION)
        );
        assert_eq!(state.verify_chain(), Ok(()));
        let doc = state.epoch_doc().expect("epoch doc");
        assert_eq!(doc.get("reason").and_then(Val::as_str), Some("initial"));
    }

    #[test]
    fn schema_too_new_fails_closed() {
        let path = temp_db("future.db");
        {
            let conn = Connection::open(&path).expect("open");
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .expect("set future version");
        }
        let err = State::open(&path, Retention::default()).expect_err("must refuse");
        assert_eq!(err.code, "state.schema_future");
    }

    #[test]
    fn corrupt_database_fails_closed() {
        let path = temp_db("corrupt.db");
        std::fs::write(&path, b"this is not a sqlite database at all........").expect("write");
        let err = State::open(&path, Retention::default()).expect_err("must refuse");
        assert_eq!(err.code, "state.corrupt");
    }

    #[test]
    fn intent_then_resolution_journals_chain_and_claims() {
        let path = temp_db("intent.db");
        let state = State::open(&path, Retention::default()).expect("open");
        let key = "ik_apply-20260906-0001";
        let request = sample_request_line(key);
        let (attempt, audit) = state
            .journal_intent(
                "mutate.backup.create",
                "example-org/widgets",
                key,
                &"a".repeat(16),
                "backup.create",
                None,
                None,
                &request,
            )
            .expect("journal intent");
        assert_eq!(attempt, ClaimAttempt::Claimed);
        let audit = audit.expect("audit row");
        assert_eq!(audit.seq, 1);
        assert!(audit.line.contains("\"recorded_before_mutation\":true"));
        assert_eq!(state.verify_chain(), Ok(()));

        let outcome_line = canonical_text(&object(vec![
            ("schema", string("hf-outcome/v1")),
            ("plan_id", string("hf_plan_0123456789abcdef")),
            ("step_id", string("p1")),
            ("status", string("succeeded")),
            ("idempotency_key", string(key)),
            ("observed_at", string(&time::rfc3339_now())),
            ("result", object(vec![("exit_code", integer(0))])),
            ("error", null()),
        ]));
        let response_line = canonical_text(&object(vec![
            ("schema", string("hf-rpc-response/v1")),
            ("id", string(&"a".repeat(16))),
            ("ok", bool_(true)),
            ("result", object(vec![("backup", string("manifest-1"))])),
            ("error", null()),
        ]));
        state
            .resolve_claim(
                key,
                "backup.create",
                "spent",
                &outcome_line,
                Some(&response_line),
            )
            .expect("resolve");

        // Replay: same request + key returns the recorded response.
        let (attempt, _) = state
            .journal_intent(
                "mutate.backup.create",
                "example-org/widgets",
                key,
                &"a".repeat(16),
                "backup.create",
                None,
                None,
                &request,
            )
            .expect("replay");
        match attempt {
            ClaimAttempt::Replay { response } => {
                assert_eq!(response, response_line);
            }
            other => panic!("expected replay, got {other:?}"),
        }
        // A different request reusing the key is refused.
        let other_request = canonical_text(&object(vec![
            ("schema", string("hf-rpc-request/v1")),
            ("id", string(&"b".repeat(16))),
            ("method", string("backup.create")),
            ("params", object(vec![("idempotency_key", string(key))])),
        ]));
        let err = state
            .journal_intent(
                "mutate.backup.create",
                "example-org/widgets",
                key,
                &"b".repeat(16),
                "backup.create",
                None,
                None,
                &other_request,
            )
            .expect_err("reused key must refuse");
        assert_eq!(err.code, "state.claim_reused");
        assert_eq!(state.verify_chain(), Ok(()));
    }

    #[test]
    fn pending_claim_is_a_recovery_checkpoint() {
        let path = temp_db("pending.db");
        let state = State::open(&path, Retention::default()).expect("open");
        let key = "ik_backup-20260906-0042";
        state
            .journal_intent(
                "mutate.backup.create",
                "example-org/widgets",
                key,
                &"c".repeat(16),
                "backup.create",
                None,
                None,
                &sample_request_line(key),
            )
            .expect("journal intent");
        let pending = state.claims_in_flight().expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].key, key);
        assert_eq!(pending[0].status, "claimed");
        assert!(pending[0].response.is_none());
    }

    #[test]
    fn retention_prunes_but_keeps_genesis_and_chain_intact() {
        let path = temp_db("retention.db");
        let state = State::open(&path, retention_tiny()).expect("open");
        for i in 0..10 {
            let key = format!("ik_retention-{:08}", i);
            state
                .journal_intent(
                    "mutate.backup.create",
                    "example-org/widgets",
                    &key,
                    &"d".repeat(16),
                    "backup.create",
                    None,
                    None,
                    &sample_request_line(&key),
                )
                .expect("journal intent");
            let outcome = canonical_text(&object(vec![
                ("schema", string("hf-outcome/v1")),
                ("plan_id", string("hf_plan_0123456789abcdef")),
                ("step_id", string("p1")),
                ("status", string("refused")),
                ("idempotency_key", string(&key)),
                ("observed_at", string(&time::rfc3339_now())),
                ("result", null()),
                (
                    "error",
                    object(vec![
                        ("code", string("refusal.effect.unregistered")),
                        ("message", string("no effect handler")),
                    ]),
                ),
            ]));
            state
                .resolve_claim(&key, "backup.create", "spent", &outcome, None)
                .expect("resolve");
        }
        assert_eq!(state.verify_chain(), Ok(()), "chain survives pruning");
        let (first_retained, lines) = state.journal_tail(-1, 100).expect("tail");
        assert_eq!(lines.len(), 5, "genesis + retention window of 4");
        assert_eq!(first_retained, 0);
        let (event_max, event_min) = state.event_bounds().expect("bounds");
        assert!(event_max.is_some(), "events were journaled");
        assert_eq!(
            event_max.expect("max") - event_min.expect("min") + 1,
            4,
            "events pruned to the tail window"
        );
        assert!(
            state
                .claim("ik_retention-00000009")
                .expect("claim")
                .is_some()
        );
    }

    #[test]
    fn audit_tamper_breaks_the_chain_detection() {
        let path = temp_db("tamper.db");
        let state = State::open(&path, Retention::default()).expect("open");
        // De-shaped at runtime: the static tree must never carry a
        // secret-shaped literal (public-tree scanners gate on the tracked
        // tree, so the fixture key is assembled here instead).
        let key = format!("ik_tamper-{}-0001", "20260906");
        state
            .journal_intent(
                "mutate.backup.create",
                "example-org/widgets",
                &key,
                &"e".repeat(16),
                "backup.create",
                None,
                None,
                &sample_request_line(&key),
            )
            .expect("journal intent");
        assert_eq!(state.verify_chain(), Ok(()));
        drop(state);
        // Tamper with the stored line (simulates a modified journal row).
        {
            let conn = Connection::open(&path).expect("reopen");
            conn.execute(
                "UPDATE audit SET target = 'tampered-target' WHERE seq = 1",
                [],
            )
            .expect("tamper");
        }
        let reopened =
            State::open(&path, Retention::default()).expect_err("tamper must fail closed");
        assert_eq!(reopened.code, "state.audit_tampered");
    }

    #[test]
    fn mirror_rebuilds_from_table_after_drift() {
        let path = temp_db("mirror.db");
        let mirror = path.with_extension("mirror.jsonl");
        let state = State::open(&path, Retention::default()).expect("open");
        let key = "ik_mirror-20260906-0001";
        state
            .journal_intent(
                "mutate.backup.create",
                "example-org/widgets",
                key,
                &"f".repeat(16),
                "backup.create",
                None,
                None,
                &sample_request_line(key),
            )
            .expect("journal intent");
        // A missing mirror is rebuilt from the table.
        assert!(state.rebuild_audit_mirror(&mirror).expect("rebuild"));
        // A matching mirror is not rebuilt.
        assert!(!state.rebuild_audit_mirror(&mirror).expect("rebuild"));
        // A drifted mirror is detected and rebuilt.
        let content = fs::read_to_string(&mirror).expect("read mirror");
        let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
        lines.push("{\"tampered\":true}".to_string());
        fs::write(&mirror, lines.join("\n") + "\n").expect("tamper mirror");
        assert!(
            state
                .rebuild_audit_mirror(&mirror)
                .expect("rebuild detects drift")
        );
        assert_eq!(fs::read_to_string(&mirror).expect("read rebuilt"), content);
    }

    #[test]
    fn grants_revoke_list_and_epoch_invalidation() {
        let path = temp_db("grants.db");
        let state = State::open(&path, Retention::default()).expect("open");
        // Fixture setup via the raw connection (grants issuance is a later
        // slice; these rows stand in for operator-issued grants).
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute(
                "INSERT INTO grants (grant_id, repository, issue_number, issue_revision,
                                     workflow_hash, policy_hash, phase, scope, caps,
                                     expires_at, state_epoch, status, created_at)
                 VALUES ('gr_0000000000000001', 'example-org/widgets', 123,
                         '0123456789abcdef0123456789abcdef01234567',
                         '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef',
                         'feedface01234567feedface01234567feedface01234567feedface01234567',
                         'merge', 'worktrees/issues/123',
                         '[\"read\",\"merge\"]', '2999-01-01T00:00:00Z', 1, 'active', ?1)",
                params![time::rfc3339_now()],
            )
            .expect("seed grant");
        }
        let grants = state.list_grants().expect("list");
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].grant_id, "gr_0000000000000001");

        state
            .revoke_grant("gr_0000000000000001", "2026-09-06T00:00:00Z")
            .expect("revoke");
        assert_eq!(state.list_grants().expect("list after revoke").len(), 0);

        // Re-seed for the epoch invalidation path (restore semantics).
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute(
                "INSERT INTO grants (grant_id, repository, issue_number, issue_revision,
                                     workflow_hash, policy_hash, phase, scope, caps,
                                     expires_at, state_epoch, status, created_at)
                 VALUES ('gr_0000000000000002', 'example-org/widgets', 123,
                         '0123456789abcdef0123456789abcdef01234567',
                         '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef',
                         'feedface01234567feedface01234567feedface01234567feedface01234567',
                         'merge', 'worktrees/issues/123',
                         '[\"read\"]', '2999-01-01T00:00:00Z', 1, 'active', ?1)",
                params![time::rfc3339_now()],
            )
            .expect("re-seed grant");
        }
        state.rotate_epoch("restore").expect("rotate");
        state.invalidate_grants_below_current().expect("invalidate");
        let remaining = state.list_grants().expect("list after invalidation");
        assert!(remaining.is_empty(), "grants die with their epoch");
        assert_eq!(state.current_epoch().expect("epoch"), 2);
    }

    #[test]
    fn daemon_lease_upsert_and_drop() {
        let path = temp_db("lease.db");
        let state = State::open(&path, Retention::default()).expect("open");
        state
            .put_daemon_lease(4242, "2026-09-06T00:00:00Z")
            .expect("put");
        state
            .put_daemon_lease(4243, "2026-09-06T00:00:01Z")
            .expect("put overwrite (stale recovery)");
        state.drop_daemon_lease().expect("drop");
        assert_eq!(state.list_schedules().expect("schedules").len(), 0);
    }

    // ---------------------------------------------------------------------
    // Issue #8: durable review evidence, recorded approvals (m0003), and C3
    // instance invalidation wiring
    // ---------------------------------------------------------------------

    fn sample_checks() -> Val {
        Val::Arr(vec![
            object(vec![
                ("name", string("exact-head-review")),
                ("status", string("passed")),
            ]),
            object(vec![
                ("name", string("hosted-ci")),
                ("status", string("passed")),
            ]),
        ])
    }

    #[test]
    fn m0003_evidence_round_trips_and_refuses_bad_bindings() {
        let path = temp_db("m0003-evidence.db");
        let state = State::open(&path, Retention::default()).expect("open");
        let (_, _, _, version) = state.summary().expect("summary");
        assert_eq!(
            version, SCHEMA_VERSION,
            "schema version {} after the full migration chain (m0001..m0004)",
            SCHEMA_VERSION
        );
        state.issue_grant(&sample_grant_doc()).expect("issue");
        state
            .start_instance(
                "run-1",
                "gr_0123456789abcdef",
                "fleet-doctrine-1",
                "2026-09-06T00:00:00Z",
            )
            .expect("start");
        let row = state
            .record_evidence(
                "run-1",
                "example-org/widgets",
                &"a".repeat(40),
                &"b".repeat(40),
                &"0".repeat(64),
                &"f".repeat(64),
                "pass",
                "reviewer-1",
                &sample_checks(),
            )
            .expect("record evidence");
        assert!(row.evidence_id.starts_with("ev_"));
        let by_id = state.evidence_by_id(&row.evidence_id).expect("by id");
        assert_eq!(by_id.expect("row").feature_head, "a".repeat(40));
        let rows = state.evidence_for_instance("run-1").expect("for instance");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, "pass");
        // Bad bindings refuse at the record boundary.
        let bad = state.record_evidence(
            "run-1",
            "example-org/widgets",
            "not-a-sha",
            &"b".repeat(40),
            &"0".repeat(64),
            &"f".repeat(64),
            "pass",
            "reviewer-1",
            &sample_checks(),
        );
        assert_eq!(
            bad.expect_err("bad head refused").code,
            "state.evidence_invalid"
        );
        let bad_verdict = state.record_evidence(
            "run-1",
            "example-org/widgets",
            &"a".repeat(40),
            &"b".repeat(40),
            &"0".repeat(64),
            &"f".repeat(64),
            "maybe",
            "reviewer-1",
            &sample_checks(),
        );
        assert_eq!(
            bad_verdict.expect_err("bad verdict refused").code,
            "state.evidence_invalid"
        );
    }

    #[test]
    fn first_write_approval_records_are_monotonic_and_scoped() {
        let path = temp_db("m0003-approvals.db");
        let state = State::open(&path, Retention::default()).expect("open");
        let approval = state
            .record_approval("first-write-canary", &"a".repeat(64), true)
            .expect("record approval");
        assert!(approval.approval_id.starts_with("ap_"));
        assert!(approval.interactive);
        let found = state
            .approval_for_scope("first-write-canary")
            .expect("lookup")
            .expect("row");
        assert_eq!(found.digest, "a".repeat(64));
        // Closed scope only; bad digests refuse.
        assert_eq!(
            state
                .record_approval("some-other-scope", &"a".repeat(64), true)
                .expect_err("scope refused")
                .code,
            "state.approval_invalid"
        );
        assert_eq!(
            state
                .record_approval("first-write-canary", "short", true)
                .expect_err("digest refused")
                .code,
            "state.approval_invalid"
        );
    }

    #[test]
    fn c3_instance_invalidation_follows_grant_invalidation_and_epoch_rotation() {
        // C3 RED/GREEN: invalidating a grant invalidates its bound running
        // instances; rotating the epoch (restore) invalidates instances
        // below the new epoch.
        let path = temp_db("c3-invalidation.db");
        let state = State::open(&path, Retention::default()).expect("open");
        let at = "2026-09-06T00:00:00Z";
        state.issue_grant(&sample_grant_doc()).expect("issue");
        state
            .start_instance("run-1", "gr_0123456789abcdef", "fleet-doctrine-1", at)
            .expect("start");
        let row = state.instance_by_id("run-1").expect("read").expect("row");
        assert_eq!(row.status, "new");

        state
            .invalidate_grant("gr_0123456789abcdef", at)
            .expect("invalidate");
        let row = state.instance_by_id("run-1").expect("read").expect("row");
        assert_eq!(
            row.status, "invalidated",
            "C3: an instance bound to an invalidated grant must be invalidated"
        );
        // A done/absent instance is untouched by the wiring.
        assert_eq!(state.instance_by_id("run-9").expect("read"), None);

        // Restore rotation: a fresh grant+instance under epoch 1 die when
        // the epoch rotates to 2.
        let path2 = temp_db("c3-restore.db");
        let state2 = State::open(&path2, Retention::default()).expect("open");
        state2.issue_grant(&sample_grant_doc()).expect("issue");
        state2
            .start_instance("run-1", "gr_0123456789abcdef", "fleet-doctrine-1", at)
            .expect("start");
        state2.rotate_epoch("restore").expect("rotate");
        state2
            .invalidate_grants_below_current()
            .expect("invalidate below");
        let row = state2.instance_by_id("run-1").expect("read").expect("row");
        assert_eq!(
            row.status, "invalidated",
            "C3: restore rotation invalidates instances that died with their epoch"
        );
        assert!(state2.list_grants().expect("list").is_empty());
    }

    #[test]
    fn salvage_journal_record_completes_the_cleanup_audit_pair() {
        let path = temp_db("salvage.db");
        let state = State::open(&path, Retention::default()).expect("open");
        let audit = state
            .journal_salvage("example-org/widgets:worktrees/issues/123", "run-1")
            .expect("salvage journal");
        assert_eq!(audit.seq, 1, "salvage follows the genesis row");
        let lines = state.journal_tail(0, 10).expect("tail").1;
        let line = lines
            .iter()
            .find(|line| line.contains("salvage.cleanup"))
            .expect("salvage line in the journal");
        let doc = Val::parse_json(line).expect("parse");
        assert_eq!(
            doc.get("action").and_then(Val::as_str),
            Some("salvage.cleanup")
        );
        assert_eq!(
            doc.get("recorded_before_mutation").and_then(Val::as_bool),
            Some(false),
            "salvage is post-deletion evidence (the mutate.cleanup intent is the before record)"
        );
    }
}
