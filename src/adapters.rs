//! Harness adapters (issues #7, #33): capability-negotiated adapters for
//! Hermes, Claude Code, Codex, Pi (earendil-works/pi), and the declarative
//! generic argv adapter.
//!
//! # Adapter contract (docs/contracts/spec-capabilities.md, "Adapter
//! contract", issue #7)
//!
//! Every adapter profile declares an `hf-capability/v1` document (axis
//! `harness`, closed 7-capability set) and executes typed operations through
//! [`crate::process::run`]: argv arrays only, allowlisted environment only,
//! per-op deadline, capped + redacted output, typed exits. Prompts and
//! untrusted issue text are transported as data — a single final argv
//! element on the prompt operation — and can never alter adapter argv or
//! policy. Adapters never write files, never read the host environment
//! directly (the caller passes an allowlisted environment map), and never
//! store tokens or transcripts (trust model T5 / AC8).
//!
//! - Hermes / Claude Code / Codex / Pi are the four official 1.0 adapters
//!   (adapter examples per ADR-0003; metadata lives here, never in core
//!   planning). Their declared version ranges are recorded in
//!   docs/contracts/compatibility.md (Hermes/Claude Code/Codex measured
//!   2026-09-06; Pi 0.85.1 measured 2026-09-08 against the SHA-verified
//!   linux-x64 prebuilt, with darwin arm64/x64 prebuilts available at that
//!   version); the exact real-world flag parity of the headless invocation
//!   rows is [awaiting-evidence] until the human-gated clean-host smokes
//!   run (AC6), so this slice verifies the contract with fake executables
//!   only (AC7).
//! - The `argv` kind is the declarative generic adapter: validated static
//!   argv prefixes per operation, explicit capability declarations, bare
//!   executable names resolved through the allowlisted PATH (the resolved
//!   absolute identity is what is spawned), bounded time/output, typed
//!   exits. No shell evaluation, no command templates, no capability
//!   inference from prose, no dynamic plugin SDK.
//! - Session identity (AC3) binds the Herdr workspace session id plus a
//!   stable terminal/native-session identity plus a generation counter.
//!   A mutable pane label is not part of the identity and can never
//!   substitute for any of the three parts; binding without all three is a
//!   typed refusal (`refusal.identity.incomplete`).
//! - Pi lane lifecycle under Herdr (issue #33 A2): when a pi profile
//!   operation runs inside a Herdr pane (`HERDR_ENV=1` + `HERDR_PANE_ID`
//!   in the allowlisted environment), the adapter reports the lane
//!   lifecycle through the workspace executable's `pane report-agent` row
//!   (custom-integration contract: `--source custom:herdr-fleet-pi`,
//!   `--agent pi`). `start` reports `working`; a terminal `prompt` reports
//!   `idle` (Herdr has no done state), except `refusal.credentials` which
//!   reports `blocked` (a user decision — provider key — is required; the
//!   message is static and never carries credential detail). Reporting is
//!   best-effort and never changes the typed op result, and is a no-op
//!   outside Herdr. `herdr agent start --kind pi` remains the
//!   substrate/orchestrator path for interactive pi panes (requires a pane
//!   at an interactive shell prompt); headless adapter runs report through
//!   the pane rows instead.
//! - Unknown or unavailable harnesses fail with a typed refusal
//!   (`unknown.harness`, `refusal.unavailable.harness`) and never disturb
//!   independent read-only operations (observe.rs pattern, AC4).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::formats::{is_actor, parse_semver};
use crate::process::{ProcSpec, ProcStatus, run};
use crate::redact::redact;
use crate::schema::{Family, validate_doc};
use crate::value::{Val, bool_, integer, null, object, string};

/// Closed `harness` axis capability set (`hf-capability/v1`, mirror of
/// schema.rs and the fixture probe).
pub const HARNESS_CAPS: [&str; 7] = [
    "discover",
    "start",
    "prompt",
    "observe",
    "interrupt",
    "outcome",
    "identity",
];

/// The executable implementing workspace session operations (session
/// observation, interruption, outcome collection, identity read-back). The
/// invocation rows against it are a v1 candidate contract
/// ([awaiting-evidence] until clean-host verification, AC6); fake
/// executables in tests pin the exact argv shape.
pub const WORKSPACE_EXECUTABLE: &str = "herdr";

/// Default per-operation deadline (same bound as the read adapters).
pub const ADAPTER_TIMEOUT: Duration = Duration::from_secs(10);

/// Byte cap on captured harness/workspace output (bounded output; spec-cli
/// §6 redaction happens before this text is ever stored).
pub const OUTPUT_CAP: usize = 64 * 1024;

/// Cap on diagnostics text attached to typed failures.
const DIAGNOSTIC_CAP: usize = 300;

// ---------------------------------------------------------------------------
// Typed refusal / failure codes (documented in spec-capabilities.md,
// "Adapter contract — refusal and failure codes")
// ---------------------------------------------------------------------------

/// The configured kind is not one of the closed adapter kinds.
pub const CODE_UNKNOWN_HARNESS: &str = "unknown.harness";
/// A requested capability/operation is not in the closed harness set or is
/// not declared by the profile.
pub const CODE_UNKNOWN_CAPABILITY: &str = "unknown.capability";
/// The harness executable is missing from the allowlisted PATH or could not
/// be spawned.
pub const CODE_UNAVAILABLE: &str = "refusal.unavailable.harness";
/// The harness reported an authentication failure (closed-marker
/// classification of an already-failed invocation; never capability
/// inference from prose).
pub const CODE_CREDENTIALS: &str = "refusal.credentials";
/// Structured output could not be parsed or validated.
pub const CODE_MALFORMED: &str = "refusal.malformed.output";
/// The identity read-back does not match the bound session identity.
pub const CODE_STALE_IDENTITY: &str = "refusal.stale.identity";
/// A session identity was bound without all three required parts.
pub const CODE_INCOMPLETE_IDENTITY: &str = "refusal.identity.incomplete";
/// The per-operation deadline was exceeded and the child was killed.
pub const CODE_TIMEOUT: &str = "adapter.timeout";
/// The child process died without producing a terminal outcome.
pub const CODE_PROCESS_DEATH: &str = "adapter.process_death";
/// The child exited with a non-zero code that is not a typed refusal.
pub const CODE_EXIT: &str = "adapter.exit";
/// The typed request itself is malformed (payload on a non-prompt
/// operation, prompt without a payload, path-like executable).
pub const CODE_BAD_REQUEST: &str = "refusal.request.malformed";

/// A typed adapter error shaped like `hf-error/v1` (spec-cli.md §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterError {
    /// Stable lowercase-dotted code (see constants above).
    pub code: &'static str,
    /// Human message.
    pub message: String,
    /// Whether retrying the same operation may succeed later.
    pub retryable: bool,
}

impl AdapterError {
    /// A typed refusal (not retryable).
    pub fn refusal(code: &'static str, message: impl Into<String>) -> AdapterError {
        AdapterError {
            code,
            message: message.into(),
            retryable: false,
        }
    }

    /// A runtime failure that may succeed on retry.
    pub fn failure(
        code: &'static str,
        message: impl Into<String>,
        retryable: bool,
    ) -> AdapterError {
        AdapterError {
            code,
            message: message.into(),
            retryable,
        }
    }

    /// Render as an `hf-error/v1` document (validated against the family in
    /// tests).
    pub fn to_error_doc(&self) -> Val {
        object(vec![
            ("schema", string("hf-error/v1")),
            ("code", string(self.code)),
            ("message", string(&self.message)),
            ("retryable", bool_(self.retryable)),
            ("details", null()),
        ])
    }
}

// ---------------------------------------------------------------------------
// Kinds and official adapter metadata
// ---------------------------------------------------------------------------

/// The five closed adapter kinds. Hermes, Claude Code, Codex, and Pi are
/// the official 1.0 adapters; `argv` is the declarative generic adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarnessKind {
    /// Hermes Agent (`hermes` on PATH).
    Hermes,
    /// Claude Code (`claude` on PATH).
    ClaudeCode,
    /// OpenAI Codex CLI (`codex` on PATH).
    Codex,
    /// Pi (earendil-works/pi, `pi` on PATH).
    Pi,
    /// Declarative generic argv adapter (bare executable resolved via PATH).
    Argv,
}

