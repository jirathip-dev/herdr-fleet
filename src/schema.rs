//! #3 schema-family validators (closed v1 surfaces).
//!
//! This module is the Rust mirror of the machine-readable half of the
//! contract — `scripts/check-contract-fixtures.py` — for exactly the seven
//! families this read-only slice emits or consumes: `hf-config/v1`,
//! `hf-policy/v1` (TOML), `hf-output/v1`, `hf-error/v1`,
//! `hf-observation/v1`, `hf-plan/v1`, and `hf-capability/v1` (JSON).
//!
//! Verdict semantics mirror the probe: `refuse-parse`, `refuse-schema`
//! (missing/foreign identifier), `refuse-version` (known family, unsupported
//! version), `refuse-malformed` (closed-surface violations), and
//! `refuse-noncanonical` (valid but not canonical bytes, `hf-plan/v1` only).
//! The unit tests in this module run the committed fixture corpus from
//! `schemas/fixtures/` through these validators and assert every manifest
//! expectation holds, so the emitted JSON of every command is covered by the
//! same discriminating rules the #3 probe enforces on fixtures.

use crate::canonical::canonical_bytes;
use crate::formats::{
    is_actor, is_error_code, is_hex40, is_hex64, is_plan_id, is_repository_identity,
    is_rfc3339_seconds_z, is_slug,
};
use crate::value::Val;

/// A contract family handled by this slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// `hf-config/v1` — canonical XDG TOML configuration.
    Config,
    /// `hf-policy/v1` — explicit optional policy overlay.
    Policy,
    /// `hf-output/v1` — CLI JSON output envelope.
    Output,
    /// `hf-error/v1` — typed error envelope.
    Error,
    /// `hf-observation/v1` — normalized observation.
    Observation,
    /// `hf-plan/v1` — deterministic plan (canonical bytes + digest).
    Plan,
    /// `hf-capability/v1` — capability negotiation envelope.
    Capability,
}

impl Family {
    /// The registry family name (`hf-config`, …).
    pub fn family_name(self) -> &'static str {
        match self {
            Family::Config => "hf-config",
            Family::Policy => "hf-policy",
            Family::Output => "hf-output",
            Family::Error => "hf-error",
            Family::Observation => "hf-observation",
            Family::Plan => "hf-plan",
            Family::Capability => "hf-capability",
        }
    }

    /// The versioned schema identifier (`hf-config/v1`, …).
    pub fn schema_id(self) -> &'static str {
        match self {
            Family::Config => "hf-config/v1",
            Family::Policy => "hf-policy/v1",
            Family::Output => "hf-output/v1",
            Family::Error => "hf-error/v1",
            Family::Observation => "hf-observation/v1",
            Family::Plan => "hf-plan/v1",
            Family::Capability => "hf-capability/v1",
        }
    }

    /// Parse document bytes into a [`Val`]: TOML for config/policy, strict
    /// JSON for every other family.
    fn parse_bytes(self, bytes: &[u8]) -> Result<Val, String> {
        let text = std::str::from_utf8(bytes).map_err(|err| format!("not valid UTF-8 ({err})"))?;
        match self {
            Family::Config | Family::Policy => Val::parse_toml(text),
            _ => Val::parse_json(text),
        }
    }
}

/// Refusal classes, matching the probe's result vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Bytes do not parse.
    Parse,
    /// Schema identifier is missing or names another family.
    Schema,
    /// Family is known but the version is not supported.
    Version,
    /// Structurally invalid for the family rules.
    Malformed,
    /// Bytes are not the canonical serialization.
    Noncanonical,
}

impl Refusal {
    /// The probe's refusal-code spelling (`refuse-parse`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::Parse => "refuse-parse",
            Refusal::Schema => "refuse-schema",
            Refusal::Version => "refuse-version",
            Refusal::Malformed => "refuse-malformed",
            Refusal::Noncanonical => "refuse-noncanonical",
        }
    }
}

/// One validation outcome: accepted, or refused with a class and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    refusal: Option<Refusal>,
    message: String,
}

impl Verdict {
    /// Accepted.
    pub fn accept() -> Verdict {
        Verdict {
            refusal: None,
            message: String::new(),
        }
    }

    /// Refused with a class and human message.
    pub fn refuse(class: Refusal, message: impl Into<String>) -> Verdict {
        Verdict {
            refusal: Some(class),
            message: message.into(),
        }
    }

    /// Whether the document was accepted.
    pub fn is_accepted(&self) -> bool {
        self.refusal.is_none()
    }

    /// The refusal class of a refused document.
    pub fn refusal(&self) -> Option<Refusal> {
        self.refusal
    }

