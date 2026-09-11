//! CLI command parsing and execution.
//!
//! Every stable command emits one `hf-output/v1` envelope when `--json` is
//! requested (stdout = the JSON document only; diagnostics stay on stderr;
//! JSON mode never prompts). Human output is a rendering of the same data —
//! never a second, contradicting contract. Exit codes follow
//! docs/contracts/spec-cli.md: 0 ok, 1 operational error, 2 usage, 3
//! partial, 4 refusal, 5 config/policy error.
//!
//! Untrusted input handling: repository identities always come from the
//! validated config document (argv may only select a configured key or
//! identity), issue numbers must be digits, revisions must be 40-hex, and
//! captured adapter output is redacted at the boundary and never feeds back
//! into argv, framing, policy, or identity.

use std::path::PathBuf;

use crate::canonical::canonical_bytes;
use crate::client::{self, RpcError};
use crate::config::{
    Config, LoadError, adapter_environment, default_config_hint, discover_config, init_template,
    load_config, resolve_repository,
};
use crate::daemon::{self, DaemonError};
use crate::dirs::DaemonPaths;
use crate::formats::is_hex40;
use crate::lock::describe_socket;
use crate::observe::{
    HERDR_MINIMUM, Observation, acceptance_revision, gh_issue_text, observe_all, observe_herdr,
    probe_gh_auth, probe_version,
};
use crate::plan::{DOCTRINE_WORKFLOW_ID, PlanInput, render_plan};
use crate::service::{self, LAUNCHD_LABEL, SYSTEMD_UNIT_NAME};
use crate::value::{Val, bool_, integer, null, object, string};

/// Top-level usage text (also the `--help` output body).
pub const USAGE: &str = "\
herdr-fleet — typed, plan-first companion CLI for Herdr coding-agent fleets

READ-ONLY CORE: this binary observes configuration, prerequisites, and
repository state and renders deterministic plans. It never installs, starts,
stops, or upgrades Herdr; it never mutates repositories or fleet state; and
it never stores credentials (GitHub reads use the invoking environment's
authenticated `gh`).

USAGE:
    herdr-fleet --help
    herdr-fleet --version
    herdr-fleet config init
    herdr-fleet config validate [--config PATH] [--json]
    herdr-fleet config show [--config PATH] [--json]
    herdr-fleet doctor [--json]
    herdr-fleet status [--config PATH] [--json]
    herdr-fleet plan <repository> <issue> [--revision HEX40] [--config PATH] [--json]
    herdr-fleet capabilities [--json]
    herdr-fleet daemon run [--socket PATH] [--config PATH]
    herdr-fleet daemon status [--config PATH] [--json]
    herdr-fleet service doctor [--config PATH] [--json]
    herdr-fleet service install-plan [--config PATH] [--json]
    herdr-fleet service status-plan [--config PATH] [--json]
    herdr-fleet service uninstall-plan [--config PATH] [--json]

GLOBAL OPTIONS:
    -h, --help       Print help (add --help to any command for its usage).
    -V, --version    Print the package name, version, description, and schema facts.

COMMANDS:
    config init      Print an annotated hf-config/v1 template to stdout.
    config validate  Validate the config file and its named policy overlay.
    config show      Inspect the effective configuration.
    doctor           Diagnose prerequisites; never installs/starts/stops.
    status           Observe configured repositories (read-only, bounded).
    plan             Render a deterministic read-only hf-plan/v1 plan.
    capabilities     Report the CLI's declared forge read capabilities.
    daemon           Run or probe the single-writer state daemon.
    service          Render per-user launchd/systemd plans; doctor checks.

EXIT CODES (with or without --json):
    0 ok · 1 operational error · 2 usage · 3 partial · 4 refusal · 5 config error

JSON MODE: every command that accepts --json writes exactly one hf-output/v1
document to stdout; diagnostics go to stderr; JSON output never prompts.
";

/// Parsed top-level command line.
#[derive(Clone, Debug)]
pub struct Invocation {
    /// Canonical command name for the envelope `command` field.
    pub command: String,
    /// Whether `--json` was requested.
    pub json: bool,
    /// Explicit `--config` path.
    pub config_path: Option<PathBuf>,
    /// Per-command arguments (plan only).
    pub plan: Option<PlanArgs>,
    /// Config subcommand kind (`init`/`validate`/`show`).
    pub config_action: Option<ConfigAction>,
    /// Daemon subcommand (`run`/`status`).
    pub daemon_action: Option<DaemonAction>,
    /// Service subcommand (doctor/plan actions).
    pub service_action: Option<ServiceAction>,
}

/// Daemon subcommands (issue #5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonAction {
    /// Run the daemon in the foreground (`daemon run [--socket PATH]`).
    Run {
        /// Socket path override (`--socket PATH`).
        socket: Option<String>,
    },
    /// Probe the daemon socket and report state
    /// (`daemon status [--socket PATH]`).
    Status {
        /// Socket path override (`--socket PATH`).
        socket: Option<String>,
    },
}

/// Service-management subcommands: pure checks and rendered plans (never
/// activate the host service manager).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceAction {
    /// Diagnose the daemon/service environment (read-only).
    Doctor,
    /// Render the install plan (unit text + command steps).
    PlanInstall,
    /// Render the status-check plan.
    PlanStatus,
    /// Render the uninstall plan.
    PlanUninstall,
}

/// Config subcommands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigAction {
    /// Print the annotated template.
    Init,
    /// Validate the config (+ overlay).
    Validate,
    /// Show the effective config.
    Show,
}

/// Plan command arguments (validated during parsing).
#[derive(Clone, Debug)]
pub struct PlanArgs {
    /// Repository argument: a configured key or its owner/name identity.
    pub repository: String,
    /// Issue number (digits only, positive).
    pub issue_number: u64,
    /// Explicit acceptance revision (40-hex) when provided.
    pub revision: Option<String>,
}

/// Kind of an `hf-output/v1` envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `ok` — complete result with data.
    Ok,
    /// `partial` — some targets observed, some not (never hidden).
    Partial,
    /// `error` — typed error.
    Error,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Ok => "ok",
            Kind::Partial => "partial",
            Kind::Error => "error",
        }
    }
}

/// Typed error document (`hf-error/v1` shape, embedded in error envelopes).
#[derive(Clone, Debug)]
pub struct ErrorDoc {
    /// Stable lowercase-dotted code.
    pub code: String,
    /// Human message.
    pub message: String,
    /// Whether retrying may help.
    pub retryable: bool,
    /// Optional structured details.
    pub details: Option<Val>,
}

/// Result of executing one command.
#[derive(Clone, Debug)]
pub struct CmdResult {
    /// Process exit code (also carried inside the JSON envelope).
    pub exit_code: u8,
    /// Envelope kind.
    pub kind: Kind,
    /// `data` object for ok/partial results.
    pub data: Option<Val>,
    /// Error document for error results.
    pub error: Option<ErrorDoc>,
    /// Human rendering for stdout (empty for error results).
    pub human: String,
    /// Diagnostics for stderr (both modes).
    pub diagnostics: String,
}

fn ok_result(data: Val, human: String) -> CmdResult {
    CmdResult {
        exit_code: 0,
        kind: Kind::Ok,
        data: Some(data),
        error: None,
        human,
        diagnostics: String::new(),
    }
}

fn partial_result(data: Val, human: String, diagnostics: String) -> CmdResult {
    CmdResult {
        exit_code: 3,
        kind: Kind::Partial,
        data: Some(data),
        error: None,
        human,
        diagnostics,
    }
}