impl HarnessKind {
    /// The official adapters (Argv is excluded).
    pub const OFFICIAL: [HarnessKind; 4] = [
        HarnessKind::Hermes,
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::Pi,
    ];

    /// Stable kind name used in config (`harness.<key>.kind`).
    pub fn name(self) -> &'static str {
        match self {
            HarnessKind::Hermes => "hermes",
            HarnessKind::ClaudeCode => "claude-code",
            HarnessKind::Codex => "codex",
            HarnessKind::Pi => "pi",
            HarnessKind::Argv => "argv",
        }
    }

    /// Parse a config kind name; `None` for anything outside the closed set
    /// (the caller turns that into `unknown.harness`).
    pub fn parse(text: &str) -> Option<HarnessKind> {
        match text {
            "hermes" => Some(HarnessKind::Hermes),
            "claude-code" => Some(HarnessKind::ClaudeCode),
            "codex" => Some(HarnessKind::Codex),
            "pi" => Some(HarnessKind::Pi),
            "argv" => Some(HarnessKind::Argv),
            _ => None,
        }
    }
}

/// A semantic version `major.minor.patch`.
pub type Semver = (u64, u64, u64);

/// Declared support range for an official adapter: the minimum version
/// below which herdr-fleet refuses to operate, and the current version the
/// slice documents as the tested ceiling (compatibility.md policy shape).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionRange {
    /// Declared minimum (inclusive); below this the adapter is refused.
    pub minimum: Semver,
    /// Declared current — the version the release notes name as tested
    /// ceiling.
    pub current: Semver,
}

impl VersionRange {
    /// Whether a probed version satisfies the declared range.
    pub fn accepts(&self, version: Semver) -> bool {
        version >= self.minimum
    }
}

/// Official adapter metadata (adapter layer only — core never branches on
/// actor ids, ADR-0003). Version facts are measured from public release
/// metadata on 2026-09-06 (Hermes/Claude Code/Codex) and 2026-09-08 (Pi;
/// docs/contracts/compatibility.md rows); the minimum == current rows are
/// provisional exact-version floors ([awaiting-evidence] until the
/// human-gated clean-host matrix, AC6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfficialSpec {
    /// The kind.
    pub kind: HarnessKind,
    /// Bare executable name (resolved via the allowlisted PATH).
    pub executable: &'static str,
    /// Stable opaque actor id (`hf-capability/v1`).
    pub actor: &'static str,
    /// Declared version range.
    pub range: VersionRange,
    /// Declared capabilities (the full closed harness set for all four
    /// official adapters).
    pub capabilities: &'static [&'static str],
}

/// The four official adapter specs.
pub fn official_specs() -> [OfficialSpec; 4] {
    [
        OfficialSpec {
            kind: HarnessKind::Hermes,
            executable: "hermes",
            actor: "hermes",
            range: VersionRange {
                minimum: (0, 21, 0),
                current: (0, 21, 0),
            },
            capabilities: &HARNESS_CAPS,
        },
        OfficialSpec {
            kind: HarnessKind::ClaudeCode,
            executable: "claude",
            actor: "claude-code",
            range: VersionRange {
                minimum: (2, 1, 263),
                current: (2, 1, 263),
            },
            capabilities: &HARNESS_CAPS,
        },
        OfficialSpec {
            kind: HarnessKind::Codex,
            executable: "codex",
            actor: "codex",
            range: VersionRange {
                minimum: (0, 153, 4),
                current: (0, 153, 4),
            },
            capabilities: &HARNESS_CAPS,
        },
        OfficialSpec {
            kind: HarnessKind::Pi,
            executable: "pi",
            actor: "pi",
            range: VersionRange {
                minimum: (0, 85, 1),
                current: (0, 85, 1),
            },
            capabilities: &HARNESS_CAPS,
        },
    ]
}

/// Look up the official spec for a kind (`None` for `Argv`).
pub fn official_spec(kind: HarnessKind) -> Option<OfficialSpec> {
    official_specs().into_iter().find(|spec| spec.kind == kind)
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

/// The typed adapter operations (contract: capability discovery, start,
/// prompt delivery, observation, interruption/cancellation, terminal
/// outcome, identity/read-back).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    /// Instantiate/bind a harness session (validates the bound identity and
    /// the declared capability; the real workspace session creation is
    /// daemon/substrate wiring, not an adapter subprocess).
    Start,
    /// Deliver one prompt as data (single final argv element; transcript
    /// output is capped and redacted at the boundary).
    Prompt,
    /// Observe the session through the workspace read-back.
    Observe,
    /// Interrupt/cancel the session through the workspace read-back.
    Interrupt,
    /// Collect the terminal outcome through the workspace read-back.
    Outcome,
    /// Read back the stable agent identity and compare it to the bound one.
    Identity,
}

impl Op {
    /// All contract operations in closed-set order.
    pub const ALL: [Op; 6] = [
        Op::Start,
        Op::Prompt,
        Op::Observe,
        Op::Interrupt,
        Op::Outcome,
        Op::Identity,
    ];

    /// Stable operation name.
    pub fn name(self) -> &'static str {
        match self {
            Op::Start => "start",
            Op::Prompt => "prompt",
            Op::Observe => "observe",
            Op::Interrupt => "interrupt",
            Op::Outcome => "outcome",
            Op::Identity => "identity",
        }
    }

    /// The harness capability this operation requires.
    pub fn capability(self) -> &'static str {
        self.name()
    }

    /// Parse an operation name from the closed set.
    pub fn parse(text: &str) -> Option<Op> {
        Op::ALL.iter().copied().find(|op| op.name() == text)
    }
}

/// One typed operation request. The payload is data, never code: it is a
/// single final argv element on `Prompt` only, and it can never alter the
/// adapter argv or policy (AC5).
#[derive(Clone, Debug)]
pub struct OpRequest<'a> {
    /// The operation.
    pub op: Op,
    /// The bound session the operation targets.
    pub session: &'a SessionHandle,
    /// Untrusted text payload (prompt data); only valid for `Prompt`.
    pub payload: Option<&'a str>,
    /// Per-operation deadline.
    pub timeout: Duration,
}

/// One typed operation result. `status` uses the `hf-outcome/v1` closed set
/// (`succeeded` | `failed` | `ambiguous` | `refused`); interruption,
/// timeout, and process death yield `ambiguous`, typed refusals yield
/// `refused`, ordinary non-zero exits yield `failed`.
#[derive(Clone, Debug, PartialEq)]
pub struct OpResult {
    /// Profile key the operation ran against.
    pub profile_key: String,
    /// Session id the operation targeted.
    pub session_id: String,
    /// The operation that ran.
    pub op: Op,
    /// Outcome status (`hf-outcome/v1` closed set).
    pub status: &'static str,
    /// Stable failure code when the operation did not succeed.
    pub code: Option<&'static str>,
    /// Human message (redacted).
    pub message: Option<String>,
    /// Typed payload (redacted at the boundary).
    pub payload: Option<Val>,
    /// Bounded diagnostic detail (redacted).
    pub detail: Option<String>,
    /// Wall time of the operation in milliseconds.
    pub elapsed_ms: u64,
}

impl OpResult {
    /// Render as an `hf-outcome/v1` document bound to a plan step. The
    /// caller supplies the plan/step identity and the idempotency key; the
    /// status and error/result shape come from this result.
    pub fn to_outcome_doc(
        &self,
        plan_id: &str,
        step_id: &str,
        idempotency_key: &str,
        observed_at: &str,
    ) -> Val {
        let failed_like = matches!(self.status, "failed" | "refused");
        let error = if failed_like {
            object(vec![
                ("schema", string("hf-error/v1")),
                ("code", string(self.code.unwrap_or("adapter.exit"))),
                ("message", string(self.message.as_deref().unwrap_or(""))),
                ("retryable", bool_(false)),
                (
                    "details",
                    self.detail
                        .as_ref()
                        .map(|detail| object(vec![("diagnostics", string(detail))]))
                        .unwrap_or_else(null),
                ),
            ])
        } else {
            null()
        };
        let result = if failed_like {
            null()
        } else {
            self.payload
                .clone()
                .unwrap_or_else(|| object(vec![("ok", bool_(true))]))
        };
        object(vec![
            ("schema", string("hf-outcome/v1")),
            ("plan_id", string(plan_id)),
            ("step_id", string(step_id)),
            ("status", string(self.status)),
            ("idempotency_key", string(idempotency_key)),
            ("observed_at", string(observed_at)),
            ("result", result),
            ("error", error),
        ])
    }
}