    /// Human message (acceptance or refusal reason).
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Validate raw document bytes for a family (parse + version + shape +
/// canonical-bytes rules). This is the entry point used by the CLI's config
/// commands and by the fixture-corpus tests.
pub fn validate_bytes(family: Family, bytes: &[u8]) -> Verdict {
    let doc = match family.parse_bytes(bytes) {
        Ok(doc) => doc,
        Err(message) => return Verdict::refuse(Refusal::Parse, message),
    };
    let verdict = validate_doc(family, &doc);
    if verdict.refusal.is_some() {
        return verdict;
    }
    if family == Family::Plan {
        let canonical = canonical_bytes(&doc);
        let without_lf = &canonical[..canonical.len() - 1];
        if bytes != canonical.as_slice() && bytes != without_lf {
            return Verdict::refuse(
                Refusal::Noncanonical,
                "bytes are not the canonical serialization",
            );
        }
    }
    Verdict::accept()
}

/// Validate an already-parsed document value for a family.
pub fn validate_doc(family: Family, doc: &Val) -> Verdict {
    match family {
        Family::Config => validate_config(doc),
        Family::Policy => validate_policy(doc),
        Family::Output => validate_output(doc),
        Family::Error => validate_error(doc),
        Family::Observation => validate_observation(doc),
        Family::Plan => validate_plan(doc),
        Family::Capability => validate_capability(doc),
    }
}

// ---------------------------------------------------------------------------
// Shared shape helpers (each mirrors the probe's `_expect_*` helpers).
// ---------------------------------------------------------------------------

fn check_schema(obj: &Val, family: Family) -> Result<(), Verdict> {
    let Some(value) = obj.get("schema") else {
        return Err(Verdict::refuse(
            Refusal::Schema,
            "missing schema identifier",
        ));
    };
    let Some(text) = value.as_str() else {
        return Err(Verdict::refuse(
            Refusal::Schema,
            "schema identifier must be a string",
        ));
    };
    if text == family.schema_id() {
        return Ok(());
    }
    let prefix = format!("{}/v", family.family_name());
    if text.starts_with(&prefix) {
        Err(Verdict::refuse(
            Refusal::Version,
            format!("unsupported version {text:?} for {}", family.family_name()),
        ))
    } else {
        Err(Verdict::refuse(
            Refusal::Schema,
            format!("missing or foreign schema identifier {text:?}"),
        ))
    }
}

type Rule = Result<(), Verdict>;

fn require_keys(obj: &Val, required: &[&str], allowed: &[&str], where_: &str) -> Rule {
    for key in required {
        if obj.get(key).is_none() {
            return Err(Verdict::refuse(
                Refusal::Malformed,
                format!("{where_}: missing required key {key:?}"),
            ));
        }
    }
    if let Val::Obj(map) = obj {
        for key in map.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(Verdict::refuse(
                    Refusal::Malformed,
                    format!("{where_}: unknown key {key:?}"),
                ));
            }
        }
    }
    Ok(())
}

fn table(obj: &Val, where_: &str) -> Rule {
    match obj {
        Val::Obj(_) => Ok(()),
        other => Err(Verdict::refuse(
            Refusal::Malformed,
            format!("{where_} must be a table/object, got {}", other.type_name()),
        )),
    }
}

fn expect_str(obj: &Val, key: &str, where_: &str, check: Option<fn(&str) -> bool>) -> Rule {
    let Some(value) = obj.get(key) else {
        return Ok(());
    };
    match value {
        Val::Str(text) if !text.is_empty() => {
            if let Some(check) = check
                && !check(text)
            {
                return Err(Verdict::refuse(
                    Refusal::Malformed,
                    format!("{where_}: {key:?} fails its format rule"),
                ));
            }
            Ok(())
        }
        other => Err(Verdict::refuse(
            Refusal::Malformed,
            format!(
                "{where_}: {key:?} must be a non-empty string, got {}",
                other.type_name()
            ),
        )),
    }
}

fn expect_bool(obj: &Val, key: &str, where_: &str) -> Rule {
    match obj.get(key) {
        None | Some(Val::Bool(_)) => Ok(()),
        Some(other) => Err(Verdict::refuse(
            Refusal::Malformed,
            format!(
                "{where_}: {key:?} must be a boolean, got {}",
                other.type_name()
            ),
        )),
    }
}

fn expect_int(obj: &Val, key: &str, where_: &str, minimum: i64) -> Rule {
    match obj.get(key) {
        Some(Val::Int(int)) if *int >= minimum => Ok(()),
        Some(other) => Err(Verdict::refuse(
            Refusal::Malformed,
            format!(
                "{where_}: {key:?} must be an integer >= {minimum}, got {}",
                other.type_name()
            ),
        )),
        None => Err(Verdict::refuse(
            Refusal::Malformed,
            format!("{where_}: missing required integer {key:?}"),
        )),
    }
}

