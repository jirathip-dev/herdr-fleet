//! Optional system-SSH remote transport (issue #9 AC5/AC6, capability
//! C16): invoke a remote herdr CLI/daemon through the system `ssh`
//! executable — argv-only, allowlisted environment, bounded deadline — with
//! plan-bound host identity and remote state.
//!
//! Boundaries (locked specs, risk-model.md "Remote host" row + capability
//! C24 rationale "SSH is the only remote path"):
//!
//! - NO daemon federation and NO network control API: this module spawns
//!   the system `ssh` binary exactly like the mutation engine spawns
//!   `git`/`gh`/harness executables. Nothing here opens sockets.
//! - Remote plans bind a VERIFIED SSH host identity (fingerprint) and the
//!   remote state (epoch); a reply that does not echo the binding is
//!   refused (`refusal.remote.state_mismatch`).
//! - UNKNOWN target/action/remote identity is classified production +
//!   destructive (risk-model lattice) and requires a fresh interactive TTY
//!   confirmation — it can never be scheduled and never runs unattended
//!   (AC5).
//! - Transport failure (spawn failure, deadline, process death) yields an
//!   explicit ambiguous outcome: the caller must not replay locally; an
//!   ambiguous remote effect needs external reconciliation, exactly like
//!   an interrupted local effect (AC6).
//!
//! This slice never runs a real transport: tests drive a fake `ssh`
//! executable on an allowlisted PATH that asserts its argv and returns
//! deterministic remote replies. Real remote canaries are separately
//! human-gated (issue stop condition).

use std::path::Path;
use std::time::Duration;

use crate::process::{ProcSpec, ProcStatus, run};
use crate::value::{Val, bool_, integer, object, string};

/// The system SSH executable name (resolved through the allowlisted PATH;
/// argv-only invocation, never shell-evaluated).
pub const REMOTE_SSH: &str = "ssh";
/// Per-invocation deadline (bounded; a stuck transport never hangs the
/// daemon). Transport failure past this bound is ambiguous, not retried.
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);

/// Remote transport codes (stable, typed).
pub mod code {
    /// The remote identity is unverified/unknown: classified production +
    /// destructive, fresh interactive TTY confirmation required.
    pub const IDENTITY: &str = "refusal.remote.identity";
    /// The remote reply does not match the plan-bound identity/state.
    pub const STATE_MISMATCH: &str = "refusal.remote.state_mismatch";
    /// The remote reply is not the typed remote document.
    pub const MALFORMED: &str = "refusal.remote.malformed";
    /// The transport did not produce a terminal outcome (ambiguous).
    pub const TRANSPORT: &str = "refusal.remote.transport";
}

/// A typed remote-transport refusal/error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteError {
    /// Stable dotted code (`refusal.remote.*`).
    pub code: &'static str,
    /// Human message.
    pub message: String,
}

impl RemoteError {
    fn new(code: &'static str, message: impl Into<String>) -> RemoteError {
        RemoteError {
            code,
            message: message.into(),
        }
    }
}

/// A remote target: a host plus its VERIFIED identity fingerprint. A target
/// whose fingerprint is absent or unverified is UNKNOWN (production +
/// destructive; never schedulable, fresh TTY required — AC5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTarget {
    /// Host name (synthetic identities in tests; grammar is tight: no
    /// slash, no whitespace, no `@`, no `:`).
    pub host: String,
    /// Verified SSH host identity fingerprint (64-hex or `SHA256:` form)
    /// bound by the remote plan. `None` = unverified/unknown.
    pub verified_identity: Option<String>,
}