/// A declarative adapter profile: kind, bare executable name, stable actor
/// id, explicit capability set, and (for the `argv` kind) the explicit
/// per-operation static argv prefixes. Config v1 (`harness.<key>`) carries
/// kind/executable/env_allow; the env allowlist is applied by the caller
/// when it builds the environment map ([`Profile::from_config`] documents
/// the argv-capability consequence).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// Config/profile key (slug).
    pub key: String,
    /// Adapter kind.
    pub kind: HarnessKind,
    /// Bare executable name, resolved through the allowlisted PATH (never
    /// an absolute path in config).
    pub executable: String,
    /// Stable opaque actor id for `hf-capability/v1`.
    pub actor: String,
    /// Declared capabilities (explicit subset of the closed harness set).
    pub capabilities: Vec<String>,
    /// Declared version range (official adapters only).
    pub declared_range: Option<VersionRange>,
    /// Explicit per-operation static argv prefixes for the `argv` kind
    /// (op name -> prefix; the prompt payload is appended as one final
    /// element). Ignored for official kinds, which use their documented
    /// headless invocation rows.
    pub op_args: BTreeMap<String, Vec<String>>,
}

impl Profile {
    /// An official adapter profile (capabilities and range come from the
    /// official metadata table).
    pub fn official(kind: HarnessKind, key: impl Into<String>) -> Result<Profile, AdapterError> {
        let spec = official_spec(kind).ok_or_else(|| {
            AdapterError::refusal(CODE_UNKNOWN_HARNESS, "argv is not an official adapter kind")
        })?;
        Ok(Profile {
            key: key.into(),
            kind,
            executable: spec.executable.to_string(),
            actor: spec.actor.to_string(),
            capabilities: spec.capabilities.iter().map(|s| s.to_string()).collect(),
            declared_range: Some(spec.range),
            op_args: BTreeMap::new(),
        })
    }

    /// A declarative generic argv profile. `capabilities` must be a
    /// non-empty subset of the closed harness set (anything else is refused
    /// with `unknown.capability`); `op_args` maps operations to explicit
    /// static argv prefixes whose names must be declared capabilities.
    /// `actor` defaults to the profile key.
    pub fn argv(
        key: impl Into<String>,
        executable: impl Into<String>,
        capabilities: &[&str],
        op_args: BTreeMap<String, Vec<String>>,
    ) -> Result<Profile, AdapterError> {
        let key = key.into();
        let executable = executable.into();
        Self::validate_bare_executable(&executable)?;
        if capabilities.is_empty() {
            return Err(AdapterError::refusal(
                CODE_UNKNOWN_CAPABILITY,
                "argv profiles must declare an explicit non-empty capability set",
            ));
        }
        for capability in capabilities {
            if !HARNESS_CAPS.contains(capability) {
                return Err(AdapterError::refusal(
                    CODE_UNKNOWN_CAPABILITY,
                    format!("capability {capability:?} is not in the closed harness set"),
                ));
            }
        }
        let actor = key.clone();
        if !is_actor(&actor) {
            return Err(AdapterError::refusal(
                CODE_BAD_REQUEST,
                format!("profile key {actor:?} is not a valid actor id"),
            ));
        }
        for (op_name, args) in &op_args {
            let Some(op) = Op::parse(op_name) else {
                return Err(AdapterError::refusal(
                    CODE_UNKNOWN_CAPABILITY,
                    format!("op {op_name:?} is not a harness operation"),
                ));
            };
            if !capabilities.contains(&op.capability()) {
                return Err(AdapterError::refusal(
                    CODE_UNKNOWN_CAPABILITY,
                    format!("op {op_name:?} requires a declared capability"),
                ));
            }
            for arg in args {
                if arg.contains('\0') {
                    return Err(AdapterError::refusal(
                        CODE_BAD_REQUEST,
                        "argv entries must not contain NUL bytes",
                    ));
                }
            }
        }
        Ok(Profile {
            key,
            kind: HarnessKind::Argv,
            executable,
            actor,
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            declared_range: None,
            op_args,
        })
    }

    /// Build a profile from a validated `hf-config/v1` harness entry
    /// (`config::Harness`). Official kinds take their capability set and
    /// version range from the official metadata; the `argv` kind declares
    /// no capabilities through config v1 (its table carries no capability
    /// field), so config-declared argv profiles support discovery and
    /// probing only — any operation is refused with `unknown.capability`
    /// until an explicit capability declaration is wired from a profile
    /// source (documented in spec-config.md/spec-capabilities.md).
    pub fn from_config(harness: &crate::config::Harness) -> Result<Profile, AdapterError> {
        let kind = HarnessKind::parse(&harness.kind).ok_or_else(|| {
            AdapterError::refusal(
                CODE_UNKNOWN_HARNESS,
                format!(
                    "unknown harness kind {:?}; supported kinds: {} (official) and {:?} (declarative)",
                    harness.kind,
                    HarnessKind::OFFICIAL
                        .iter()
                        .map(|kind| kind.name())
                        .collect::<Vec<_>>()
                        .join(", "),
                    "argv"
                ),
            )
        })?;
        let (actor, capabilities, declared_range) = match official_spec(kind) {
            Some(spec) => (
                spec.actor.to_string(),
                spec.capabilities.iter().map(|s| s.to_string()).collect(),
                Some(spec.range),
            ),
            None => (harness.key.clone(), Vec::new(), None),
        };
        Ok(Profile {
            key: harness.key.clone(),
            kind,
            executable: harness.executable.clone(),
            actor,
            capabilities,
            declared_range,
            op_args: BTreeMap::new(),
        })
    }

    /// Whether the profile declares a capability.
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    /// The `hf-capability/v1` declaration document for this profile
    /// (validated against the family before it is returned).
    pub fn declaration_doc(&self) -> Result<Val, AdapterError> {
        let doc = object(vec![
            ("schema", string("hf-capability/v1")),
            ("axis", string("harness")),
            ("actor", string(&self.actor)),
            (
                "capabilities",
                Val::Arr(self.capabilities.iter().map(|c| string(c)).collect()),
            ),
        ]);
        let verdict = validate_doc(Family::Capability, &doc);
        if !verdict.is_accepted() {
            return Err(AdapterError::refusal(
                CODE_BAD_REQUEST,
                format!("invalid capability declaration: {}", verdict.message()),
            ));
        }
        Ok(doc)
    }