fn expect_timestamp(obj: &Val, key: &str, where_: &str) -> Rule {
    match obj.get(key) {
        Some(Val::Str(text)) if is_rfc3339_seconds_z(text) => Ok(()),
        Some(other) => Err(Verdict::refuse(
            Refusal::Malformed,
            format!(
                "{where_}: {key:?} must be RFC3339 UTC (seconds, Z), got {}",
                other.type_name()
            ),
        )),
        None => Err(Verdict::refuse(
            Refusal::Malformed,
            format!("{where_}: missing required timestamp {key:?}"),
        )),
    }
}

fn expect_in(obj: &Val, key: &str, where_: &str, closed: &[&str]) -> Rule {
    match obj.get(key) {
        Some(Val::Str(text)) if closed.contains(&text.as_str()) => Ok(()),
        Some(other) => Err(Verdict::refuse(
            Refusal::Malformed,
            format!(
                "{where_}: {key:?} must be one of the closed set {closed:?}, got {}",
                other.type_name()
            ),
        )),
        None => Err(Verdict::refuse(
            Refusal::Malformed,
            format!("{where_}: missing required key {key:?}"),
        )),
    }
}

/// Embedded error shape `{code, message}` used by output envelopes.
fn err_shaped(value: &Val) -> Rule {
    table(value, "embedded error")?;
    match value.get("code") {
        Some(Val::Str(text)) if is_error_code(text) => {}
        _ => {
            return Err(Verdict::refuse(
                Refusal::Malformed,
                "embedded error code invalid",
            ));
        }
    }
    match value.get("message") {
        Some(Val::Str(text)) if !text.is_empty() => {}
        _ => {
            return Err(Verdict::refuse(
                Refusal::Malformed,
                "embedded error message must be non-empty",
            ));
        }
    }
    Ok(())
}