fn error_result(exit_code: u8, code: &str, message: String, retryable: bool) -> CmdResult {
    CmdResult {
        exit_code,
        kind: Kind::Error,
        data: None,
        error: Some(ErrorDoc {
            code: code.to_string(),
            message: message.clone(),
            retryable,
            details: None,
        }),
        human: String::new(),
        diagnostics: message,
    }
}

fn error_result_details(
    exit_code: u8,
    code: &str,
    message: String,
    retryable: bool,
    details: Val,
) -> CmdResult {
    let mut result = error_result(exit_code, code, message, retryable);
    if let Some(error) = &mut result.error {
        error.details = Some(details);
    }
    result
}

/// Outcome of parsing: an invocation, a help request (exit 0, stdout), or a
/// usage error (exit 2, stderr).
#[derive(Clone, Debug)]
pub enum ParseError {
    /// User asked for help: print text to stdout and exit 0.
    Help(String),
    /// Malformed argv: print message to stderr and exit 2.
    Usage(String),
}

/// Parse the full argv (after the program name).
pub fn parse_invocation(args: &[String]) -> Result<Invocation, ParseError> {
    let command = args
        .first()
        .ok_or_else(|| ParseError::Usage("missing command".to_string()))?;
    let rest: Vec<&String> = args[1..].iter().collect();
    match command.as_str() {
        "config" => parse_config(&rest),
        "doctor" => parse_flag_command("doctor", &rest),
        "status" => parse_flag_command("status", &rest),
        "capabilities" => parse_flag_command("capabilities", &rest),
        "plan" => parse_plan(&rest),
        "daemon" => parse_daemon(&rest),
        "service" => parse_service(&rest),
        other => Err(ParseError::Usage(format!("unknown command {other:?}"))),
    }
}

/// Shared flag loop for commands with no positionals (doctor/status/
/// capabilities).
fn parse_flag_command(name: &str, args: &[&String]) -> Result<Invocation, ParseError> {
    let mut json = false;
    let mut config_path: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--config" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    ParseError::Usage(format!("{name}: --config requires a path argument"))
                })?;
                config_path = Some(PathBuf::from(value));
            }
            "-h" | "--help" => return Err(ParseError::Help(help_request(name))),
            flag => {
                return Err(ParseError::Usage(format!(
                    "{name}: unknown flag {flag:?}; run `herdr-fleet {name} --help`"
                )));
            }
        }
        index += 1;
    }
    Ok(Invocation {
        command: name.to_string(),
        json,
        config_path,
        plan: None,
        config_action: None,
        daemon_action: None,
        service_action: None,
    })
}

/// Per-command help texts.
fn help_request(name: &str) -> String {
    match name {
        "doctor" => DOCTOR_USAGE.trim_end().to_string(),
        "status" => STATUS_USAGE.trim_end().to_string(),
        "capabilities" => CAPABILITIES_USAGE.trim_end().to_string(),
        "plan" => PLAN_USAGE.trim_end().to_string(),
        "config" => CONFIG_USAGE.trim_end().to_string(),
        "daemon" => DAEMON_USAGE.trim_end().to_string(),
        "service" => SERVICE_USAGE.trim_end().to_string(),
        _ => USAGE.to_string(),
    }
}

const DOCTOR_USAGE: &str = "\
herdr-fleet doctor — diagnose prerequisites (read-only; never installs,
starts, stops, or upgrades Herdr, gh, git, or any harness)

USAGE:
    herdr-fleet doctor [--json]

Checks: git presence, herdr presence + version vs the declared minimum
0.8.2, gh presence + authentication + reported scopes, and the config file
(when present). Exits 0 when every check is ok, 3 when any prerequisite is
missing or degraded, and 5 when a found config/policy document is invalid.
";

const STATUS_USAGE: &str = "\
herdr-fleet status — observe configured repositories (read-only)

USAGE:
    herdr-fleet status [--config PATH] [--json]

Observes each enabled configured repository through the local git checkout
in the invoking directory and authenticated `gh` from the invoking
environment, plus a herdr presence/version probe. Observation is bounded
(at most 4 concurrent observers, 10s per process); freshness, completeness,
and every partial failure are explicit in the output. Exits 3 when any
observation is partial; 5 for config errors.
";

const CAPABILITIES_USAGE: &str = "\
herdr-fleet capabilities — report the CLI's declared forge read capabilities

USAGE:
    herdr-fleet capabilities [--json]

Emits the hf-capability/v1 declaration for this read-only CLI: axis
\"forge\", actor \"herdr-fleet\", capabilities [\"read_refs\", \"read_issues\",
\"read_checks\"]. No negotiation or shell guessing is performed.
";

const PLAN_USAGE: &str = "\
herdr-fleet plan <repository> <issue> — render a deterministic read-only plan

USAGE:
    herdr-fleet plan <repository> <issue> [--revision HEX40] [--config PATH] [--json]

<repository> names a configured repository (its config key or owner/name
identity — identities always come from the config, never from free text).
<issue> is a positive integer. The acceptance revision is either given with
--revision or derived from the issue's live acceptance text through gh; when
gh is unavailable and no --revision is given, the plan is refused explicitly
(exit 1, code forge.unavailable).

The rendered hf-plan/v1 document is deterministic: the same inputs always
produce the same canonical bytes and sha256 digest. state_epoch is 0 (no
daemon state in this slice). No plan is ever applied by this command.
";

const CONFIG_USAGE: &str = "\
herdr-fleet config <init|validate|show> — configuration guidance/inspection

USAGE:
    herdr-fleet config init
    herdr-fleet config validate [--config PATH] [--json]
    herdr-fleet config show [--config PATH] [--json]

init prints an annotated hf-config/v1 template to stdout (the CLI never
writes files). validate and show load the config at --config PATH or the XDG
default ($XDG_CONFIG_HOME/herdr-fleet/config.toml or
~/.config/herdr-fleet/config.toml) and its explicitly named policy overlay.
Unknown keys, foreign/missing schema identifiers, unsupported versions, and
invalid overlay content are refused with typed refusals (exit 5).
";

fn parse_config(args: &[&String]) -> Result<Invocation, ParseError> {
    let action = args
        .first()
        .ok_or_else(|| ParseError::Help(help_request("config")))?;
    let action = match action.as_str() {
        "init" => ConfigAction::Init,
        "validate" => ConfigAction::Validate,
        "show" => ConfigAction::Show,
        "-h" | "--help" => return Err(ParseError::Help(help_request("config"))),
        other => {
            return Err(ParseError::Usage(format!(
                "config: unknown subcommand {other:?}"
            )));
        }
    };
    let mut json = false;
    let mut config_path: Option<PathBuf> = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--config" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    ParseError::Usage("config: --config requires a path argument".to_string())
                })?;
                config_path = Some(PathBuf::from(value));
            }
            "-h" | "--help" => return Err(ParseError::Help(help_request("config"))),
            flag => {
                return Err(ParseError::Usage(format!(
                    "config: unknown flag {flag:?}; run `herdr-fleet config --help`"
                )));
            }
        }
        index += 1;
    }
    Ok(Invocation {
        command: match action {
            ConfigAction::Init => "config init",
            ConfigAction::Validate => "config validate",
            ConfigAction::Show => "config show",
        }
        .to_string(),
        json,
        config_path,
        plan: None,
        config_action: Some(action),
        daemon_action: None,
        service_action: None,
    })
}