    fn validate_bare_executable(executable: &str) -> Result<(), AdapterError> {
        if executable.is_empty()
            || executable.contains('/')
            || executable.contains('\\')
            || executable.contains('\0')
        {
            return Err(AdapterError::refusal(
                CODE_BAD_REQUEST,
                "executable must be a bare name resolved via PATH (never an absolute path)",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Executable resolution and version probing
// ---------------------------------------------------------------------------

/// Resolve a bare executable name to a verified absolute path through the
/// PATH of the allowlisted environment map. The child is spawned from the
/// resolved absolute identity, never from the parent process's PATH, which
/// keeps tests hermetic and matches the "absolute/verified executable
/// identity" requirement for subprocess adapters. Empty PATH entries are
/// not searched (no implicit current-directory lookup).
pub fn resolve_executable(
    program: &str,
    env: &BTreeMap<String, String>,
) -> Result<PathBuf, AdapterError> {
    if program.is_empty() || program.contains('/') || program.contains('\\') {
        return Err(AdapterError::refusal(
            CODE_BAD_REQUEST,
            "executable must be a bare name resolved via PATH",
        ));
    }
    let Some(path_value) = env.get("PATH") else {
        return Err(AdapterError::refusal(
            CODE_UNAVAILABLE,
            "PATH is not in the allowlisted environment; cannot resolve executables",
        ));
    };
    let sep = if cfg!(windows) { ';' } else { ':' };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for entry in path_value.split(sep) {
        if entry.is_empty() {
            continue;
        }
        let dir = PathBuf::from(entry);
        let dir = if dir.is_absolute() {
            dir
        } else {
            cwd.join(dir)
        };
        let candidate = dir.join(program);
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Ok(candidate);
    }
    Err(AdapterError::refusal(
        CODE_UNAVAILABLE,
        format!("{program:?} not found on the allowlisted PATH"),
    ))
}

/// Outcome of a version/presence probe (`<executable> --version`, bounded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    /// Whether the executable was found and spawned.
    pub present: bool,
    /// Parsed version text when the probe succeeded.
    pub version: Option<String>,
    /// Version compatibility against the declared range when one exists
    /// (`Some(false)` below the declared minimum).
    pub compatible: Option<bool>,
    /// Stable failure code when the probe could not complete.
    pub code: Option<&'static str>,
    /// Redacted, bounded diagnostic detail.
    pub detail: Option<String>,
}

/// Probe a profile's executable presence and version against its declared
/// range (runtime capability probe; compatibility.md policy shape).
pub fn probe_profile(profile: &Profile, env: &BTreeMap<String, String>) -> ProbeResult {
    let (present, version, compatible, code, detail) =
        match resolve_executable(&profile.executable, env) {
            Err(err) => (false, None, None, Some(err.code), Some(err.message)),
            Ok(path) => {
                let args = vec!["--version".to_string()];
                let out = run(ProcSpec {
                    program: path.to_str().unwrap_or_default(),
                    args: &args,
                    env,
                    cwd: None,
                    timeout: ADAPTER_TIMEOUT,
                });
                match out.status {
                    ProcStatus::Exit(0) => {
                        let first_line = out.stdout.lines().next().unwrap_or("").trim();
                        match first_line.split_whitespace().find_map(parse_semver) {
                            Some(version) => {
                                let compatible = profile
                                    .declared_range
                                    .map(|range| range.accepts(version))
                                    .unwrap_or(true);
                                (
                                    true,
                                    Some(format!("{}.{}.{}", version.0, version.1, version.2)),
                                    Some(compatible),
                                    None,
                                    None,
                                )
                            }
                            None => (
                                true,
                                None,
                                Some(false),
                                Some(CODE_MALFORMED),
                                Some(format!(
                                    "unparsable version output: {}",
                                    diagnostics(first_line)
                                )),
                            ),
                        }
                    }
                    ProcStatus::Exit(_code) => (
                        true,
                        None,
                        Some(false),
                        Some(CODE_EXIT),
                        Some(format!(
                            "version probe failed: {}",
                            diagnostics(&out.stderr)
                        )),
                    ),
                    ProcStatus::TimedOut => (
                        true,
                        None,
                        Some(false),
                        Some(CODE_TIMEOUT),
                        Some("version probe timed out".to_string()),
                    ),
                    ProcStatus::SpawnFailed(message) => (
                        true,
                        None,
                        Some(false),
                        Some(CODE_UNAVAILABLE),
                        Some(diagnostics(&message)),
                    ),
                }
            }
        };
    ProbeResult {
        present,
        version,
        compatible,
        code,
        detail,
    }
}

// ---------------------------------------------------------------------------
// Session identity (AC3)
// ---------------------------------------------------------------------------

/// The stable agent identity triple (AC3): Herdr workspace session id +
/// stable terminal/native-session identity + generation. A mutable pane
/// label is deliberately not part of this type — labels can never satisfy
/// any of the three parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentity {
    /// Herdr workspace session id (stable across pane renames).
    pub herdr_session: String,
    /// Stable terminal/native-session identity (e.g. the session leader the
    /// harness process runs under).
    pub terminal_session: String,
    /// Generation counter: increments every time the workspace session is
    /// recreated/rerouted; read-backs from an older generation are stale.
    pub generation: u64,
}

/// Bind an agent identity from its three required parts. Binding without
/// all three parts (or with a part outside the closed identity grammar
/// `[A-Za-z0-9_.-]{1,64}`) is refused with `refusal.identity.incomplete`:
/// a caller that only knows a mutable pane label cannot construct this
/// identity at all (AC3).
pub fn bind_identity(
    herdr_session: &str,
    terminal_session: &str,
    generation: u64,
) -> Result<AgentIdentity, AdapterError> {
    let mut missing = Vec::new();
    if !is_actor(herdr_session) {
        missing.push("herdr_session");
    }
    if !is_actor(terminal_session) {
        missing.push("terminal_session");
    }
    if !missing.is_empty() {
        return Err(AdapterError::refusal(
            CODE_INCOMPLETE_IDENTITY,
            format!(
                "stable agent identity requires all three parts; missing/invalid: {}",
                missing.join(", ")
            ),
        ));
    }
    Ok(AgentIdentity {
        herdr_session: herdr_session.to_string(),
        terminal_session: terminal_session.to_string(),
        generation,
    })
}

/// A bound harness session: an id plus the full identity triple (AC3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionHandle {
    /// Session id (actor grammar; used for workspace read-back addressing).
    pub session_id: String,
    /// The bound stable identity.
    pub identity: AgentIdentity,
}

/// Create a bound session handle from a session id and a fully bound
/// identity.
pub fn new_session(
    session_id: &str,
    identity: AgentIdentity,
) -> Result<SessionHandle, AdapterError> {
    if !is_actor(session_id) {
        return Err(AdapterError::refusal(
            CODE_INCOMPLETE_IDENTITY,
            format!("session id {session_id:?} is outside the closed identity grammar"),
        ));
    }
    Ok(SessionHandle {
        session_id: session_id.to_string(),
        identity,
    })
}

// ---------------------------------------------------------------------------
// Invocation rows and operation execution
// ---------------------------------------------------------------------------

/// Headless prompt invocation rows for the official adapters (v1 contract
/// candidates; exact real-world flag parity is [awaiting-evidence] until
/// the human-gated clean-host smokes, AC6 — see the smoke commands in
/// .report-7.md). The prompt payload is appended as one final argv element
/// and is never interpolated. Workspace operations (observe/interrupt/
/// outcome/identity) run against the workspace executable with typed
/// session addressing.
fn prompt_args(profile: &Profile) -> Result<Vec<String>, AdapterError> {
    match profile.kind {
        HarnessKind::Hermes => Ok(vec!["chat".to_string(), "-q".to_string()]),
        HarnessKind::ClaudeCode => Ok(vec!["-p".to_string()]),
        HarnessKind::Codex => Ok(vec!["exec".to_string()]),
        // One-shot `--print` row (issue #33, measured against pi v0.85.1 on
        // 2026-09-08). Provider/model are opaque adapter metadata carried as
        // argv flags (never persisted, never on a wire); credentials arrive
        // only through the allowlisted environment. The trailing `--` is
        // pi's documented end-of-options guard, so a data-last payload that
        // begins with `-` can never be parsed as an option.
        HarnessKind::Pi => Ok(vec![
            "--provider".to_string(),
            "deepseek".to_string(),
            "--model".to_string(),
            "deepseek-chat".to_string(),
            "--print".to_string(),
            "--".to_string(),
        ]),
        HarnessKind::Argv => Ok(profile.op_args.get("prompt").cloned().unwrap_or_default()),
    }
}

/// Workspace session-operation argv rows (v1 candidate contract against
/// [`WORKSPACE_EXECUTABLE`]; fake executables in tests pin the shape).
fn workspace_args(op: Op, session_id: &str) -> Vec<String> {
    let subcommand = match op {
        Op::Observe | Op::Identity => "show",
        Op::Interrupt => "interrupt",
        Op::Outcome => "outcome",
        _ => unreachable!("workspace_args called for a workspace op"),
    };
    vec![
        "session".to_string(),
        subcommand.to_string(),
        session_id.to_string(),
        "--json".to_string(),
    ]
}

// ---------------------------------------------------------------------------
// Pi lane lifecycle reporting under Herdr (issue #33 A2)
//
// Herdr's custom-integration contract (docs/integrations, herdr 0.8.2):
// an agent running in a Herdr pane inherits `HERDR_ENV`/`HERDR_PANE_ID`/
// `HERDR_BIN_PATH`/`HERDR_SOCKET_PATH`; integrations report semantic state
// through `pane report-agent <pane> --source <id> --agent <label>
// --state <working|idle|blocked>` and release the source's authority with
// `pane release-agent` when the agent exits. Reports must only fire when
// `HERDR_ENV=1` and the required variables are present, and `--source`
// must stay stable and unique to the integration.
// ---------------------------------------------------------------------------

/// Stable, unique lifecycle source id this adapter reports under (herdr
/// custom-integration contract). Never reported outside a Herdr pane.
pub const HERDR_LIFECYCLE_SOURCE: &str = "custom:herdr-fleet-pi";

/// The agent label reported for pi lanes (herdr `agent list` shows the
/// lane as `agent=pi`).
pub const HERDR_LIFECYCLE_AGENT: &str = "pi";

/// The herdr pane context of an operation: `Some(pane_id)` when the
/// allowlisted environment marks a Herdr pane (`HERDR_ENV=1` with a
/// non-empty `HERDR_PANE_ID`); `None` otherwise, which makes lifecycle
/// reporting a no-op outside Herdr.
pub fn herdr_pane_context(env: &BTreeMap<String, String>) -> Option<&str> {
    if env.get("HERDR_ENV").map(String::as_str) != Some("1") {
        return None;
    }
    env.get("HERDR_PANE_ID")
        .map(String::as_str)
        .filter(|pane| !pane.is_empty())
}

/// The herdr lifecycle report after a typed pi operation result
/// (issue #33 A2). Herdr has no `done` state, so a terminal one-shot
/// `prompt` reports `idle`; `refusal.credentials` reports `blocked` (a
/// user decision is required — the provider key — with a static message
/// that never carries credential text). `start` reports `working` while
/// the lane is active. Returns `(state, message)` or `None` when no
/// report applies.
fn herdr_lifecycle_report(result: &OpResult) -> Option<(&'static str, Option<&'static str>)> {
    match result.op {
        Op::Start if result.status == "succeeded" => Some(("working", None)),
        Op::Prompt => match result.code {
            Some(CODE_CREDENTIALS) => Some(("blocked", Some("harness credentials required"))),
            _ => Some(("idle", None)),
        },
        _ => None,
    }
}

/// Run the documented `pane report-agent` row for one lifecycle report.
/// Best-effort sideband: the typed op result is never changed by a report
/// failure (missing/unusable workspace executable, nonzero exit).
fn report_herdr_lifecycle(
    pane: &str,
    state: &str,
    message: Option<&str>,
    env: &BTreeMap<String, String>,
) {
    let mut args = vec![
        "pane".to_string(),
        "report-agent".to_string(),
        pane.to_string(),
        "--source".to_string(),
        HERDR_LIFECYCLE_SOURCE.to_string(),
        "--agent".to_string(),
        HERDR_LIFECYCLE_AGENT.to_string(),
        "--state".to_string(),
        state.to_string(),
    ];
    if let Some(message) = message {
        args.push("--message".to_string());
        args.push(message.to_string());
    }
    match run_typed(WORKSPACE_EXECUTABLE, &args, ADAPTER_TIMEOUT, env, None) {
        ProcessOutcome::Ok(_) => {}
        ProcessOutcome::Failed(_) => {}
    }
}

/// Run one typed operation against a profile (see module docs for the
/// bounds). `Op::Start` binds no subprocess (the handle is already bound);
/// `Op::Prompt` runs the harness executable with the payload as a single
/// final data element; the remaining operations run the workspace
/// read-back/control rows against [`WORKSPACE_EXECUTABLE`].
pub fn execute_op(
    profile: &Profile,
    request: &OpRequest<'_>,
    env: &BTreeMap<String, String>,
) -> OpResult {
    execute_op_at(profile, request, env, None)
}

/// Execute an operation with the child process confined to `cwd` (issue #8:
/// harness work may edit, test, and commit only inside the assigned
/// worktree, so the daemon passes the lane worktree as the child working
/// directory). Everything else matches [`execute_op`].
pub fn execute_op_in_worktree(
    profile: &Profile,
    request: &OpRequest<'_>,
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> OpResult {
    execute_op_at(profile, request, env, Some(cwd))
}

fn execute_op_at(
    profile: &Profile,
    request: &OpRequest<'_>,
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
) -> OpResult {
    let result = execute_op_inner(profile, request, env, cwd);
    // Issue #33 A2: a pi profile running inside a Herdr pane reports the
    // lane lifecycle through the workspace executable (best-effort sideband
    // that never changes the typed op result; no-op outside Herdr).
    if profile.kind == HarnessKind::Pi
        && let (Some(pane), Some((state, message))) =
            (herdr_pane_context(env), herdr_lifecycle_report(&result))
    {
        report_herdr_lifecycle(pane, state, message, env);
    }
    result
}

fn execute_op_inner(
    profile: &Profile,
    request: &OpRequest<'_>,
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
) -> OpResult {
    let started = std::time::Instant::now();
    let session_id = request.session.session_id.clone();
    let capability = request.op.capability();
    if !profile.supports(capability) {
        return op_result(
            profile,
            request,
            "refused",
            Some(CODE_UNKNOWN_CAPABILITY),
            Some(format!(
                "profile {:?} does not declare capability {:?}",
                profile.key, capability
            )),
            None,
            None,
            started,
        );
    }
    if request.payload.is_some() && request.op != Op::Prompt {
        return op_result(
            profile,
            request,
            "refused",
            Some(CODE_BAD_REQUEST),
            Some("a payload is only valid on the prompt operation".to_string()),
            None,
            None,
            started,
        );
    }
    match request.op {
        Op::Start => op_result(
            profile,
            request,
            "succeeded",
            None,
            None,
            Some(object(vec![
                ("session_id", string(&session_id)),
                (
                    "generation",
                    integer(request.session.identity.generation as i64),
                ),
            ])),
            None,
            started,
        ),
        Op::Prompt => {
            let payload = match request.payload {
                Some(payload) => payload,
                None => {
                    return op_result(
                        profile,
                        request,
                        "refused",
                        Some(CODE_BAD_REQUEST),
                        Some("the prompt operation requires a payload".to_string()),
                        None,
                        None,
                        started,
                    );
                }
            };
            let mut args = match prompt_args(profile) {
                Ok(args) => args,
                Err(err) => {
                    return op_result(
                        profile,
                        request,
                        "refused",
                        Some(err.code),
                        Some(err.message),
                        None,
                        None,
                        started,
                    );
                }
            };
            // Data-last rule (AC5): the payload is appended as one literal
            // argv element; nothing else in the argv depends on it.
            args.push(payload.to_string());
            let out = run_typed(&profile.executable, &args, request.timeout, env, cwd);
            match out {
                ProcessOutcome::Ok(text) => op_result(
                    profile,
                    request,
                    "succeeded",
                    None,
                    None,
                    Some(object(vec![("transcript", string(&text))])),
                    None,
                    started,
                ),
                ProcessOutcome::Failed(err) => op_result(
                    profile,
                    request,
                    err.status(),
                    Some(err.code),
                    Some(err.message),
                    None,
                    Some(err.detail),
                    started,
                ),
            }
        }
        Op::Observe | Op::Identity | Op::Interrupt | Op::Outcome => {
            let workspace_session = &request.session.identity.herdr_session;
            let args = workspace_args(request.op, workspace_session);
            let out = run_typed(WORKSPACE_EXECUTABLE, &args, request.timeout, env, cwd);
            let out = match out {
                ProcessOutcome::Ok(text) => text,
                ProcessOutcome::Failed(err) => {
                    return op_result(
                        profile,
                        request,
                        err.status(),
                        Some(err.code),
                        Some(err.message),
                        None,
                        Some(err.detail),
                        started,
                    );
                }
            };
            let doc = match Val::parse_json(&out) {
                Ok(doc) => doc,
                Err(message) => {
                    return op_result(
                        profile,
                        request,
                        "refused",
                        Some(CODE_MALFORMED),
                        Some("workspace read-back returned unparsable JSON".to_string()),
                        None,
                        Some(diagnostics(&format!("{message}: {out}"))),
                        started,
                    );
                }
            };
            match request.op {
                Op::Observe => op_result(
                    profile,
                    request,
                    "succeeded",
                    None,
                    None,
                    Some(session_state_payload(&doc)),
                    None,
                    started,
                ),
                Op::Outcome => {
                    let outcome = doc.get("outcome").and_then(Val::as_str).unwrap_or("");
                    if outcome.is_empty() {
                        op_result(
                            profile,
                            request,
                            "refused",
                            Some(CODE_MALFORMED),
                            Some(
                                "workspace read-back outcome is missing a terminal outcome"
                                    .to_string(),
                            ),
                            None,
                            Some(diagnostics(&out)),
                            started,
                        )
                    } else {
                        op_result(
                            profile,
                            request,
                            "succeeded",
                            None,
                            None,
                            Some(object(vec![
                                (
                                    "state",
                                    string(doc.get("state").and_then(Val::as_str).unwrap_or("")),
                                ),
                                ("outcome", string(outcome)),
                            ])),
                            None,
                            started,
                        )
                    }
                }
                Op::Interrupt => op_result(
                    profile,
                    request,
                    "succeeded",
                    None,
                    None,
                    Some(object(vec![("interrupted", bool_(true))])),
                    None,
                    started,
                ),
                // Op::Identity
                _ => {
                    let read_back = read_back_identity(&doc, request.session);
                    match read_back {
                        Ok(identity_doc) => op_result(
                            profile,
                            request,
                            "succeeded",
                            None,
                            None,
                            Some(identity_doc),
                            None,
                            started,
                        ),
                        Err(err) => op_result(
                            profile,
                            request,
                            "refused",
                            Some(err.code),
                            Some(err.message),
                            None,
                            Some(err.detail),
                            started,
                        ),
                    }
                }
            }
        }
    }
}

/// Execute an operation by name (the typed boundary for callers that carry
/// operation names as data). An operation outside the closed harness set is
/// refused with `unknown.capability`; the payload is only valid for the
/// `prompt` operation.
pub fn execute_named(
    profile: &Profile,
    op_name: &str,
    session: &SessionHandle,
    payload: Option<&str>,
    timeout: Duration,
    env: &BTreeMap<String, String>,
) -> OpResult {
    let Some(op) = Op::parse(op_name) else {
        let request = OpRequest {
            op: Op::Start,
            session,
            payload,
            timeout,
        };
        return op_result(
            profile,
            &request,
            "refused",
            Some(CODE_UNKNOWN_CAPABILITY),
            Some(format!(
                "capability {op_name:?} is not part of the closed harness set"
            )),
            None,
            None,
            std::time::Instant::now(),
        );
    };
    let request = OpRequest {
        op,
        session,
        payload,
        timeout,
    };
    execute_op(profile, &request, env)
}

/// The session-state payload fields the adapter contract reads back from
/// the workspace (whitelisted; everything else in the read-back document is
/// ignored).
fn session_state_payload(doc: &Val) -> Val {
    let mut fields = vec![
        (
            "session_id",
            doc.get("session_id")
                .and_then(Val::as_str)
                .map(string)
                .unwrap_or_else(null),
        ),
        (
            "generation",
            match doc.get("generation") {
                Some(Val::Int(n)) => integer(*n),
                _ => null(),
            },
        ),
        (
            "state",
            doc.get("state")
                .and_then(Val::as_str)
                .map(string)
                .unwrap_or_else(null),
        ),
    ];
    if let Some(outcome) = doc.get("outcome").and_then(Val::as_str) {
        fields.push(("outcome", string(outcome)));
    }
    object(fields)
}

/// Compare a workspace identity read-back document against the bound
/// identity. Any mismatch is a stale identity (`refusal.stale.identity`,
/// AC3): the session no longer runs under the bound triple.
fn read_back_identity(doc: &Val, bound: &SessionHandle) -> Result<Val, StaleIdentity> {
    let read_session = doc.get("session_id").and_then(Val::as_str).unwrap_or("");
    let read_terminal = doc
        .get("terminal_session")
        .and_then(Val::as_str)
        .unwrap_or("");
    let read_generation = match doc.get("generation") {
        Some(Val::Int(n)) => Some(*n),
        _ => None,
    };
    let mut stale = Vec::new();
    if read_session != bound.identity.herdr_session {
        stale.push("herdr_session");
    }
    if read_terminal != bound.identity.terminal_session {
        stale.push("terminal_session");
    }
    if read_generation != Some(bound.identity.generation as i64) {
        stale.push("generation");
    }
    if !stale.is_empty() {
        return Err(StaleIdentity {
            code: CODE_STALE_IDENTITY,
            message: format!("identity read-back mismatch on {}", stale.join(", ")),
            detail: format!(
                "bound={}/{} gen={}",
                bound.identity.herdr_session,
                bound.identity.terminal_session,
                bound.identity.generation
            ),
        });
    }
    Ok(object(vec![
        ("herdr_session", string(&bound.identity.herdr_session)),
        ("terminal_session", string(&bound.identity.terminal_session)),
        ("generation", integer(bound.identity.generation as i64)),
    ]))
}

/// A stale-identity failure carrying a redacted detail line.
#[derive(Debug)]
struct StaleIdentity {
    code: &'static str,
    message: String,
    detail: String,
}

/// Classified child outcome after a typed invocation.
enum ProcessOutcome {
    /// The child exited zero and its output was accepted.
    Ok(String),
    /// The child failed in a classified way.
    Failed(ProcessFailure),
}

/// A classified failure of one typed invocation.
struct ProcessFailure {
    code: &'static str,
    message: String,
    detail: String,
}

impl ProcessFailure {
    /// `hf-outcome/v1` status for the failure class: typed refusals are
    /// `refused`; interruption/timeout/process death are `ambiguous`
    /// (spec-plans.md §5: ambiguous = interrupted/restored work); ordinary
    /// exits are `failed`.
    fn status(&self) -> &'static str {
        if self.code.starts_with("refusal.") || self.code.starts_with("unknown.") {
            "refused"
        } else if matches!(self.code, CODE_TIMEOUT | CODE_PROCESS_DEATH) {
            "ambiguous"
        } else {
            "failed"
        }
    }
}

/// Run one bounded, allowlisted invocation and classify the outcome
/// (typed exits, auth markers, timeout, process death; output capped and
/// redacted at this boundary).
fn run_typed(
    program: &str,
    args: &[String],
    timeout: Duration,
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
) -> ProcessOutcome {
    let resolved = match resolve_executable(program, env) {
        Ok(path) => path,
        Err(err) => {
            return ProcessOutcome::Failed(ProcessFailure {
                code: err.code,
                message: err.message,
                detail: format!("while resolving {program:?}"),
            });
        }
    };
    let out = run(ProcSpec {
        program: resolved.to_str().unwrap_or_default(),
        args,
        env,
        cwd,
        timeout,
    });
    match out.status {
        ProcStatus::Exit(0) => {
            let text = redact(&out.stdout);
            ProcessOutcome::Ok(cap_text(&text))
        }
        ProcStatus::Exit(code) => {
            if code == -1 {
                return ProcessOutcome::Failed(ProcessFailure {
                    code: CODE_PROCESS_DEATH,
                    message: "the harness process died without a terminal outcome".to_string(),
                    detail: diagnostics(&format!("{}{}", out.stderr, out.stdout)),
                });
            }
            let combined = format!("{}{}", out.stdout, out.stderr);
            let lower = combined.to_ascii_lowercase();
            if AUTH_MARKERS.iter().any(|marker| lower.contains(marker)) {
                ProcessOutcome::Failed(ProcessFailure {
                    code: CODE_CREDENTIALS,
                    message: "the harness reported an authentication failure; credentials live in the harness, never here".to_string(),
                    detail: diagnostics(&combined),
                })
            } else {
                ProcessOutcome::Failed(ProcessFailure {
                    code: CODE_EXIT,
                    message: format!("the harness exited with code {code}"),
                    detail: diagnostics(&combined),
                })
            }
        }
        ProcStatus::TimedOut => ProcessOutcome::Failed(ProcessFailure {
            code: CODE_TIMEOUT,
            message: "the operation exceeded its deadline and was cancelled".to_string(),
            detail: format!("deadline {:?}", timeout),
        }),
        ProcStatus::SpawnFailed(message) => ProcessOutcome::Failed(ProcessFailure {
            code: CODE_UNAVAILABLE,
            message: format!("could not spawn {program:?}: {message}"),
            detail: format!("while spawning {program:?}"),
        }),
    }
}

/// Conservative closed markers that turn an already-failed invocation into
/// a `refusal.credentials` typed refusal. This is failure-shape
/// classification of nonzero exits only — never capability inference from
/// prose (ADR-0003), and the matched text never becomes a record.
/// `no api key found` is the measured missing-credentials stderr of pi
/// v0.85.1 (`pi --provider deepseek ... --print ...`, 2026-09-08).
const AUTH_MARKERS: [&str; 9] = [
    "authentication failed",
    "not authenticated",
    "not logged in",
    "unauthorized",
    "authentication required",
    "login required",
    "auth required",
    "api key required",
    "no api key found",
];

/// Build a result with the wall-time already measured.
#[allow(clippy::too_many_arguments)]
fn op_result(
    profile: &Profile,
    request: &OpRequest<'_>,
    status: &'static str,
    code: Option<&'static str>,
    message: Option<String>,
    payload: Option<Val>,
    detail: Option<String>,
    started: std::time::Instant,
) -> OpResult {
    OpResult {
        profile_key: profile.key.clone(),
        session_id: request.session.session_id.clone(),
        op: request.op,
        status,
        code,
        message: message.map(|m| redact(&m)),
        payload,
        detail: detail.map(|d| redact(&d)),
        elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
    }
}

/// Cap captured output on a char boundary and redact (redaction happens
/// first so the byte cap never splits a redaction marker).
fn cap_text(text: &str) -> String {
    if text.len() <= OUTPUT_CAP {
        return text.to_string();
    }
    let mut end = OUTPUT_CAP;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Bounded, redacted diagnostics text for failure detail fields.
fn diagnostics(text: &str) -> String {
    let redacted = redact(text);
    let mut lines = redacted.lines().map(str::trim).filter(|l| !l.is_empty());
    let mut out = String::new();
    for line in lines.by_ref().take(2) {
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(line);
        if out.len() >= DIAGNOSTIC_CAP {
            break;
        }
    }
    if out.len() > DIAGNOSTIC_CAP {
        let mut end = DIAGNOSTIC_CAP;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::validate_bytes;

    fn env_with_path(paths: &[&str]) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), paths.join(":"));
        env
    }

    fn sample_identity() -> AgentIdentity {
        bind_identity("ws-session-7", "tty-7-main", 3).expect("bound")
    }

    fn sample_session() -> SessionHandle {
        new_session("sess-20260906-0001", sample_identity()).expect("session")
    }

    #[test]
    fn closed_kind_set_and_official_metadata_are_consistent() {
        assert_eq!(HarnessKind::OFFICIAL.len(), 4);
        for kind in HarnessKind::OFFICIAL {
            assert_eq!(HarnessKind::parse(kind.name()), Some(kind));
            let spec = official_spec(kind).expect("official spec");
            assert_eq!(spec.kind, kind);
            assert!(is_actor(spec.actor));
            assert!(spec.range.accepts(spec.range.current));
            assert!(!spec.capabilities.is_empty());
            for cap in spec.capabilities {
                assert!(HARNESS_CAPS.contains(cap));
            }
        }
        assert_eq!(HarnessKind::parse("teleport"), None);
        assert_eq!(HarnessKind::parse("OpenCode"), None);
        assert_eq!(HarnessKind::parse("argv"), Some(HarnessKind::Argv));
    }

    #[test]
    fn official_declarations_validate_and_declare_the_closed_sets() {
        for kind in HarnessKind::OFFICIAL {
            let profile = Profile::official(kind, kind.name()).expect("profile");
            let doc = profile.declaration_doc().expect("declaration");
            let verdict = validate_doc(Family::Capability, &doc);
            assert!(verdict.is_accepted(), "{}", verdict.message());
            let caps = doc
                .get("capabilities")
                .and_then(|c| c.as_array())
                .map(|items| items.len())
                .unwrap_or(0);
            assert_eq!(caps, HARNESS_CAPS.len());
        }
    }

    #[test]
    fn argv_profile_requires_explicit_closed_capabilities() {
        let empty = Profile::argv("cli", "hf-cli", &[], BTreeMap::new());
        assert_eq!(empty.err().map(|e| e.code), Some(CODE_UNKNOWN_CAPABILITY));
        let teleport = Profile::argv("cli", "hf-cli", &["teleport"], BTreeMap::new());
        assert_eq!(
            teleport.err().map(|e| e.code),
            Some(CODE_UNKNOWN_CAPABILITY)
        );
        let path_exe = Profile::argv("cli", "/abs/path", &["prompt"], BTreeMap::new());
        assert_eq!(path_exe.err().map(|e| e.code), Some(CODE_BAD_REQUEST));
        let mut ops = BTreeMap::new();
        ops.insert("prompt".to_string(), vec!["-p".to_string()]);
        let profile = Profile::argv("cli", "hf-cli", &["start", "prompt"], ops).expect("profile");
        assert!(profile.supports("start"));
        assert!(profile.supports("prompt"));
        assert!(!profile.supports("observe"));
        let doc = profile.declaration_doc().expect("declaration");
        assert!(validate_doc(Family::Capability, &doc).is_accepted());
    }

    #[test]
    fn config_profiles_parse_official_kinds_and_refuse_unknown_ones() {
        let harness = crate::config::Harness {
            key: "codex-a".to_string(),
            kind: "codex".to_string(),
            executable: "codex".to_string(),
            env_allow: vec!["PATH".to_string()],
        };
        let profile = Profile::from_config(&harness).expect("profile");
        assert_eq!(profile.kind, HarnessKind::Codex);
        assert!(profile.supports("prompt"));
        assert_eq!(profile.actor, "codex");

        let unknown = crate::config::Harness {
            key: "wat".to_string(),
            kind: "teleport".to_string(),
            executable: "wat".to_string(),
            env_allow: vec![],
        };
        let err = Profile::from_config(&unknown).expect_err("refused");
        assert_eq!(err.code, CODE_UNKNOWN_HARNESS);

        let argv = crate::config::Harness {
            key: "cli".to_string(),
            kind: "argv".to_string(),
            executable: "hf-cli".to_string(),
            env_allow: vec!["PATH".to_string()],
        };
        let profile = Profile::from_config(&argv).expect("profile");
        assert_eq!(profile.kind, HarnessKind::Argv);
        assert!(
            !profile.supports("prompt"),
            "config v1 declares no argv capabilities"
        );
    }

    #[test]
    fn bind_identity_requires_all_three_parts_and_labels_cannot_substitute() {
        let ok = bind_identity("ws-1", "tty-1", 0).expect("bound");
        assert_eq!(ok.generation, 0);
        for (session, terminal) in [
            ("", "tty-1"),
            ("ws-1", ""),
            ("bad id/with slash", "tty-1"),
            ("ws-1", "not a tty id either!"),
        ] {
            let err = bind_identity(session, terminal, 0).expect_err("refused");
            assert_eq!(err.code, CODE_INCOMPLETE_IDENTITY);
        }
        let missing_session = bind_identity("", "tty-1", 3);
        assert_eq!(
            missing_session.err().map(|e| e.code),
            Some(CODE_INCOMPLETE_IDENTITY)
        );
        let missing_terminal = bind_identity("ws-1", "", 3);
        assert_eq!(
            missing_terminal.err().map(|e| e.code),
            Some(CODE_INCOMPLETE_IDENTITY)
        );
        // A mutable pane label is not an accepted part anywhere: the type
        // has exactly three fields and no label constructor exists, so a
        // label-only identity cannot be expressed.
        let identity = ok;
        assert_eq!(identity.herdr_session, "ws-1");
        assert_eq!(identity.terminal_session, "tty-1");
        assert_eq!(identity.generation, 0);
    }

    #[test]
    fn version_ranges_accept_at_and_above_minimum() {
        let range = VersionRange {
            minimum: (2, 1, 263),
            current: (2, 1, 263),
        };
        assert!(range.accepts((2, 1, 263)));
        assert!(range.accepts((2, 1, 300)));
        assert!(!range.accepts((2, 1, 262)));
        assert!(!range.accepts((1, 0, 0)));
    }

    #[test]
    fn resolve_executable_uses_the_allowlisted_path_only() {
        let env = env_with_path(&["/definitely/not/a/real/dir"]);
        let err = resolve_executable("hermes", &env).expect_err("absent");
        assert_eq!(err.code, CODE_UNAVAILABLE);
        assert!(err.message.contains("not found"));
        let no_path = BTreeMap::new();
        let err = resolve_executable("hermes", &no_path).expect_err("no PATH");
        assert_eq!(err.code, CODE_UNAVAILABLE);
        let slashed = resolve_executable("/etc/hosts", &env);
        assert_eq!(slashed.err().map(|e| e.code), Some(CODE_BAD_REQUEST));
    }

    #[test]
    fn start_op_binds_and_reports_the_session_without_a_subprocess() {
        let profile = Profile::official(HarnessKind::Hermes, "h1").expect("profile");
        let session = sample_session();
        let request = OpRequest {
            op: Op::Start,
            session: &session,
            payload: None,
            timeout: Duration::from_secs(1),
        };
        let result = execute_op(&profile, &request, &BTreeMap::new());
        assert_eq!(result.status, "succeeded");
        assert_eq!(result.elapsed_ms, 0);
        let doc = result.to_outcome_doc(
            "hf_plan_0123456789abcdef",
            "p3",
            "ik_apply-20260906-0001",
            "2026-09-06T00:00:00Z",
        );
        let bytes = crate::canonical::canonical_bytes(&doc);
        let verdict = validate_bytes(Family::Outcome, &bytes);
        assert!(verdict.is_accepted(), "{}", verdict.message());
    }

    #[test]
    fn declined_capabilities_refuse_without_running_anything() {
        // A config-declared argv profile declares no capabilities: every
        // operation is a typed refusal, not a subprocess attempt.
        let harness = crate::config::Harness {
            key: "cli".to_string(),
            kind: "argv".to_string(),
            executable: "hf-cli".to_string(),
            env_allow: vec![],
        };
        let profile = Profile::from_config(&harness).expect("profile");
        let session = sample_session();
        let request = OpRequest {
            op: Op::Prompt,
            session: &session,
            payload: Some("do the thing"),
            timeout: Duration::from_secs(1),
        };
        let result = execute_op(&profile, &request, &BTreeMap::new());
        assert_eq!(result.status, "refused");
        assert_eq!(result.code, Some(CODE_UNKNOWN_CAPABILITY));
    }

    #[test]
    fn payload_is_only_valid_on_prompt_and_prompt_requires_payload() {
        let profile = Profile::official(HarnessKind::Codex, "c1").expect("profile");
        let session = sample_session();
        let with_payload = OpRequest {
            op: Op::Observe,
            session: &session,
            payload: Some("nope"),
            timeout: Duration::from_secs(1),
        };
        let result = execute_op(&profile, &with_payload, &BTreeMap::new());
        assert_eq!(result.code, Some(CODE_BAD_REQUEST));
        let no_payload = OpRequest {
            op: Op::Prompt,
            session: &session,
            payload: None,
            timeout: Duration::from_secs(1),
        };
        let result = execute_op(&profile, &no_payload, &BTreeMap::new());
        assert_eq!(result.code, Some(CODE_BAD_REQUEST));
    }

    #[test]
    fn unknown_op_names_are_refused_at_the_named_boundary() {
        let profile = Profile::official(HarnessKind::Hermes, "h1").expect("profile");
        let session = sample_session();
        let result = execute_named(
            &profile,
            "teleport",
            &session,
            None,
            Duration::from_secs(1),
            &BTreeMap::new(),
        );
        assert_eq!(result.status, "refused");
        assert_eq!(result.code, Some(CODE_UNKNOWN_CAPABILITY));
        assert!(result.message.unwrap().contains("teleport"));
    }

    #[test]
    fn error_docs_validate_against_the_error_family() {
        for err in [
            AdapterError::refusal(CODE_UNKNOWN_HARNESS, "unknown kind"),
            AdapterError::refusal(CODE_STALE_IDENTITY, "stale"),
            AdapterError::failure(CODE_TIMEOUT, "timeout", true),
        ] {
            let doc = err.to_error_doc();
            let bytes = crate::canonical::canonical_bytes(&doc);
            let verdict = validate_bytes(Family::Error, &bytes);
            assert!(verdict.is_accepted(), "{}", verdict.message());
            assert!(matches!(doc.get("code"), Some(Val::Str(c)) if c == err.code));
        }
    }

    #[test]
    fn read_back_detects_each_stale_identity_part() {
        // identity read-back compares the workspace doc against the bound
        // triple; here the workspace is emulated by a doc (the subprocess
        // path is covered in tests/harness_adapters.rs).
        let identity = bind_identity("ws-7", "tty-7", 3).expect("bound");
        let session = new_session("sess-1", identity).expect("session");
        let doc = object(vec![
            ("session_id", string("ws-7")),
            ("terminal_session", string("tty-7")),
            ("generation", integer(3)),
            ("state", string("running")),
        ]);
        let read = read_back_identity(&doc, &session).expect("matches");
        assert_eq!(read.get("generation"), Some(&Val::Int(3)));

        for (field, doc) in [
            (
                "herdr_session",
                object(vec![
                    ("session_id", string("ws-OTHER")),
                    ("terminal_session", string("tty-7")),
                    ("generation", integer(3)),
                ]),
            ),
            (
                "terminal_session",
                object(vec![
                    ("session_id", string("ws-7")),
                    ("terminal_session", string("tty-OTHER")),
                    ("generation", integer(3)),
                ]),
            ),
            (
                "generation",
                object(vec![
                    ("session_id", string("ws-7")),
                    ("terminal_session", string("tty-7")),
                    ("generation", integer(4)),
                ]),
            ),
        ] {
            let err = read_back_identity(&doc, &session).expect_err("stale");
            assert_eq!(err.code, CODE_STALE_IDENTITY);
            assert!(err.message.contains(field), "{} names {field}", err.message);
        }
    }

    #[test]
    fn herdr_pane_context_requires_herdr_env_and_pane_id() {
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), "/bin".to_string());
        assert_eq!(herdr_pane_context(&env), None, "no herdr markers");
        env.insert("HERDR_ENV".to_string(), "1".to_string());
        assert_eq!(herdr_pane_context(&env), None, "env without pane id");
        env.insert("HERDR_PANE_ID".to_string(), "w1:p2".to_string());
        assert_eq!(herdr_pane_context(&env), Some("w1:p2"));
        env.insert("HERDR_ENV".to_string(), "0".to_string());
        assert_eq!(herdr_pane_context(&env), None, "HERDR_ENV=0 is not a pane");
        env.insert("HERDR_ENV".to_string(), "1".to_string());
        env.insert("HERDR_PANE_ID".to_string(), "".to_string());
        assert_eq!(herdr_pane_context(&env), None, "empty pane id");
    }

    #[test]
    fn herdr_lifecycle_report_maps_typed_results_to_semantic_states() {
        let profile = Profile::official(HarnessKind::Pi, "pi").expect("profile");
        let session = sample_session();
        let started = std::time::Instant::now();
        let result_for = |op: Op, status: &'static str, code: Option<&'static str>| -> OpResult {
            op_result(
                &profile,
                &OpRequest {
                    op,
                    session: &session,
                    payload: if op == Op::Prompt { Some("x") } else { None },
                    timeout: Duration::from_secs(1),
                },
                status,
                code,
                None,
                None,
                None,
                started,
            )
        };
        // start succeeded -> working; terminal prompts -> idle except
        // credentials -> blocked; workspace ops -> no report.
        assert_eq!(
            herdr_lifecycle_report(&result_for(Op::Start, "succeeded", None)),
            Some(("working", None))
        );
        assert_eq!(
            herdr_lifecycle_report(&result_for(Op::Start, "refused", Some(CODE_BAD_REQUEST))),
            None
        );
        assert_eq!(
            herdr_lifecycle_report(&result_for(Op::Prompt, "succeeded", None)),
            Some(("idle", None))
        );
        assert_eq!(
            herdr_lifecycle_report(&result_for(Op::Prompt, "ambiguous", Some(CODE_TIMEOUT))),
            Some(("idle", None))
        );
        assert_eq!(
            herdr_lifecycle_report(&result_for(Op::Prompt, "refused", Some(CODE_CREDENTIALS))),
            Some(("blocked", Some("harness credentials required")))
        );
        assert_eq!(
            herdr_lifecycle_report(&result_for(Op::Prompt, "failed", Some(CODE_EXIT))),
            Some(("idle", None))
        );
        assert_eq!(
            herdr_lifecycle_report(&result_for(Op::Outcome, "succeeded", None)),
            None
        );
        assert_eq!(
            herdr_lifecycle_report(&result_for(Op::Identity, "succeeded", None)),
            None
        );
    }
}