/// Issue binding `{number: positive int, revision: 40-hex}`.
fn issue_shaped(value: &Val) -> Rule {
    table(value, "issue")?;
    match value.get("number") {
        Some(Val::Int(number)) if *number > 0 => {}
        _ => {
            return Err(Verdict::refuse(
                Refusal::Malformed,
                "issue.number must be a positive integer",
            ));
        }
    }
    expect_str(value, "revision", "issue", Some(is_hex40))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-family validators (normative; the docs under docs/contracts/ must not
// contradict these rules).
// ---------------------------------------------------------------------------

const CONFIG_TOP_KEYS: [&str; 7] = [
    "schema",
    "daemon",
    "policy",
    "repository",
    "harness",
    "workflow",
    "role",
];
const POLICY_TOP_KEYS: [&str; 4] = ["schema", "repositories", "production_confirmation", "role"];

fn validate_config(obj: &Val) -> Verdict {
    if let Err(verdict) = check_schema(obj, Family::Config) {
        return verdict;
    }
    if let Err(verdict) = require_keys(obj, &["schema"], &CONFIG_TOP_KEYS, "config") {
        return verdict;
    }
    for key in [
        "daemon",
        "policy",
        "repository",
        "harness",
        "workflow",
        "role",
    ] {
        if obj.get(key).is_some()
            && let Err(verdict) = table(obj.get(key).expect("present"), &format!("config.{key}"))
        {
            return verdict;
        }
    }
    // daemon: {enabled?: bool, socket?: string}
    if let Some(daemon) = obj.get("daemon") {
        if let Err(verdict) = require_keys(daemon, &[], &["enabled", "socket"], "config.daemon") {
            return verdict;
        }
        if let Err(verdict) = expect_bool(daemon, "enabled", "config.daemon") {
            return verdict;
        }
        if let Err(verdict) = expect_str(daemon, "socket", "config.daemon", None) {
            return verdict;
        }
    }
    // policy: exactly {overlay: non-empty string}
    if let Some(policy) = obj.get("policy") {
        if let Err(verdict) = require_keys(policy, &["overlay"], &["overlay"], "config.policy") {
            return verdict;
        }
        if let Err(verdict) = expect_str(policy, "overlay", "config.policy", None) {
            return verdict;
        }
    }
    // repository.<slug>: {origin: string required, branch?: string, enabled?: bool}
    if let Some(repositories) = obj.get("repository") {
        let Val::Obj(entries) = repositories else {
            return Verdict::refuse(Refusal::Malformed, "config.repository must be a table");
        };
        for (name, entry) in entries {
            if !is_slug(name) {
                return Verdict::refuse(
                    Refusal::Malformed,
                    format!("config.repository key {name:?} invalid (slug required)"),
                );
            }
            let where_ = format!("config.repository.{name}");
            if let Err(verdict) = table(entry, &where_) {
                return verdict;
            }
            if let Err(verdict) = require_keys(
                entry,
                &["origin"],
                &["origin", "branch", "enabled"],
                &where_,
            ) {
                return verdict;
            }
            if let Err(verdict) = expect_str(entry, "origin", &where_, None) {
                return verdict;
            }
            // Normative-table strictness: branch must be a string and enabled
            // a boolean when present (the fixture probe checks origin only;
            // these types come from docs/contracts/spec-config.md).
            if let Err(verdict) = expect_str(entry, "branch", &where_, None) {
                return verdict;
            }
            if let Err(verdict) = expect_bool(entry, "enabled", &where_) {
                return verdict;
            }
        }
    }
    // harness.<key>: exactly {kind, executable, env_allow}
    if let Some(harnesses) = obj.get("harness") {
        let Val::Obj(entries) = harnesses else {
            return Verdict::refuse(Refusal::Malformed, "config.harness must be a table");
        };
        for (name, entry) in entries {
            let where_ = format!("config.harness.{name}");
            if let Err(verdict) = table(entry, &where_) {
                return verdict;
            }
            if let Err(verdict) = require_keys(
                entry,
                &["kind", "executable", "env_allow"],
                &["kind", "executable", "env_allow"],
                &where_,
            ) {
                return verdict;
            }
            if let Err(verdict) = expect_str(entry, "kind", &where_, None) {
                return verdict;
            }
            if let Err(verdict) = expect_str(entry, "executable", &where_, None) {
                return verdict;
            }
            match entry.get("env_allow") {
                Some(Val::Arr(items)) => {
                    for item in items {
                        if !matches!(item, Val::Str(_)) {
                            return Verdict::refuse(
                                Refusal::Malformed,
                                format!("{where_}.env_allow must be a [string]"),
                            );
                        }
                    }
                }
                Some(other) => {
                    return Verdict::refuse(
                        Refusal::Malformed,
                        format!(
                            "{where_}.env_allow must be an array of strings, got {}",
                            other.type_name()
                        ),
                    );
                }
                None => unreachable!("require_keys enforced env_allow"),
            }
        }
    }
    // workflow.<key>: {id: non-empty string, hash: 64-hex}; role.<key>: {hash}
    for (table_key, allowed) in [("workflow", &["id", "hash"][..]), ("role", &["hash"][..])] {
        if let Some(entries) = obj.get(table_key) {
            let Val::Obj(map) = entries else {
                return Verdict::refuse(
                    Refusal::Malformed,
                    format!("config.{table_key} must be a table"),
                );
            };
            for (name, entry) in map {
                let where_ = format!("config.{table_key}.{name}");
                if let Err(verdict) = table(entry, &where_) {
                    return verdict;
                }
                if let Err(verdict) = require_keys(entry, allowed, allowed, &where_) {
                    return verdict;
                }
                if let Err(verdict) = expect_str(entry, "id", &where_, None) {
                    return verdict;
                }
                if let Err(verdict) = expect_str(entry, "hash", &where_, Some(is_hex64)) {
                    return verdict;
                }
            }
        }
    }
    Verdict::accept()
}

fn validate_policy(obj: &Val) -> Verdict {
    if let Err(verdict) = check_schema(obj, Family::Policy) {
        return verdict;
    }
    if let Err(verdict) = require_keys(obj, &["schema"], &POLICY_TOP_KEYS, "policy") {
        return verdict;
    }
    let constraining = ["repositories", "production_confirmation", "role"]
        .iter()
        .any(|key| obj.get(key).is_some());
    if !constraining {
        return Verdict::refuse(
            Refusal::Malformed,
            "policy: overlay must constrain at least one axis",
        );
    }
    if let Some(repositories) = obj.get("repositories") {
        match repositories {
            Val::Arr(items) if !items.is_empty() => {
                for item in items {
                    let ok = matches!(item, Val::Str(text) if is_repository_identity(text));
                    if !ok {
                        return Verdict::refuse(
                            Refusal::Malformed,
                            "policy.repositories must be a non-empty [owner/name]",
                        );
                    }
                }
            }
            _ => {
                return Verdict::refuse(
                    Refusal::Malformed,
                    "policy.repositories must be a non-empty [owner/name]",
                );
            }
        }
    }
    if let Some(confirmation) = obj.get("production_confirmation") {
        match confirmation {
            Val::Str(text) if text == "tty" || text == "deny" => {}
            other => {
                return Verdict::refuse(
                    Refusal::Malformed,
                    format!(
                        "policy.production_confirmation must be one of tty|deny (overlay may only tighten), got {}",
                        other.type_name()
                    ),
                );
            }
        }
    }
    if let Some(roles) = obj.get("role") {
        let Val::Obj(map) = roles else {
            return Verdict::refuse(Refusal::Malformed, "policy.role must be a table");
        };
        for (name, entry) in map {
            let where_ = format!("policy.role.{name}");
            if let Err(verdict) = table(entry, &where_) {
                return verdict;
            }
            if let Err(verdict) = require_keys(entry, &["hash"], &["hash"], &where_) {
                return verdict;
            }
            if let Err(verdict) = expect_str(entry, "hash", &where_, Some(is_hex64)) {
                return verdict;
            }
        }
    }
    Verdict::accept()
}

fn validate_output(obj: &Val) -> Verdict {
    if let Err(verdict) = check_schema(obj, Family::Output) {
        return verdict;
    }
    if let Err(verdict) = require_keys(
        obj,
        &["schema", "command", "kind", "exit_code"],
        &["schema", "command", "kind", "exit_code", "data", "error"],
        "output",
    ) {
        return verdict;
    }
    if let Err(verdict) = expect_str(obj, "command", "output", None) {
        return verdict;
    }
    if let Err(verdict) = expect_in(obj, "kind", "output", &["ok", "error", "partial"]) {
        return verdict;
    }

    match obj.get("exit_code") {
        Some(Val::Int(code)) if (0..=255).contains(code) => {}
        Some(other) => {
            return Verdict::refuse(
                Refusal::Malformed,
                format!(
                    "output.exit_code must be an int in 0..=255, got {}",
                    other.type_name()
                ),
            );
        }
        None => {
            return Verdict::refuse(Refusal::Malformed, "output: missing required exit_code");
        }
    }
    match obj.get("kind") {
        Some(Val::Str(kind)) if kind == "error" => {
            let Some(error) = obj.get("error") else {
                return Verdict::refuse(
                    Refusal::Malformed,
                    "output: error kind requires an error object",
                );
            };
            if let Err(verdict) = err_shaped(error) {
                return verdict;
            }
        }
        _ => {
            let Some(data) = obj.get("data") else {
                return Verdict::refuse(
                    Refusal::Malformed,
                    "output: ok|partial kind requires a data object",
                );
            };
            if let Err(verdict) = table(data, "output.data") {
                return verdict;
            }
        }
    }
    Verdict::accept()
}

fn validate_error(obj: &Val) -> Verdict {
    if let Err(verdict) = check_schema(obj, Family::Error) {
        return verdict;
    }
    let allowed = ["schema", "code", "message", "retryable", "details"];
    if let Err(verdict) = require_keys(obj, &allowed, &allowed, "error") {
        return verdict;
    }
    if let Err(verdict) = expect_str(obj, "code", "error", Some(is_error_code)) {
        return verdict;
    }
    if let Err(verdict) = expect_str(obj, "message", "error", None) {
        return verdict;
    }
    match obj.get("retryable") {
        Some(Val::Bool(_)) => {}
        Some(other) => {
            return Verdict::refuse(
                Refusal::Malformed,
                format!(
                    "error.retryable must be a boolean, got {}",
                    other.type_name()
                ),
            );
        }
        None => {
            return Verdict::refuse(Refusal::Malformed, "error: missing required retryable");
        }
    }
    match obj.get("details") {
        None | Some(Val::Null) | Some(Val::Obj(_)) => {}
        Some(other) => {
            return Verdict::refuse(
                Refusal::Malformed,
                format!(
                    "error.details must be an object or null, got {}",
                    other.type_name()
                ),
            );
        }
    }
    Verdict::accept()
}

fn validate_observation(obj: &Val) -> Verdict {
    if let Err(verdict) = check_schema(obj, Family::Observation) {
        return verdict;
    }
    if let Err(verdict) = require_keys(
        obj,
        &[
            "schema",
            "subject",
            "observed_at",
            "freshness",
            "completeness",
        ],
        &[
            "schema",
            "subject",
            "observed_at",
            "freshness",
            "completeness",
            "payload",
        ],
        "observation",
    ) {
        return verdict;
    }
    let Some(subject) = obj.get("subject") else {
        return Verdict::refuse(Refusal::Malformed, "observation: missing required subject");
    };
    if let Err(verdict) = table(subject, "observation.subject") {
        return verdict;
    }
    if let Err(verdict) = expect_in(
        subject,
        "type",
        "observation.subject",
        &["repository", "agent", "host"],
    ) {
        return verdict;
    }
    if let Err(verdict) = expect_str(subject, "id", "observation.subject", None) {
        return verdict;
    }
    if let Err(verdict) = expect_timestamp(obj, "observed_at", "observation") {
        return verdict;
    }
    if let Err(verdict) = expect_in(
        obj,
        "freshness",
        "observation",
        &["fresh", "stale", "unknown"],
    ) {
        return verdict;
    }
    if let Err(verdict) = expect_in(obj, "completeness", "observation", &["complete", "partial"]) {
        return verdict;
    }
    if let Some(payload) = obj.get("payload")
        && let Err(verdict) = table(payload, "observation.payload")
    {
        return verdict;
    }
    Verdict::accept()
}

fn validate_plan(obj: &Val) -> Verdict {
    if let Err(verdict) = check_schema(obj, Family::Plan) {
        return verdict;
    }
    let allowed = [
        "schema",
        "plan_id",
        "workflow_id",
        "workflow_hash",
        "state_epoch",
        "repository",
        "issue",
        "steps",
    ];
    if let Err(verdict) = require_keys(obj, &allowed, &allowed, "plan") {
        return verdict;
    }
    if let Err(verdict) = expect_str(obj, "plan_id", "plan", Some(is_plan_id)) {
        return verdict;
    }
    if let Err(verdict) = expect_str(obj, "workflow_id", "plan", Some(is_slug)) {
        return verdict;
    }
    if let Err(verdict) = expect_str(obj, "workflow_hash", "plan", Some(is_hex64)) {
        return verdict;
    }
    if let Err(verdict) = expect_int(obj, "state_epoch", "plan", 0) {
        return verdict;
    }
    if let Err(verdict) = expect_str(obj, "repository", "plan", Some(is_repository_identity)) {
        return verdict;
    }
    let Some(issue) = obj.get("issue") else {
        return Verdict::refuse(Refusal::Malformed, "plan: missing required issue");
    };
    if let Err(verdict) = issue_shaped(issue) {
        return verdict;
    }
    let Some(steps) = obj.get("steps") else {
        return Verdict::refuse(Refusal::Malformed, "plan: missing required steps");
    };
    const STEP_KINDS: [&str; 9] = [
        "checkout",
        "worktree_create",
        "harness_start",
        "prompt",
        "collect_outcome",
        "review_evidence",
        "merge",
        "cleanup",
        "publish",
    ];
    match steps {
        Val::Arr(items) if !items.is_empty() => {
            for step in items {
                if let Err(verdict) = table(step, "plan step") {
                    return verdict;
                }
                if let Err(verdict) = expect_str(step, "id", "plan step", Some(is_slug)) {
                    return verdict;
                }
                match step.get("kind") {
                    Some(Val::Str(kind)) if STEP_KINDS.contains(&kind.as_str()) => {}
                    _ => {
                        return Verdict::refuse(
                            Refusal::Malformed,
                            "plan step kind outside closed set",
                        );
                    }
                }
                match step.get("params") {
                    None | Some(Val::Null) | Some(Val::Obj(_)) => {}
                    Some(other) => {
                        return Verdict::refuse(
                            Refusal::Malformed,
                            format!(
                                "plan step params must be an object or null, got {}",
                                other.type_name()
                            ),
                        );
                    }
                }
            }
        }
        _ => return Verdict::refuse(Refusal::Malformed, "plan.steps must be a non-empty list"),
    }
    Verdict::accept()
}

fn validate_capability(obj: &Val) -> Verdict {
    if let Err(verdict) = check_schema(obj, Family::Capability) {
        return verdict;
    }
    if let Err(verdict) = require_keys(
        obj,
        &["schema", "axis", "actor", "capabilities"],
        &["schema", "axis", "actor", "capabilities"],
        "capability",
    ) {
        return verdict;
    }
    let axis = match obj.get("axis") {
        Some(Val::Str(text)) if text == "harness" || text == "forge" => text.as_str(),
        Some(other) => {
            return Verdict::refuse(
                Refusal::Malformed,
                format!(
                    "capability.axis outside closed set, got {}",
                    other.type_name()
                ),
            );
        }
        None => return Verdict::refuse(Refusal::Malformed, "capability: missing required axis"),
    };
    if let Err(verdict) = expect_str(obj, "actor", "capability", Some(is_actor)) {
        return verdict;
    }
    let closed: &[&str] = if axis == "harness" {
        &[
            "discover",
            "start",
            "prompt",
            "observe",
            "interrupt",
            "outcome",
            "identity",
        ]
    } else {
        &[
            "read_refs",
            "read_issues",
            "read_checks",
            "create_pr",
            "comment",
        ]
    };
    match obj.get("capabilities") {
        Some(Val::Arr(items)) if !items.is_empty() => {
            for item in items {
                let ok = matches!(item, Val::Str(text) if closed.contains(&text.as_str()));
                if !ok {
                    return Verdict::refuse(
                        Refusal::Malformed,
                        format!(
                            "capability.capabilities must be a non-empty subset of the closed {axis} set"
                        ),
                    );
                }
            }
        }
        _ => {
            return Verdict::refuse(
                Refusal::Malformed,
                format!(
                    "capability.capabilities must be a non-empty subset of the closed {axis} set"
                ),
            );
        }
    }
    Verdict::accept()
}

/// Registry of the families this module validates (used by drift tests).
pub const SUPPORTED_FAMILIES: [Family; 7] = [
    Family::Config,
    Family::Policy,
    Family::Output,
    Family::Error,
    Family::Observation,
    Family::Plan,
    Family::Capability,
];

// ---------------------------------------------------------------------------
// Fixture-corpus discrimination tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::sha256_hex;
    use std::path::{Path, PathBuf};

    /// Location of the committed fixture corpus.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn fixture_bytes(relative: &str) -> Vec<u8> {
        std::fs::read(repo_root().join("schemas/fixtures").join(relative))
            .unwrap_or_else(|err| panic!("read fixture {relative}: {err}"))
    }

    /// Every manifest row for a supported family holds under these
    /// validators: accepts accept, malformed/unknown-version/noncanonical
    /// refusals refuse with the right class, and pinned sha256 digests match.
    #[test]
    fn fixture_corpus_manifest_expectations_hold() {
        let manifest = fixture_bytes("manifest.jsonl");
        let text = std::str::from_utf8(&manifest).expect("manifest utf8");
        let mut rows = Vec::new();
        for (lineno, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row = Val::parse_json(line)
                .unwrap_or_else(|err| panic!("manifest line {}: {err}", lineno + 1));
            rows.push(row);
        }
        assert!(!rows.is_empty(), "manifest is empty");

        let family_of = |name: &str| -> Option<Family> {
            SUPPORTED_FAMILIES
                .iter()
                .copied()
                .find(|f| f.family_name() == name)
        };

        let mut checked = 0usize;
        let mut failures: Vec<String> = Vec::new();
        for row in rows {
            let Some(family_name) = row.get("family").and_then(Val::as_str) else {
                continue;
            };
            let Some(family) = family_of(family_name) else {
                continue;
            };
            let file = row.get("file").and_then(Val::as_str).expect("row file");
            let expect = row.get("expect").and_then(Val::as_str).expect("row expect");
            let kind = row.get("kind").and_then(Val::as_str);
            let bytes = fixture_bytes(file);
            let verdict = validate_bytes(family, &bytes);

            let expectation = match (expect, kind) {
                ("accept", _) => {
                    if !verdict.is_accepted() {
                        Some(format!(
                            "{file}: expected accept, got {} ({})",
                            verdict.refusal.map(Refusal::as_str).unwrap_or("accept"),
                            verdict.message()
                        ))
                    } else {
                        None
                    }
                }
                ("refuse", Some("unknown-version")) => {
                    if verdict.refusal() != Some(Refusal::Version) {
                        Some(format!(
                            "{file}: unknown-version fixture must refuse-version, got {:?}",
                            verdict.refusal().map(Refusal::as_str)
                        ))
                    } else {
                        None
                    }
                }
                ("refuse", Some("noncanonical")) => {
                    if verdict.refusal() != Some(Refusal::Noncanonical) {
                        Some(format!(
                            "{file}: noncanonical fixture must refuse-noncanonical, got {:?}",
                            verdict.refusal().map(Refusal::as_str)
                        ))
                    } else {
                        None
                    }
                }
                ("refuse", _) => {
                    if verdict.is_accepted() {
                        Some(format!("{file}: expected refusal, got accept"))
                    } else {
                        None
                    }
                }
                other => Some(format!("{file}: unexpected manifest row {other:?}")),
            };
            if let Some(failure) = expectation {
                failures.push(failure);
            }
            if let Some(pinned) = row.get("sha256").and_then(Val::as_str) {
                let actual = sha256_hex(&bytes);
                if actual != pinned {
                    failures.push(format!(
                        "{file}: digest mismatch (manifest {pinned} vs actual {actual})"
                    ));
                }
            }
            checked += 1;
        }
        assert!(checked > 0, "no supported-family rows were checked");
        assert!(
            failures.is_empty(),
            "fixture expectations failed ({} checked):\n{}",
            checked,
            failures.join("\n")
        );
    }

    /// Canonical-bytes rule: the stored plan.valid.json is byte-canonical,
    /// its digest is pinned, and re-canonicalizing the parsed document
    /// reproduces the stored bytes exactly.
    #[test]
    fn plan_valid_fixture_is_canonical_and_digest_pinned() {
        let bytes = fixture_bytes("plan/plan.valid.json");
        let parsed = Val::parse_json(std::str::from_utf8(&bytes).expect("utf8")).expect("parse");
        assert_eq!(
            canonical_bytes(&parsed),
            bytes,
            "fixture must be stored canonically"
        );
        assert_eq!(
            sha256_hex(&bytes),
            "8199a193f906491e3061847f1e02440360c75b2a7c8ee89b7d0edf532bb7fe33"
        );
        let verdict = validate_bytes(Family::Plan, &bytes);
        assert!(
            verdict.is_accepted(),
            "valid plan refused: {}",
            verdict.message()
        );
    }

    /// Discriminating acceptance: each family's valid fixture accepts and its
    /// malformed/unknown-version fixtures refuse with the expected class.
    /// (The manifest test above already covers every row generically; this
    /// table keeps the per-file expectations readable and explicit.)
    #[test]
    fn per_family_discrimination_bites() {
        let checks: &[(Family, &str, bool)] = &[
            (Family::Config, "config/config.valid.toml", true),
            (Family::Config, "config/config.malformed.toml", false),
            (Family::Config, "config/config.unknown-version.toml", false),
            (Family::Policy, "policy/policy.valid.toml", true),
            (Family::Policy, "policy/policy.malformed.toml", false),
            (Family::Policy, "policy/policy.unknown-version.toml", false),
            (Family::Output, "output/output.valid.json", true),
            (Family::Output, "output/output.partial.valid.json", true),
            (Family::Output, "output/output.error.valid.json", true),
            (Family::Output, "output/output.malformed.json", false),
            (Family::Output, "output/output.unknown-version.json", false),
            (Family::Error, "error/error.valid.json", true),
            (Family::Error, "error/error.malformed.json", false),
            (Family::Error, "error/error.unknown-version.json", false),
            (
                Family::Observation,
                "observation/observation.valid.json",
                true,
            ),
            (
                Family::Observation,
                "observation/observation.malformed.json",
                false,
            ),
            (
                Family::Observation,
                "observation/observation.unknown-version.json",
                false,
            ),
            (Family::Plan, "plan/plan.valid.json", true),
            (Family::Plan, "plan/plan.malformed.json", false),
            (Family::Plan, "plan/plan.noncanonical.json", false),
            (Family::Plan, "plan/plan.unknown-version.json", false),
            (
                Family::Capability,
                "capability/capability.harness.valid.json",
                true,
            ),
            (
                Family::Capability,
                "capability/capability.forge.valid.json",
                true,
            ),
            (
                Family::Capability,
                "capability/capability.malformed.json",
                false,
            ),
            (
                Family::Capability,
                "capability/capability.unknown-version.json",
                false,
            ),
        ];
        for (family, file, expect_accept) in checks {
            let bytes = fixture_bytes(file);
            let verdict = validate_bytes(*family, &bytes);
            assert_eq!(
                verdict.is_accepted(),
                *expect_accept,
                "{file}: expected accept={expect_accept}, got {:?} ({})",
                verdict.refusal().map(Refusal::as_str),
                verdict.message()
            );
        }
    }

    /// The malformed config fixture refuses because of the unknown top-level
    /// table, and the unknown-version config refuses with REFUSE_VERSION —
    /// the two are distinguishable (discrimination).
    #[test]
    fn config_version_and_shape_refusals_are_distinct() {
        let malformed = fixture_bytes("config/config.malformed.toml");
        let verdict = validate_bytes(Family::Config, &malformed);
        assert_eq!(verdict.refusal(), Some(Refusal::Malformed));
        assert!(verdict.message().contains("unknown key"));

        let unknown = fixture_bytes("config/config.unknown-version.toml");
        let verdict = validate_bytes(Family::Config, &unknown);
        assert_eq!(verdict.refusal(), Some(Refusal::Version));
    }

    /// Parser equivalence: this crate's strict parser and the reference
    /// serde_json parser agree on every JSON fixture (catches escape/unicode
    /// handling regressions in the hand-written parser).
    #[test]
    fn json_parser_matches_serde_json_on_fixtures() {
        let dir = repo_root().join("schemas/fixtures");
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("fixtures dir") {
            collect_json_files(&entry.expect("entry").path(), &mut files);
        }
        assert!(files.len() >= 30, "expected a wide fixture corpus");
        for path in files {
            let text = std::fs::read_to_string(&path).expect("read fixture");
            let ours = Val::parse_json(&text);
            let theirs = serde_json::from_str::<serde_json::Value>(&text);
            match (&ours, &theirs) {
                (Ok(ours), Ok(theirs)) => {
                    let converted = from_json_value(theirs.clone());
                    assert_eq!(
                        &converted,
                        ours,
                        "parser disagreement on {}",
                        path.display()
                    );
                }
                (Err(_), Err(_)) => {}
                (Err(ours), Ok(_)) => panic!(
                    "our parser refused {} but serde_json accepts: {ours:?}",
                    path.display()
                ),
                (Ok(_), Err(theirs)) => panic!(
                    "serde_json refused {} but our parser accepts: {theirs}",
                    path.display()
                ),
            }
        }
    }

    fn collect_json_files(path: &Path, out: &mut Vec<PathBuf>) {
        if path.is_dir() {
            for entry in std::fs::read_dir(path).expect("read dir") {
                collect_json_files(&entry.expect("entry").path(), out);
            }
        } else if path
            .extension()
            .is_some_and(|ext| ext == "json" || ext == "jsonl")
        {
            out.push(path.to_path_buf());
        }
    }

    fn from_json_value(value: serde_json::Value) -> Val {
        match value {
            serde_json::Value::Null => Val::Null,
            serde_json::Value::Bool(b) => Val::Bool(b),
            serde_json::Value::Number(number) => {
                if let Some(int) = number.as_i64() {
                    Val::Int(int)
                } else {
                    Val::Float(number.as_f64().expect("number"))
                }
            }
            serde_json::Value::String(text) => Val::Str(text),
            serde_json::Value::Array(items) => Val::Arr(
                items
                    .iter()
                    .map(|item| from_json_value(item.clone()))
                    .collect(),
            ),
            serde_json::Value::Object(map) => Val::Obj(
                map.iter()
                    .map(|(k, v)| (k.clone(), from_json_value(v.clone())))
                    .collect(),
            ),
        }
    }
}