/// Parse `daemon <run|status>`.
fn parse_daemon(args: &[&String]) -> Result<Invocation, ParseError> {
    let action = args
        .first()
        .ok_or_else(|| ParseError::Help(help_request("daemon")))?;
    let mut socket: Option<String> = None;
    let mut json = false;
    let mut config_path: Option<PathBuf> = None;
    let action = match action.as_str() {
        "run" => {
            let mut index = 1;
            while index < args.len() {
                match args[index].as_str() {
                    "--socket" => {
                        index += 1;
                        let value = args.get(index).ok_or_else(|| {
                            ParseError::Usage("daemon run: --socket requires a path".to_string())
                        })?;
                        socket = Some(value.to_string());
                    }
                    "--config" => {
                        index += 1;
                        let value = args.get(index).ok_or_else(|| {
                            ParseError::Usage("daemon run: --config requires a path".to_string())
                        })?;
                        config_path = Some(PathBuf::from(value));
                    }
                    "--json" => json = true,
                    "-h" | "--help" => return Err(ParseError::Help(help_request("daemon"))),
                    flag => {
                        return Err(ParseError::Usage(format!(
                            "daemon run: unknown flag {flag:?}; run `herdr-fleet daemon --help`"
                        )));
                    }
                }
                index += 1;
            }
            DaemonAction::Run { socket }
        }
        "status" => {
            let mut index = 1;
            while index < args.len() {
                match args[index].as_str() {
                    "--socket" => {
                        index += 1;
                        let value = args.get(index).ok_or_else(|| {
                            ParseError::Usage("daemon status: --socket requires a path".to_string())
                        })?;
                        socket = Some(value.to_string());
                    }
                    "--config" => {
                        index += 1;
                        let value = args.get(index).ok_or_else(|| {
                            ParseError::Usage("daemon status: --config requires a path".to_string())
                        })?;
                        config_path = Some(PathBuf::from(value));
                    }
                    "--json" => json = true,
                    "-h" | "--help" => return Err(ParseError::Help(help_request("daemon"))),
                    flag => {
                        return Err(ParseError::Usage(format!(
                            "daemon status: unknown flag {flag:?}; run `herdr-fleet daemon --help`"
                        )));
                    }
                }
                index += 1;
            }
            DaemonAction::Status { socket }
        }
        "-h" | "--help" => return Err(ParseError::Help(help_request("daemon"))),
        other => {
            return Err(ParseError::Usage(format!(
                "daemon: unknown subcommand {other:?}; run `herdr-fleet daemon --help`"
            )));
        }
    };
    Ok(Invocation {
        command: match &action {
            DaemonAction::Run { .. } => "daemon run".to_string(),
            DaemonAction::Status { .. } => "daemon status".to_string(),
        },
        json,
        config_path,
        plan: None,
        config_action: None,
        daemon_action: Some(action),
        service_action: None,
    })
}

/// Parse `service <doctor|install-plan|status-plan|uninstall-plan>`.
fn parse_service(args: &[&String]) -> Result<Invocation, ParseError> {
    let action = args
        .first()
        .ok_or_else(|| ParseError::Help(help_request("service")))?;
    let action = match action.as_str() {
        "doctor" => ServiceAction::Doctor,
        "install-plan" => ServiceAction::PlanInstall,
        "status-plan" => ServiceAction::PlanStatus,
        "uninstall-plan" => ServiceAction::PlanUninstall,
        "-h" | "--help" => return Err(ParseError::Help(help_request("service"))),
        other => {
            return Err(ParseError::Usage(format!(
                "service: unknown subcommand {other:?}; run `herdr-fleet service --help`"
            )));
        }
    };
    let mut json = false;
    let mut config_path: Option<PathBuf> = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--config" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    ParseError::Usage("service: --config requires a path argument".to_string())
                })?;
                config_path = Some(PathBuf::from(value));
            }
            "-h" | "--help" => return Err(ParseError::Help(help_request("service"))),
            flag => {
                return Err(ParseError::Usage(format!(
                    "service: unknown flag {flag:?}; run `herdr-fleet service --help`"
                )));
            }
        }
        index += 1;
    }
    let command = match action {
        ServiceAction::Doctor => "service doctor",
        ServiceAction::PlanInstall => "service install-plan",
        ServiceAction::PlanStatus => "service status-plan",
        ServiceAction::PlanUninstall => "service uninstall-plan",
    };
    Ok(Invocation {
        command: command.to_string(),
        json,
        config_path,
        plan: None,
        config_action: None,
        daemon_action: None,
        service_action: Some(action),
    })
}

fn parse_plan(args: &[&String]) -> Result<Invocation, ParseError> {
    let mut json = false;
    let mut config_path: Option<PathBuf> = None;
    let mut revision: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--config" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    ParseError::Usage("plan: --config requires a path argument".to_string())
                })?;
                config_path = Some(PathBuf::from(value));
            }
            "--revision" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    ParseError::Usage("plan: --revision requires a 40-hex value".to_string())
                })?;
                if !is_hex40(value) {
                    return Err(ParseError::Usage(format!(
                        "plan: --revision must be 40 lowercase hex, got {value:?}"
                    )));
                }
                revision = Some(value.to_string());
            }
            "-h" | "--help" => return Err(ParseError::Help(help_request("plan"))),
            flag if flag.starts_with('-') => {
                return Err(ParseError::Usage(format!(
                    "plan: unknown flag {flag:?}; run `herdr-fleet plan --help`"
                )));
            }
            positional => positionals.push(positional.to_string()),
        }
        index += 1;
    }
    if positionals.len() != 2 {
        return Err(ParseError::Usage(format!(
            "plan: expected <repository> <issue>, got {} positional argument(s)",
            positionals.len()
        )));
    }
    let issue_number = match positionals[1].parse::<u64>() {
        Ok(number) if number > 0 => number,
        _ => {
            return Err(ParseError::Usage(format!(
                "plan: issue must be a positive integer, got {:?}",
                positionals[1]
            )));
        }
    };
    Ok(Invocation {
        command: "plan".to_string(),
        json,
        config_path,
        daemon_action: None,
        service_action: None,
        plan: Some(PlanArgs {
            repository: positionals[0].clone(),
            issue_number,
            revision,
        }),
        config_action: None,
    })
}

/// Load the config required by a command, mapping failures onto exit-5
/// error results with stable codes. The error is boxed so `required_config`
/// stays small (clippy result-large-err).
fn required_config(config_path: &Option<PathBuf>) -> Result<Config, Box<CmdResult>> {
    match discover_config(config_path.as_deref()) {
        Ok(Some(path)) => match load_config(&path) {
            Ok(config) => Ok(config),
            Err(load_error) => Err(Box::new(load_error_result(load_error))),
        },
        Ok(None) => Err(Box::new(error_result(
            5,
            "config.not_found",
            format!(
                "no config file found (looked for {}); run `herdr-fleet config init` and save the template there",
                default_config_hint()
            ),
            false,
        ))),
        Err(load_error) => Err(Box::new(load_error_result(load_error))),
    }
}

fn load_error_result(error: LoadError) -> CmdResult {
    let mut pairs: Vec<(&str, Val)> = Vec::new();
    if let Some(refusal) = error.refusal {
        pairs.push(("refusal", string(refusal.as_str())));
    }
    if !error.path.is_empty() {
        pairs.push(("path", string(&error.path)));
    }
    let details = if pairs.is_empty() {
        null()
    } else {
        object(pairs)
    };
    error_result_details(5, error.code, error.message, false, details)
}