/// Host grammar: `[A-Za-z0-9][A-Za-z0-9._-]*` — no shell metacharacters, no
/// user@host, no port syntax (port selection is a separate allowlisted
/// option, never embedded in the host token).
pub fn host_grammar_ok(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// Whether a fingerprint text is a plausible verified identity (64-hex or
/// the `SHA256:` form ssh prints for known hosts).
pub fn verified_identity_ok(identity: &str) -> bool {
    let body = identity.strip_prefix("SHA256:").unwrap_or(identity);
    (identity.starts_with("SHA256:") && body.len() >= 32 && !body.contains(' '))
        || (body.len() == 64 && body.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Risk classification for a remote target/action (AC5): when the host,
/// its verified identity, or the action is UNKNOWN, the remote operation is
/// treated as production + destructive (fail closed) and requires a fresh
/// interactive TTY confirmation. Verified read/status actions on a known
/// host stay production-class (risk-model "Remote host" row) and still
/// require plan binding; nothing here is ever schedulable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteRisk {
    /// Whether the operation is production-class.
    pub production: bool,
    /// Whether the operation is destructive-class.
    pub destructive: bool,
}

/// Classify a remote operation. `host_known` and `action_known` come from
/// the caller's typed plan surface; `identity_verified` from
/// [`RemoteTarget::verified_identity`].
pub fn classify_remote(
    host_known: bool,
    identity_verified: bool,
    action_known: bool,
) -> RemoteRisk {
    if host_known && identity_verified && action_known {
        RemoteRisk {
            production: true,
            destructive: false,
        }
    } else {
        // UNKNOWN inherits PRODUCTION + DESTRUCTIVE (risk lattice).
        RemoteRisk {
            production: true,
            destructive: true,
        }
    }
}

/// Fresh interactive TTY gate for remote operations (AC5): an UNKNOWN
/// remote identity (or any scheduled invocation) is refused unless the
/// caller presents a fresh interactive TTY-confirmed digest. Recurring
/// automation can never authorize a remote operation.
pub fn check_remote_confirmation(
    risk: RemoteRisk,
    interactive: bool,
    digest_confirmed: bool,
    scheduled: bool,
) -> Result<(), RemoteError> {
    if scheduled {
        return Err(RemoteError::new(
            code::IDENTITY,
            "remote operations are never authorized by recurring schedules or stale approvals",
        ));
    }
    if risk.destructive && !(interactive && digest_confirmed) {
        return Err(RemoteError::new(
            code::IDENTITY,
            "unknown/remote identities are production + destructive: a fresh interactive TTY-confirmed digest is required",
        ));
    }
    Ok(())
}

/// One bounded remote invocation (argv built here; never shell-evaluated).
pub struct RemoteInvocation<'a> {
    /// The verified remote target.
    pub target: &'a RemoteTarget,
    /// Path to the operator's known_hosts file (host verification is
    /// delegated to the system SSH client against THIS file; the plan binds
    /// the fingerprint the remote must echo).
    pub known_hosts_path: &'a Path,
    /// Remote CLI binary name (bare name; resolved remotely, not locally).
    pub remote_cli: &'a str,
    /// Remote argv (e.g. `daemon status --json`); the first token is the
    /// remote CLI, everything after is its argv.
    pub remote_args: &'a [String],
    /// The state epoch the remote must report (plan-bound remote state).
    pub expected_epoch: Option<i64>,
    /// The identity the remote must report (plan-bound; equals
    /// `target.verified_identity`).
    pub expected_identity: Option<String>,
}

/// Build the system-ssh argv for one invocation (asserted verbatim by the
/// fake-ssh contract tests).
pub fn ssh_argv(invocation: &RemoteInvocation<'_>) -> Vec<String> {
    let mut argv = vec![
        "ssh".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=15".to_string(),
        "-o".to_string(),
        format!(
            "UserKnownHostsFile={}",
            invocation.known_hosts_path.display()
        ),
        invocation.target.host.clone(),
        "--".to_string(),
        invocation.remote_cli.to_string(),
    ];
    argv.extend(invocation.remote_args.iter().cloned());
    argv
}

/// Verify a remote reply document against the plan bindings. The remote
/// CLI/daemon answers with its host identity + state epoch; any mismatch
/// (or an unparseable reply) is a typed refusal — never a guess.
pub fn remote_reply_matches(
    reply: &Val,
    expected_host: &str,
    expected_identity: Option<&str>,
    expected_epoch: Option<i64>,
) -> Result<(), RemoteError> {
    let ok = reply.get("ok").and_then(Val::as_bool);
    if ok == Some(false) {
        let error = reply
            .get("error")
            .map(|error| {
                error
                    .get("message")
                    .and_then(Val::as_str)
                    .unwrap_or("remote refused")
                    .to_string()
            })
            .unwrap_or_else(|| "remote refused without a typed error".to_string());
        return Err(RemoteError::new(code::STATE_MISMATCH, error));
    }
    if ok != Some(true) {
        return Err(RemoteError::new(
            code::MALFORMED,
            "remote reply is not the typed response document (ok:true expected)",
        ));
    }
    let remote = reply
        .get("result")
        .and_then(|result| result.get("remote"))
        .ok_or_else(|| {
            RemoteError::new(
                code::MALFORMED,
                "remote reply carries no result.remote document",
            )
        })?;
    let host = remote
        .get("host")
        .and_then(Val::as_str)
        .ok_or_else(|| RemoteError::new(code::MALFORMED, "result.remote.host missing"))?;
    if host != expected_host {
        return Err(RemoteError::new(
            code::STATE_MISMATCH,
            format!("remote host {host:?} does not match the plan binding {expected_host:?}"),
        ));
    }
    if let Some(expected_identity) = expected_identity {
        let identity = remote
            .get("identity")
            .and_then(Val::as_str)
            .ok_or_else(|| RemoteError::new(code::MALFORMED, "result.remote.identity missing"))?;
        if identity != expected_identity {
            return Err(RemoteError::new(
                code::STATE_MISMATCH,
                "remote identity does not match the verified plan binding",
            ));
        }
    }
    if let Some(expected_epoch) = expected_epoch {
        let epoch = remote
            .get("epoch")
            .and_then(Val::as_int)
            .ok_or_else(|| RemoteError::new(code::MALFORMED, "result.remote.epoch missing"))?;
        if epoch != expected_epoch {
            return Err(RemoteError::new(
                code::STATE_MISMATCH,
                format!(
                    "remote epoch {epoch} does not match the plan-bound epoch {expected_epoch}"
                ),
            ));
        }
    }
    Ok(())
}

/// The typed outcome of one remote transport attempt. Ambiguous outcomes
/// (spawn failure, deadline, process death) are EXPLICIT: the caller never
/// replays the invocation locally; ambiguous remote effects require
/// external reconciliation exactly like interrupted local effects (AC6).
#[derive(Clone, Debug, PartialEq)]
pub enum RemoteOutcome {
    /// Transport completed and the reply matched every plan binding.
    Succeeded {
        /// The verified remote reply document.
        reply: Val,
    },
    /// Transport completed with a typed refusal (reply mismatch, non-zero
    /// remote exit, malformed reply).
    Failed { code: &'static str, message: String },
    /// Transport did not produce a terminal outcome; never retry locally.
    Ambiguous { code: &'static str, message: String },
}

/// Invoke the remote CLI/daemon through system ssh with the allowlisted
/// environment. This function NEVER retries: transport failures resolve
/// ambiguous or failed and the caller records them.
pub fn invoke_remote(
    invocation: &RemoteInvocation<'_>,
    env: &std::collections::BTreeMap<String, String>,
) -> RemoteOutcome {
    if !host_grammar_ok(&invocation.target.host) {
        return RemoteOutcome::Failed {
            code: code::MALFORMED,
            message: format!(
                "remote host {:?} is outside the host grammar",
                invocation.target.host
            ),
        };
    }
    let argv = ssh_argv(invocation);
    let args: Vec<String> = argv[1..].to_vec();
    let out = run(ProcSpec {
        program: REMOTE_SSH,
        args: &args,
        env,
        cwd: None,
        timeout: REMOTE_TIMEOUT,
    });
    match out.status {
        ProcStatus::SpawnFailed(message) => RemoteOutcome::Ambiguous {
            code: code::TRANSPORT,
            message: format!("ssh transport could not start: {message}"),
        },
        ProcStatus::TimedOut => RemoteOutcome::Ambiguous {
            code: code::TRANSPORT,
            message: "ssh transport exceeded its deadline; the remote effect is ambiguous and must be reconciled externally (no local replay)".to_string(),
        },
        ProcStatus::Exit(code) if code < 0 => RemoteOutcome::Ambiguous {
            code: code::TRANSPORT,
            message: "ssh transport died without a terminal outcome; the remote effect is ambiguous (no local replay)".to_string(),
        },
        ProcStatus::Exit(0) => {
            let reply = match Val::parse_json(out.stdout.trim()) {
                Ok(reply) => reply,
                Err(err) => {
                    return RemoteOutcome::Failed {
                        code: code::MALFORMED,
                        message: format!("remote reply is not valid JSON ({err})"),
                    };
                }
            };
            match remote_reply_matches(
                &reply,
                &invocation.target.host,
                invocation.expected_identity.as_deref(),
                invocation.expected_epoch,
            ) {
                Ok(()) => RemoteOutcome::Succeeded { reply },
                Err(err) => RemoteOutcome::Failed {
                    code: err.code,
                    message: err.message,
                },
            }
        }
        ProcStatus::Exit(code) => RemoteOutcome::Failed {
            code: "adapter.exit",
            message: format!("remote transport exited with status {code}"),
        },
    }
}

/// Build the typed remote document a compliant remote CLI/daemon answers
/// with (used by the fake-ssh contract tests and remote fixtures).
pub fn remote_doc(host: &str, identity: &str, epoch: i64) -> Val {
    object(vec![
        ("schema", string("hf-rpc-response/v1")),
        ("id", string(&"ab".repeat(8))),
        ("ok", bool_(true)),
        (
            "result",
            object(vec![(
                "remote",
                object(vec![
                    ("host", string(host)),
                    ("identity", string(identity)),
                    ("epoch", integer(epoch)),
                ]),
            )]),
        ),
        ("error", Val::Null),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sandbox(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("hf-remote-{}", std::process::id()));
        let dir = base.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).expect("bin dir");
        dir
    }

    fn identity() -> String {
        "0".repeat(64)
    }

    /// Fake ssh: asserts argv by appending it to a log, then emits a
    /// canned remote document. The log path is embedded at write time
    /// (runtime temp paths never appear in the tracked tree).
    fn write_fake_ssh(bin_dir: &Path, log: &Path, reply_json: &str, exit_code: i32) -> PathBuf {
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s' '{}'\nexit {}\n",
            log.display(),
            reply_json.replace('\'', "'\\''"),
            exit_code
        );
        let path = bin_dir.join("ssh");
        std::fs::write(&path, script).expect("write fake ssh");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path
    }

    fn host_env(bin_dir: &Path) -> std::collections::BTreeMap<String, String> {
        let mut env = std::collections::BTreeMap::new();
        let mut path = bin_dir.display().to_string();
        if let Ok(host_path) = std::env::var("PATH") {
            path.push(':');
            path.push_str(&host_path);
        }
        env.insert("PATH".to_string(), path);
        env.insert(
            "HOME".to_string(),
            std::env::temp_dir().display().to_string(),
        );
        env
    }

    fn invocation<'a>(
        target: &'a RemoteTarget,
        known_hosts: &'a Path,
        remote_cli: &'a str,
        args: &'a [String],
        epoch: Option<i64>,
        expected_identity: Option<String>,
    ) -> RemoteInvocation<'a> {
        RemoteInvocation {
            target,
            known_hosts_path: known_hosts,
            remote_cli,
            remote_args: args,
            expected_epoch: epoch,
            expected_identity,
        }
    }

    #[test]
    fn host_and_identity_grammars_refuse_odd_tokens() {
        assert!(host_grammar_ok("host-1.example"));
        assert!(!host_grammar_ok("user@host"));
        assert!(!host_grammar_ok("host:22"));
        assert!(!host_grammar_ok("ho st"));
        assert!(!host_grammar_ok(""));
        assert!(verified_identity_ok(&"a".repeat(64)));
        assert!(verified_identity_ok(
            "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ"
        ));
        assert!(!verified_identity_ok("unverified"));
    }

    #[test]
    fn unknown_remote_identity_is_production_plus_destructive_and_needs_tty() {
        let unknown = classify_remote(false, false, true);
        assert!(unknown.production && unknown.destructive);
        // A fresh interactive TTY confirmation admits the risk...
        assert!(check_remote_confirmation(unknown, true, true, false).is_ok());
        // ...but schedules and stale approvals never do (AC5).
        assert_eq!(
            check_remote_confirmation(unknown, true, true, true)
                .unwrap_err()
                .code,
            code::IDENTITY
        );
        assert_eq!(
            check_remote_confirmation(unknown, false, false, false)
                .unwrap_err()
                .code,
            code::IDENTITY
        );
        let verified = classify_remote(true, true, true);
        assert!(
            !verified.destructive,
            "verified remote read work is not destructive"
        );
        // Even verified remote work is production-class and refuses
        // scheduled invocation.
        assert_eq!(
            check_remote_confirmation(verified, true, true, true)
                .unwrap_err()
                .code,
            code::IDENTITY
        );
    }

    #[test]
    fn fake_ssh_asserts_argv_and_success_matches_plan_bindings() {
        let dir = sandbox("argv-ok");
        let log = dir.join("argv.log");
        let id = identity();
        let reply = crate::canonical::canonical_text(&remote_doc("build-host-1", &id, 7));
        write_fake_ssh(&dir.join("bin"), &log, &reply, 0);
        let known_hosts = dir.join("known_hosts");
        std::fs::write(&known_hosts, "synthetic known_hosts\n").expect("known hosts");
        let target = RemoteTarget {
            host: "build-host-1".to_string(),
            verified_identity: Some(id.clone()),
        };
        let args = vec!["status".to_string(), "--json".to_string()];
        let outcome = invoke_remote(
            &invocation(
                &target,
                &known_hosts,
                "herdr-fleet",
                &args,
                Some(7),
                Some(id.clone()),
            ),
            &host_env(&dir.join("bin")),
        );
        match outcome {
            RemoteOutcome::Succeeded { reply } => {
                assert_eq!(reply.get("ok").and_then(Val::as_bool), Some(true));
            }
            other => panic!("expected success, got {other:?}"),
        }
        // The fake ssh recorded the exact argv: batch mode, the bound
        // known_hosts file, the host, and the remote CLI command.
        let argv = std::fs::read_to_string(&log).expect("argv log");
        assert!(argv.contains("BatchMode=yes"), "argv: {argv}");
        assert!(
            argv.contains(&format!("UserKnownHostsFile={}", known_hosts.display())),
            "argv must bind the known_hosts file: {argv}"
        );
        assert!(
            argv.contains("build-host-1"),
            "argv must carry the host: {argv}"
        );
        assert!(argv.contains("herdr-fleet"), "argv: {argv}");
        assert!(argv.contains("status --json"), "argv: {argv}");
        assert!(
            argv.contains("--"),
            "argv separates local from remote: {argv}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_state_or_identity_mismatch_is_a_typed_failure() {
        let dir = sandbox("mismatch");
        let log = dir.join("argv.log");
        let id = identity();
        let reply = crate::canonical::canonical_text(&remote_doc("build-host-1", &id, 7));
        write_fake_ssh(&dir.join("bin"), &log, &reply, 0);
        let known_hosts = dir.join("known_hosts");
        std::fs::write(&known_hosts, "synthetic\n").expect("known hosts");
        let target = RemoteTarget {
            host: "build-host-1".to_string(),
            verified_identity: Some(id.clone()),
        };
        let args = vec!["status".to_string()];
        // The plan bound epoch 9; the remote reports 7 -> state mismatch.
        let mismatch = invoke_remote(
            &invocation(
                &target,
                &known_hosts,
                "herdr-fleet",
                &args,
                Some(9),
                Some(id.clone()),
            ),
            &host_env(&dir.join("bin")),
        );
        assert_eq!(
            mismatch,
            RemoteOutcome::Failed {
                code: code::STATE_MISMATCH,
                message: "remote epoch 7 does not match the plan-bound epoch 9".to_string()
            }
        );
        // The plan bound a different identity -> refusal.
        let other = "f".repeat(64);
        let mismatch = invoke_remote(
            &invocation(
                &target,
                &known_hosts,
                "herdr-fleet",
                &args,
                Some(7),
                Some(other),
            ),
            &host_env(&dir.join("bin")),
        );
        assert_eq!(
            mismatch,
            RemoteOutcome::Failed {
                code: code::STATE_MISMATCH,
                message: "remote identity does not match the verified plan binding".to_string()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transport_failures_are_explicit_ambiguous_or_failed() {
        let dir = sandbox("transport");
        // Remote exit 3: failed, never ambiguous.
        let log = dir.join("exit.log");
        write_fake_ssh(&dir.join("bin"), &log, "", 3);
        let known_hosts = dir.join("known_hosts");
        std::fs::write(&known_hosts, "synthetic\n").expect("known hosts");
        let target = RemoteTarget {
            host: "build-host-1".to_string(),
            verified_identity: Some(identity()),
        };
        let args: Vec<String> = vec![];
        let failed = invoke_remote(
            &invocation(&target, &known_hosts, "herdr-fleet", &args, None, None),
            &host_env(&dir.join("bin")),
        );
        assert!(matches!(
            failed,
            RemoteOutcome::Failed {
                code: "adapter.exit",
                ..
            }
        ));
        // No ssh on the PATH: spawn failure -> AMBIGUOUS (transport
        // failure never replays locally). The PATH must exclude the host
        // PATH entirely or the real ssh binary would be found.
        let empty = dir.join("empty-bin");
        std::fs::create_dir_all(&empty).expect("empty bin");
        let mut bare_env = std::collections::BTreeMap::new();
        bare_env.insert("PATH".to_string(), empty.display().to_string());
        bare_env.insert(
            "HOME".to_string(),
            std::env::temp_dir().display().to_string(),
        );
        let ambiguous = invoke_remote(
            &invocation(&target, &known_hosts, "herdr-fleet", &args, None, None),
            &bare_env,
        );
        assert!(
            matches!(
                ambiguous,
                RemoteOutcome::Ambiguous {
                    code: code::TRANSPORT,
                    ..
                }
            ),
            "spawn failure must be ambiguous: {ambiguous:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reply_validation_refuses_foreign_hosts_and_malformed_docs() {
        let reply = remote_doc("other-host", &identity(), 1);
        let err = remote_reply_matches(&reply, "build-host-1", Some(&identity()), Some(1))
            .expect_err("host mismatch must refuse");
        assert_eq!(err.code, code::STATE_MISMATCH);
        // ok:false replies refuse like any typed refusal.
        let refused = object(vec![
            ("schema", string("hf-rpc-response/v1")),
            ("id", string(&"ab".repeat(8))),
            ("ok", bool_(false)),
            ("result", Val::Null),
            (
                "error",
                object(vec![
                    ("code", string("refusal.x")),
                    ("message", string("nope")),
                ]),
            ),
        ]);
        let err = remote_reply_matches(&refused, "build-host-1", None, None).expect_err("refused");
        assert_eq!(err.code, code::STATE_MISMATCH);
        assert!(err.message.contains("nope"));
        // Garbage is malformed, never guessed.
        let garbage = object(vec![("hello", string("world"))]);
        let err = remote_reply_matches(&garbage, "build-host-1", None, None).expect_err("garbage");
        assert_eq!(err.code, code::MALFORMED);
    }
}