/// Render the JSON `hf-output/v1` envelope for a command result.
pub fn render_envelope(command: &str, result: &CmdResult) -> String {
    let mut fields = vec![
        ("schema", string("hf-output/v1")),
        ("command", string(command)),
        ("kind", string(result.kind.as_str())),
        ("exit_code", integer(result.exit_code as i64)),
    ];
    match (&result.kind, &result.data, &result.error) {
        (Kind::Error, _, Some(error)) => {
            fields.push((
                "error",
                object(vec![
                    ("code", string(&error.code)),
                    ("message", string(&error.message)),
                    ("retryable", bool_(error.retryable)),
                    ("details", error.details.clone().unwrap_or_else(null)),
                ]),
            ));
        }
        (Kind::Error, _, None) => {
            fields.push((
                "error",
                object(vec![
                    ("code", string("internal.error")),
                    ("message", string("error result without an error document")),
                    ("retryable", bool_(false)),
                    ("details", null()),
                ]),
            ));
        }
        (_, Some(data), _) => {
            fields.push(("data", data.clone()));
        }
        _ => {
            fields.push(("data", object(vec![])));
        }
    }
    let doc = object(fields);
    let mut out = String::from_utf8(canonical_bytes(&doc)).expect("canonical JSON is ASCII");
    out.push('\n');
    out
}

/// Execute one parsed invocation.
pub fn execute(invocation: &Invocation) -> CmdResult {
    if let Some(action) = invocation.daemon_action.clone() {
        return execute_daemon(action, invocation);
    }
    if let Some(action) = invocation.service_action.clone() {
        return execute_service(action, invocation);
    }
    if let Some(action) = invocation.config_action {
        return execute_config(action, invocation);
    }
    if let Some(plan) = &invocation.plan {
        return execute_plan(plan, invocation);
    }
    match invocation.command.as_str() {
        "doctor" => execute_doctor(invocation),
        "status" => execute_status(invocation),
        "capabilities" => execute_capabilities(invocation),
        other => error_result(
            2,
            "usage.error",
            format!("unknown command {other:?}"),
            false,
        ),
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct DoctorRow {
    name: &'static str,
    status: &'static str,
    detail: Option<String>,
}

impl DoctorRow {
    fn into_val(self) -> Val {
        let mut fields = vec![("name", string(self.name)), ("status", string(self.status))];
        if let Some(detail) = self.detail {
            fields.push(("detail", string(&detail)));
        }
        object(fields)
    }

    fn render(&self) -> String {
        match &self.detail {
            Some(detail) => format!("{:<12}  {:<9}{}\n", self.name, self.status, detail),
            None => format!("{:<12}  {:<9}\n", self.name, self.status),
        }
    }
}

fn execute_doctor(invocation: &Invocation) -> CmdResult {
    // Config row: an invalid found config is a hard config error (exit 5).
    let (config_status, config_path) = match discover_config(invocation.config_path.as_deref()) {
        Ok(Some(path)) => match load_config(&path) {
            Ok(_) => ("ok", Some(path)),
            Err(load_error) => return load_error_result(load_error),
        },
        Ok(None) => ("absent", None),
        Err(load_error) => return load_error_result(load_error),
    };

    let env = adapter_environment();
    let mut rows: Vec<DoctorRow> = Vec::new();

    // git presence.
    let (git_present, git_version, git_detail, _) = probe_version("git", &env);
    let git_detail = match (git_present, git_version) {
        (true, Some(version)) => format!("git version {version}"),
        (false, _) => "git not found on PATH — git is needed for local checkout reads".to_string(),
        (true, None) => git_detail.unwrap_or_else(|| "git version could not be parsed".to_string()),
    };
    rows.push(DoctorRow {
        name: "git",
        status: if git_present { "ok" } else { "missing" },
        detail: Some(git_detail),
    });

    // herdr presence + version + compatibility (never installed/started).
    let (herdr, _) = observe_herdr(&env);
    let herdr_present = matches!(herdr.payload.get("present"), Some(Val::Bool(true)));
    let herdr_version = herdr.payload.get("version").and_then(Val::as_str);
    let herdr_compatible = matches!(herdr.payload.get("compatible"), Some(Val::Bool(true)));
    let (herdr_status, herdr_detail) = match (herdr_present, herdr_version, herdr_compatible) {
        (false, _, _) => (
            "missing",
            "herdr not found on PATH — herdr-fleet never installs or starts Herdr".to_string(),
        ),
        (true, Some(version), true) => (
            "ok",
            format!(
                "herdr {version} (declared minimum {}.{}.{})",
                HERDR_MINIMUM.0, HERDR_MINIMUM.1, HERDR_MINIMUM.2
            ),
        ),
        (true, Some(version), false) => (
            "degraded",
            format!(
                "herdr {version} is below the declared minimum {}.{}.{}",
                HERDR_MINIMUM.0, HERDR_MINIMUM.1, HERDR_MINIMUM.2
            ),
        ),
        (true, None, _) => {
            let cause = herdr
                .payload
                .get("detail")
                .and_then(Val::as_str)
                .unwrap_or("its version could not be determined");
            ("degraded", format!("herdr present but {cause}"))
        }
    };
    rows.push(DoctorRow {
        name: "herdr",
        status: herdr_status,
        detail: Some(herdr_detail),
    });

    // gh presence + authentication + reported scopes.
    let (gh_present, gh_version, _gh_version_detail, _) = probe_version("gh", &env);
    let gh_version_text = gh_version.as_deref().unwrap_or("unknown");
    let (gh_authed, gh_scopes, gh_auth_detail, _) = probe_gh_auth(&env);
    let (gh_status, gh_detail) = match (gh_present, gh_authed, &gh_scopes) {
        (false, _, _) => (
            "missing",
            "gh not found on PATH — GitHub reads need authenticated gh from the invoking environment"
                .to_string(),
        ),
        (true, false, _) => (
            "degraded",
            format!(
                "gh present but not authenticated{}",
                gh_auth_detail
                    .map(|detail| format!(" ({detail})"))
                    .unwrap_or_default()
            ),
        ),
        (true, true, Some(scopes)) if !scopes.is_empty() => (
            "ok",
            format!("gh {gh_version_text}; authenticated; scopes: {}", scopes.join(", ")),
        ),
        (true, true, _) => (
            "degraded",
            "gh authenticated but token scopes were not reported".to_string(),
        ),
    };
    rows.push(DoctorRow {
        name: "gh",
        status: gh_status,
        detail: Some(gh_detail),
    });

    let config_row = match &config_path {
        Some(path) => format!("config at {}", path.display()),
        None => "no config file (not required by doctor; status/plan need it)".to_string(),
    };

    let degraded = rows.iter().any(|row| row.status != "ok");
    let checks: Vec<Val> = rows.iter().cloned().map(DoctorRow::into_val).collect();

    let mut human = String::new();
    human.push_str("herdr-fleet doctor\n");
    human.push_str(&format!(
        "{:<12}  {:<9}{}\n",
        "config", config_status, config_row
    ));
    for row in &rows {
        human.push_str(&row.render());
    }
    human.push_str("boundary: doctor never installs, upgrades, starts, or stops Herdr or gh\n");

    let data = object(vec![
        (
            "config",
            object(vec![
                ("status", string(config_status)),
                (
                    "path",
                    config_path
                        .as_ref()
                        .map(|path| string(&path.display().to_string()))
                        .unwrap_or_else(null),
                ),
            ]),
        ),
        ("checks", Val::Arr(checks)),
    ]);

    let diagnostics = if degraded {
        let count = rows.iter().filter(|row| row.status != "ok").count();
        format!(
            "doctor: {count} prerequisite check(s) not ok — herdr-fleet never installs, upgrades, starts, or stops Herdr or gh"
        )
    } else {
        String::new()
    };

    if degraded {
        partial_result(data, human.trim_end().to_string(), diagnostics)
    } else {
        ok_result(data, human.trim_end().to_string())
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn execute_status(invocation: &Invocation) -> CmdResult {
    let config = match required_config(&invocation.config_path) {
        Ok(config) => config,
        Err(result) => return *result,
    };
    let repositories = config.effective_repositories();
    let env = adapter_environment();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let observed_at = crate::time::rfc3339_now();

    let (observations, totals) = observe_all(&repositories, &env, &cwd);

    let expected = repositories.len();
    let repo_observations: Vec<&Observation> = observations
        .iter()
        .filter(|observation| observation.subject_type == "repository")
        .collect();
    let observed_repositories = repo_observations
        .iter()
        .filter(|observation| observation.completeness == "complete")
        .count();
    let repo_observations_all_fresh = repo_observations
        .iter()
        .all(|observation| observation.freshness == "fresh");
    // The overall freshness describes the repository read-backs; without any
    // repository observation (nothing configured) it is "unknown".
    let freshness = if expected == 0 {
        "unknown"
    } else if repo_observations_all_fresh {
        "fresh"
    } else {
        "unknown"
    };
    let all_observations_complete = observations
        .iter()
        .all(|observation| observation.completeness == "complete");

    let mut docs = Vec::new();
    for observation in &observations {
        docs.push(observation.to_doc(&observed_at));
    }
    let data = object(vec![
        ("expected_repositories", integer(expected as i64)),
        (
            "observed_repositories",
            integer(observed_repositories as i64),
        ),
        ("freshness", string(freshness)),
        ("observations", Val::Arr(docs)),
        ("timings_ms", totals.timings_obj()),
    ]);

    // Human rendering (same data, stable lines).
    let mut human = String::new();
    if expected == 0 {
        human.push_str("status: no repositories configured; nothing to observe\n");
    } else {
        human.push_str(&format!(
            "status: {observed_repositories} of {expected} repositories observed ({freshness})\n"
        ));
    }
    for observation in &observations {
        human.push_str(&render_observation_human(observation));
    }
    let timings = totals.timings_obj();
    let number = |key: &str| {
        timings
            .get(key)
            .and_then(|value| match value {
                Val::Int(int) => Some(int.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "?".to_string())
    };
    human.push_str(&format!(
        "timings_ms: p50={} p95={} min={} max={} requests={} processes={} partial_failures={}\n",
        number("p50_ms"),
        number("p95_ms"),
        number("min_ms"),
        number("max_ms"),
        number("requests"),
        number("processes"),
        number("partial_failures"),
    ));

    let partial = !all_observations_complete;
    if partial {
        partial_result(
            data,
            human.trim_end().to_string(),
            format!(
                "status: {observed_repositories} of {expected} repositories observed completely; \
                 unavailable surfaces are reported explicitly (partial_failures={})",
                totals.partial_failures
            ),
        )
    } else {
        ok_result(data, human.trim_end().to_string())
    }
}

/// One-line-per-subject human rendering of an observation.
fn render_observation_human(observation: &Observation) -> String {
    match observation.subject_type {
        "repository" => {
            let git = observation.payload.get("git");
            let github = observation.payload.get("github");
            let mut notes: Vec<String> = Vec::new();
            if let Some(git) = git {
                if matches!(git.get("available"), Some(Val::Bool(true))) {
                    notes.push("local checkout".to_string());
                    if let Some(branch) = git.get("branch").and_then(Val::as_str) {
                        notes.push(format!("branch {branch}"));
                    }
                } else {
                    notes.push("no local checkout".to_string());
                }
            }
            if let Some(github) = github {
                if matches!(github.get("available"), Some(Val::Bool(true))) {
                    let default_branch = github
                        .get("default_branch")
                        .and_then(Val::as_str)
                        .unwrap_or("?");
                    notes.push(format!("github default branch {default_branch}"));
                } else {
                    let reason = github
                        .get("reason")
                        .and_then(Val::as_str)
                        .unwrap_or("unavailable");
                    notes.push(format!("github {reason}"));
                }
            }
            format!(
                "  {} [{}/{}]: {}\n",
                observation.subject_id,
                observation.freshness,
                observation.completeness,
                notes.join("; ")
            )
        }
        _ => format!(
            "  {}: {}\n",
            observation.subject_id,
            render_payload_line(&observation.payload)
        ),
    }
}

fn render_payload_line(payload: &Val) -> String {
    match payload.get("present") {
        Some(Val::Bool(true)) => {
            let version = payload.get("version").and_then(Val::as_str).unwrap_or("?");
            let compatible = payload.get("compatible").and_then(Val::as_str);
            match compatible {
                Some("true") => format!("present {version} (compatible)"),
                Some("false") => format!("present {version} (below declared minimum)"),
                _ => format!("present {version}"),
            }
        }
        Some(Val::Bool(false)) => "absent".to_string(),
        _ => "?".to_string(),
    }
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

fn execute_plan(plan: &PlanArgs, invocation: &Invocation) -> CmdResult {
    let config = match required_config(&invocation.config_path) {
        Ok(config) => config,
        Err(result) => return *result,
    };
    let repository = match resolve_repository(&config, &plan.repository) {
        Ok(repository) => repository,
        Err(message) => {
            return error_result(
                2,
                "usage.error",
                format!("plan: {message}; run `herdr-fleet plan --help`"),
                false,
            );
        }
    };

    // Acceptance revision: explicit --revision, or derived live from the
    // issue's acceptance text through gh. gh absence degrades explicitly.
    let (revision, revision_source) = match &plan.revision {
        Some(revision) => (revision.clone(), "argument"),
        None => {
            let env = adapter_environment();
            match gh_issue_text(&repository.owner, &repository.name, plan.issue_number, &env) {
                Ok((title, body)) => {
                    let revision = acceptance_revision(&title, &body);
                    (revision, "github")
                }
                Err(message) => {
                    return error_result(
                        1,
                        "forge.unavailable",
                        format!("{message}; pass --revision <40hex> to render the plan offline"),
                        true,
                    );
                }
            }
        }
    };

    // Workflow pin: prefer a fleet-doctrine-1 pin; a pin naming another
    // workflow is refused (the workflow engine is a later slice).
    let workflow = config
        .workflows
        .iter()
        .find(|pin| pin.id == DOCTRINE_WORKFLOW_ID)
        .or_else(|| config.workflows.first());
    let rendered = match render_plan(&PlanInput {
        repository,
        issue_number: plan.issue_number,
        revision: revision.clone(),
        workflow,
    }) {
        Ok(rendered) => rendered,
        Err(_refusal) => {
            return error_result(
                4,
                "refusal.workflow.unsupported",
                format!(
                    "the configured workflow pin is not supported by this read-only core (only \
                     {DOCTRINE_WORKFLOW_ID} is available; the workflow engine lands with a later slice)"
                ),
                false,
            );
        }
    };

    let data = object(vec![
        ("plan", rendered.doc.clone()),
        ("digest", string(&rendered.digest)),
        ("issue_source", string(revision_source)),
    ]);

    let mut human = String::new();
    human.push_str(&format!(
        "plan {} issue #{} (revision {})\n",
        repository.identity(),
        plan.issue_number,
        revision
    ));
    human.push_str(&format!(
        "workflow {} hash {}; state_epoch 0; digest sha256 {}\n",
        rendered.workflow_id, rendered.workflow_hash, rendered.digest
    ));
    human.push_str("steps:\n");
    if let Some(Val::Arr(items)) = rendered.doc.get("steps") {
        for step in items {
            let id = step.get("id").and_then(Val::as_str).unwrap_or("?");
            let kind = step.get("kind").and_then(Val::as_str).unwrap_or("?");
            human.push_str(&format!("  {id}  {kind}\n"));
        }
    }
    ok_result(data, human.trim_end().to_string())
}

// ---------------------------------------------------------------------------
// capabilities
// ---------------------------------------------------------------------------

fn execute_capabilities(_invocation: &Invocation) -> CmdResult {
    let capability = object(vec![
        ("schema", string("hf-capability/v1")),
        ("axis", string("forge")),
        ("actor", string("herdr-fleet")),
        (
            "capabilities",
            Val::Arr(vec![
                string("read_refs"),
                string("read_issues"),
                string("read_checks"),
            ]),
        ),
    ]);
    let data = object(vec![("capability", capability)]);
    ok_result(
        data,
        "herdr-fleet forge capabilities: read_refs read_issues read_checks\n".to_string(),
    )
}

// ---------------------------------------------------------------------------
// config init / validate / show
// ---------------------------------------------------------------------------

fn execute_config(action: ConfigAction, invocation: &Invocation) -> CmdResult {
    match action {
        ConfigAction::Init => {
            let template = init_template();
            let data = object(vec![("template", string(&template))]);
            ok_result(data, template)
        }
        ConfigAction::Validate => {
            let config = match discover_config(invocation.config_path.as_deref()) {
                Ok(Some(path)) => match load_config(&path) {
                    Ok(config) => config,
                    Err(load_error) => return load_error_result(load_error),
                },
                Ok(None) => {
                    return error_result(
                        5,
                        "config.not_found",
                        format!(
                            "no config file found (looked for {}); run `herdr-fleet config init` and save the template there",
                            default_config_hint()
                        ),
                        false,
                    );
                }
                Err(load_error) => return load_error_result(load_error),
            };
            let overlay = config.policy.as_ref().map(|policy| {
                object(vec![
                    ("valid", bool_(true)),
                    ("path", string(&policy.path.display().to_string())),
                ])
            });
            let data = object(vec![
                ("valid", bool_(true)),
                ("config_path", string(&config.path.display().to_string())),
                ("policy_overlay", overlay.unwrap_or_else(null)),
            ]);
            let mut human = format!("config valid: {}\n", config.path.display());
            if let Some(policy) = &config.policy {
                human.push_str(&format!(
                    "policy overlay valid: {}\n",
                    policy.path.display()
                ));
            }
            ok_result(data, human.trim_end().to_string())
        }
        ConfigAction::Show => {
            let config = match required_config(&invocation.config_path) {
                Ok(config) => config,
                Err(result) => return *result,
            };
            let repositories: Vec<Val> = config
                .repositories
                .iter()
                .map(|repository| {
                    object(vec![
                        ("key", string(&repository.key)),
                        ("identity", string(&repository.identity())),
                        ("origin", string(&repository.origin)),
                        (
                            "branch",
                            repository
                                .branch
                                .as_ref()
                                .map(|branch| string(branch))
                                .unwrap_or_else(null),
                        ),
                        ("enabled", bool_(repository.enabled)),
                    ])
                })
                .collect();
            let harnesses: Vec<Val> = config
                .harnesses
                .iter()
                .map(|harness| {
                    object(vec![
                        ("key", string(&harness.key)),
                        ("kind", string(&harness.kind)),
                        ("executable", string(&harness.executable)),
                        (
                            "env_allow",
                            Val::Arr(harness.env_allow.iter().map(|name| string(name)).collect()),
                        ),
                        (
                            "provider",
                            harness
                                .provider
                                .as_ref()
                                .map(|provider| string(provider))
                                .unwrap_or_else(null),
                        ),
                        (
                            "model",
                            harness
                                .model
                                .as_ref()
                                .map(|model| string(model))
                                .unwrap_or_else(null),
                        ),
                    ])
                })
                .collect();
            let workflows: Vec<Val> = config
                .workflows
                .iter()
                .map(|pin| {
                    object(vec![
                        ("key", string(&pin.key)),
                        ("id", string(&pin.id)),
                        ("hash", string(&pin.hash)),
                    ])
                })
                .collect();
            let policy_overlay = config.policy.as_ref().map(|policy| {
                object(vec![
                    ("path", string(&policy.path.display().to_string())),
                    (
                        "repositories",
                        policy
                            .repositories
                            .as_ref()
                            .map(|repos| Val::Arr(repos.iter().map(|repo| string(repo)).collect()))
                            .unwrap_or_else(null),
                    ),
                    (
                        "production_confirmation",
                        policy
                            .production_confirmation
                            .as_ref()
                            .map(|rule| string(rule))
                            .unwrap_or_else(null),
                    ),
                ])
            });
            let data = object(vec![
                ("schema", string("hf-config/v1")),
                ("config_path", string(&config.path.display().to_string())),
                (
                    "daemon_enabled",
                    config.daemon_enabled.map(bool_).unwrap_or_else(null),
                ),
                (
                    "daemon_socket",
                    config
                        .daemon_socket
                        .as_ref()
                        .map(|socket| string(socket))
                        .unwrap_or_else(null),
                ),
                ("repositories", Val::Arr(repositories)),
                ("harnesses", Val::Arr(harnesses)),
                ("workflows", Val::Arr(workflows)),
                ("policy_overlay", policy_overlay.unwrap_or_else(null)),
            ]);

            let mut human = format!("config: {}\n", config.path.display());
            if config.repositories.is_empty() {
                human.push_str("  repositories: none configured\n");
            }
            for repository in &config.repositories {
                human.push_str(&format!(
                    "  repository {} (key {}) origin {} enabled={}\n",
                    repository.identity(),
                    repository.key,
                    repository.origin,
                    repository.enabled
                ));
            }
            for harness in &config.harnesses {
                human.push_str(&format!(
                    "  harness {} kind {} executable {} env_allow=[{}]\n",
                    harness.key,
                    harness.kind,
                    harness.executable,
                    harness.env_allow.join(", ")
                ));
            }
            for pin in &config.workflows {
                human.push_str(&format!(
                    "  workflow {} id {} hash {}\n",
                    pin.key, pin.id, pin.hash
                ));
            }
            match &config.policy {
                Some(policy) => {
                    human.push_str(&format!("  policy overlay: {}\n", policy.path.display()));
                }
                None => human.push_str("  policy overlay: none\n"),
            }
            ok_result(data, human.trim_end().to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const DAEMON_USAGE: &str = "\
herdr-fleet daemon <run|status> — run or probe the state daemon

USAGE:
    herdr-fleet daemon run [--socket PATH] [--config PATH]
    herdr-fleet daemon status [--config PATH] [--json]

run serves the single-writer daemon in the foreground: it acquires the
per-user flock, opens/migrates the SQLite state, reconciles interrupted
claims, and serves hf-rpc/v1 on the per-user Unix socket (default: the XDG
runtime dir; --socket or config daemon.socket override it). A second daemon
is refused (exit 1, code daemon.busy). status probes the socket and reports
the live state; when no daemon is running it exits 1 with code daemon.absent
— read-only commands stay available without the daemon.
";

const SERVICE_USAGE: &str = "\
herdr-fleet service <doctor|install-plan|status-plan|uninstall-plan>

USAGE:
    herdr-fleet service doctor [--config PATH] [--json]
    herdr-fleet service install-plan [--config PATH] [--json]
    herdr-fleet service status-plan [--config PATH] [--json]
    herdr-fleet service uninstall-plan [--config PATH] [--json]

doctor checks the daemon environment read-only (platform, config, socket
state, per-user unit placement). The *-plan commands render the first-party
launchd/systemd unit text and the exact command steps for a clean host —
they never install, start, stop, or query the host service manager.
";

/// Resolve the daemon socket override: the CLI flag wins over
/// `config.daemon.socket`; otherwise the XDG runtime default applies.
fn effective_socket(flag: Option<&str>, config: Option<&Config>) -> Option<String> {
    flag.map(str::to_string).or_else(|| {
        config
            .and_then(|config| config.daemon_socket.clone())
            .filter(|socket| !socket.is_empty())
    })
}

/// Load the optional config; a found-but-invalid config is a hard error.
#[allow(clippy::result_large_err)]
fn load_optional_config(invocation: &Invocation) -> Result<Option<Config>, CmdResult> {
    match discover_config(invocation.config_path.as_deref()) {
        Ok(Some(path)) => match load_config(&path) {
            Ok(config) => Ok(Some(config)),
            Err(load_error) => Err(load_error_result(load_error)),
        },
        Ok(None) => Ok(None),
        Err(load_error) => Err(load_error_result(load_error)),
    }
}

/// Derive daemon paths, applying the config/flag socket override.
#[allow(clippy::result_large_err)]
fn derive_paths(socket: Option<String>) -> Result<DaemonPaths, CmdResult> {
    DaemonPaths::derive(socket.as_deref()).map_err(|err| {
        error_result(
            1,
            "daemon.paths",
            format!("{}: {}", err.code, err.message),
            false,
        )
    })
}

fn execute_daemon(action: DaemonAction, invocation: &Invocation) -> CmdResult {
    match action {
        DaemonAction::Run { socket: flag } => execute_daemon_run(flag.as_deref(), invocation),
        DaemonAction::Status { socket: flag } => execute_daemon_status(flag.as_deref(), invocation),
    }
}

fn execute_daemon_run(socket_flag: Option<&str>, invocation: &Invocation) -> CmdResult {
    let config = match load_optional_config(invocation) {
        Ok(config) => config,
        Err(result) => return result,
    };
    if let Some(false) = config.as_ref().and_then(|config| config.daemon_enabled) {
        return error_result(
            4,
            "refusal.daemon.disabled",
            "config daemon.enabled=false: the daemon is disabled for this user; remove the flag or set daemon.enabled=true".to_string(),
            false,
        );
    }
    let socket = effective_socket(socket_flag, config.as_ref());
    let paths = match derive_paths(socket) {
        Ok(paths) => paths,
        Err(result) => return result,
    };
    // The daemon logs readiness and every refusal to its own JSONL log; the
    // foreground process runs until stopped by the service manager or a
    // fatal error (there is no daemonize: launchd/systemd own the process).
    match daemon::serve(&paths) {
        Ok(()) => ok_result(object(vec![]), "daemon exited cleanly\n".to_string()),
        Err(DaemonError { code, message }) => error_result(1, code, message, false),
    }
}

fn execute_daemon_status(socket_flag: Option<&str>, invocation: &Invocation) -> CmdResult {
    let config = match load_optional_config(invocation) {
        Ok(config) => config,
        Err(result) => return result,
    };
    let socket = effective_socket(socket_flag, config.as_ref());
    let paths = match derive_paths(socket) {
        Ok(paths) => paths,
        Err(result) => return result,
    };
    match crate::lock::socket_presence(&paths.socket_path) {
        crate::lock::SocketPresence::Active => {
            let result = client::call(&paths.socket_path, "status", None);
            match result {
                Ok(doc) => {
                    let epoch = doc
                        .get("state")
                        .and_then(|state| state.get("epoch"))
                        .and_then(Val::as_int)
                        .unwrap_or(0);
                    let journal_seq = doc
                        .get("state")
                        .and_then(|state| state.get("journal_seq"))
                        .and_then(Val::as_int)
                        .unwrap_or(0);
                    let pid = doc
                        .get("daemon")
                        .and_then(|daemon| daemon.get("pid"))
                        .and_then(Val::as_int)
                        .unwrap_or(0);
                    let started_at = doc
                        .get("daemon")
                        .and_then(|daemon| daemon.get("started_at"))
                        .and_then(Val::as_str)
                        .unwrap_or("unknown");
                    let human = format!(
                        "daemon: running (pid {pid}, started {started_at})\nstate: epoch {epoch}, journal seq {journal_seq}\nsocket: {}\n",
                        paths.socket_path.display()
                    );
                    ok_result(doc, human)
                }
                Err(RpcError { code, message }) => {
                    error_result(1, &code, format!("daemon status: {message}"), false)
                }
            }
        }
        crate::lock::SocketPresence::Stale => error_result(
            1,
            "daemon.stale",
            format!(
                "a stale daemon socket exists at {}; a fresh `daemon run` reclaims it",
                paths.socket_path.display()
            ),
            true,
        ),
        crate::lock::SocketPresence::Absent => error_result(
            1,
            "daemon.absent",
            format!(
                "no daemon is running on {}; read-only commands (doctor/status/plan) stay available without it",
                paths.socket_path.display()
            ),
            false,
        ),
        crate::lock::SocketPresence::Unsafe(reason) => error_result(
            1,
            "daemon.unsafe_socket",
            format!("{} ({})", reason, paths.socket_path.display()),
            false,
        ),
    }
}

fn execute_service(action: ServiceAction, invocation: &Invocation) -> CmdResult {
    match action {
        ServiceAction::Doctor => execute_service_doctor(invocation),
        ServiceAction::PlanInstall => execute_service_plan(invocation, "install"),
        ServiceAction::PlanStatus => execute_service_plan(invocation, "status"),
        ServiceAction::PlanUninstall => execute_service_plan(invocation, "uninstall"),
    }
}

fn execute_service_doctor(invocation: &Invocation) -> CmdResult {
    let config = match load_optional_config(invocation) {
        Ok(config) => config,
        Err(result) => return result,
    };
    let platform = service::detect_platform();
    let socket = effective_socket(None, config.as_ref());
    let paths = match derive_paths(socket) {
        Ok(paths) => paths,
        Err(result) => return result,
    };
    let mut rows: Vec<DoctorRow> = Vec::new();
    let mut degraded = false;

    rows.push(DoctorRow {
        name: "platform",
        status: "ok",
        detail: Some(platform.to_string()),
    });
    let config_status = match &config {
        Some(config) => {
            let enabled = config.daemon_enabled.unwrap_or(true);
            let detail = format!(
                "daemon.enabled={enabled}{}",
                config
                    .daemon_socket
                    .as_ref()
                    .map(|socket| format!(", daemon.socket={socket}"))
                    .unwrap_or_default()
            );
            if !enabled {
                degraded = true;
            }
            ("found", detail)
        }
        None => ("absent", "no config file; defaults apply".to_string()),
    };
    rows.push(DoctorRow {
        name: "config",
        status: config_status.0,
        detail: Some(config_status.1),
    });

    let socket_presence = crate::lock::socket_presence(&paths.socket_path);
    let (socket_status, socket_detail) = match &socket_presence {
        crate::lock::SocketPresence::Active => ("running", describe_socket(&paths.socket_path)),
        crate::lock::SocketPresence::Absent => ("absent", "daemon not running".to_string()),
        crate::lock::SocketPresence::Stale => {
            degraded = true;
            ("stale", describe_socket(&paths.socket_path))
        }
        crate::lock::SocketPresence::Unsafe(reason) => {
            degraded = true;
            ("unsafe", reason.clone())
        }
    };
    rows.push(DoctorRow {
        name: "socket",
        status: socket_status,
        detail: Some(socket_detail),
    });

    // Per-user unit placement (read-only; never queried/activated).
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let config_home = crate::dirs::config_home().ok();
    let (unit_name, unit_present) = match (platform, &home, &config_home) {
        ("launchd", Some(home), _) => {
            let path = service::launchd_plist_path(home);
            (path.display().to_string(), path.exists())
        }
        ("systemd", _, Some(config_home)) => {
            let path = service::systemd_unit_path(config_home);
            (path.display().to_string(), path.exists())
        }
        _ => ("no per-user unit for this platform".to_string(), false),
    };
    rows.push(DoctorRow {
        name: "service-unit",
        status: if unit_present { "installed" } else { "absent" },
        detail: Some(unit_name),
    });
    if unit_present && socket_status == "absent" {
        degraded = true;
    }

    let data = object(vec![
        (
            "checks",
            Val::Arr(
                rows.iter()
                    .map(DoctorRow::clone)
                    .map(DoctorRow::into_val)
                    .collect(),
            ),
        ),
        (
            "summary",
            object(vec![
                ("platform", string(platform)),
                ("daemon", string(socket_status)),
                (
                    "unit",
                    string(if unit_present { "installed" } else { "absent" }),
                ),
            ]),
        ),
    ]);
    let human = rows.iter().map(DoctorRow::render).collect::<String>();
    if degraded {
        partial_result(data, human, String::new())
    } else {
        ok_result(data, human)
    }
}

fn execute_service_plan(invocation: &Invocation, kind: &str) -> CmdResult {
    let config = match load_optional_config(invocation) {
        Ok(config) => config,
        Err(result) => return result,
    };
    let platform = service::detect_platform();
    let socket = effective_socket(None, config.as_ref());
    let paths = match derive_paths(socket) {
        Ok(paths) => paths,
        Err(result) => return result,
    };
    let bin = std::env::current_exe()
        .ok()
        .unwrap_or_else(|| std::path::PathBuf::from("herdr-fleet"));
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let config_home = crate::dirs::config_home().ok();

    let (unit, target) = match platform {
        "launchd" => {
            let target = home
                .as_ref()
                .map(|home| service::launchd_plist_path(home))
                .unwrap_or_else(|| std::path::PathBuf::from(format!("~/{LAUNCHD_LABEL}.plist")));
            (service::launchd_unit(&bin, &paths.socket_path), target)
        }
        "systemd" => {
            let target = config_home
                .as_ref()
                .map(|config_home| service::systemd_unit_path(config_home))
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(format!("~/.config/systemd/user/{SYSTEMD_UNIT_NAME}"))
                });
            (service::systemd_unit(&bin, &paths.socket_path), target)
        }
        other => {
            return error_result(
                1,
                "service.unsupported",
                format!("no first-party unit exists for platform {other:?}"),
                false,
            );
        }
    };
    let steps = match kind {
        "install" => service::install_plan_steps(
            platform,
            &bin,
            &paths.socket_path,
            home.as_deref().unwrap_or(std::path::Path::new("~")),
            config_home.as_deref().unwrap_or(std::path::Path::new("~")),
        ),
        "status" => service::status_plan_steps(platform),
        "uninstall" => service::uninstall_plan_steps(
            platform,
            home.as_deref().unwrap_or(std::path::Path::new("~")),
            config_home.as_deref().unwrap_or(std::path::Path::new("~")),
        ),
        other => {
            return error_result(
                2,
                "usage.error",
                format!("unknown plan kind {other:?}"),
                false,
            );
        }
    };
    let step_vals: Vec<Val> = steps.iter().map(|step| string(step)).collect();
    let data = object(vec![
        ("platform", string(platform)),
        ("target", string(&target.display().to_string())),
        ("unit", string(&unit)),
        ("steps", Val::Arr(step_vals)),
    ]);
    let mut human = String::new();
    human.push_str(&format!(
        "herdr-fleet service {kind}-plan (platform {platform}) — nothing was installed or started\n\n"
    ));
    human.push_str(&format!("unit file: {}\n\n", target.display()));
    human.push_str(&unit);
    human.push_str("\nsteps:\n");
    for (index, step) in steps.iter().enumerate() {
        human.push_str(&format!("  {}. {step}\n", index + 1));
    }
    ok_result(data, human)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Family, validate_doc};

    fn invocation(args: &[&str]) -> Invocation {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_invocation(&owned).expect("parse")
    }

    fn assert_envelope_valid(command: &str, result: &CmdResult) {
        let json = render_envelope(command, result);
        let doc = Val::parse_json(json.trim_end()).expect("envelope parses");
        let verdict = validate_doc(Family::Output, &doc);
        assert!(
            verdict.is_accepted(),
            "envelope for {command}: {}",
            verdict.message()
        );
        assert!(json.ends_with('\n'), "envelope output ends with a newline");
    }

    #[test]
    fn parse_forms_work() {
        assert!(parse_invocation(&[]).is_err());
        assert!(parse_invocation(&["frobnicate".to_string()]).is_err());
        let doctor =
            parse_invocation(&["doctor".to_string(), "--json".to_string()]).expect("doctor");
        assert!(doctor.json);
        assert_eq!(doctor.command, "doctor");
        let plan = parse_invocation(&[
            "plan".to_string(),
            "widgets".to_string(),
            "123".to_string(),
            "--revision".to_string(),
            "0".repeat(40),
        ])
        .expect("plan");
        let plan_args = plan.plan.expect("plan args");
        assert_eq!(plan_args.repository, "widgets");
        assert_eq!(plan_args.issue_number, 123);
        assert!(plan_args.revision.is_some());
        let config = parse_invocation(&[
            "config".to_string(),
            "show".to_string(),
            "--json".to_string(),
        ])
        .expect("config");
        assert_eq!(config.config_action, Some(ConfigAction::Show));
    }

    #[test]
    fn plan_argument_validation_bites() {
        for args in [
            vec!["plan", "widgets", "abc"],
            vec!["plan", "widgets", "0"],
            vec!["plan", "widgets", "1", "--revision", "xyz"],
            vec!["plan"],
            vec!["plan", "widgets", "1", "2"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            assert!(
                parse_invocation(&owned).is_err(),
                "expected usage error for {args:?}"
            );
        }
    }

    #[test]
    fn capabilities_envelope_and_doc_validate() {
        let result = execute(&invocation(&["capabilities", "--json"]));
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.kind, Kind::Ok);
        assert_envelope_valid("capabilities", &result);
        let doc = result.data.as_ref().expect("data");
        let capability = doc.get("capability").expect("capability doc");
        let verdict = validate_doc(Family::Capability, capability);
        assert!(verdict.is_accepted(), "{}", verdict.message());
    }

    #[test]
    fn config_init_envelope_validates() {
        let result = execute(&invocation(&["config", "init", "--json"]));
        assert_eq!(result.exit_code, 0);
        assert_envelope_valid("config init", &result);
    }

    #[test]
    fn missing_config_is_a_typed_config_error() {
        // The unit test process has no herdr-fleet config (no XDG override in
        // scope), so plan/status/config validate must refuse with exit 5.
        let result = execute(&invocation(&["plan", "widgets", "1"]));
        assert_eq!(result.exit_code, 5);
        assert_eq!(result.kind, Kind::Error);
        assert_envelope_valid("plan", &result);
        let error = result.error.as_ref().expect("error doc");
        assert_eq!(error.code, "config.not_found");
    }

    #[test]
    fn unknown_flags_are_usage_errors() {
        assert!(parse_invocation(&["status".to_string(), "--spawn".to_string()]).is_err());
        assert!(parse_invocation(&["doctor".to_string(), "extra".to_string()]).is_err());
    }

    #[test]
    fn percentile_helper_is_reachable_through_execute_path() {
        // percentile_ms is a public observe helper; this guards the human
        // timings line math used by status.
        let samples = [3u64, 1, 2];
        assert_eq!(crate::observe::percentile_ms(&samples, 50.0), 2);
        assert_eq!(crate::observe::percentile_ms(&samples, 95.0), 3);
    }
}
