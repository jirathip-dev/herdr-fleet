//! Bounded subprocess runner for the read adapters.
//!
//! Every external read (git, herdr, `gh`) runs through [`run`], which:
//! - passes only the allowlisted adapter environment (never the full host
//!   environment),
//! - never evaluates shell syntax (argv is passed directly),
//! - enforces a per-process deadline and kills the child when it expires,
//! - reports elapsed wall time for the performance evidence in `status`.
//!
//! The environment allowlist is the only environment channel to child
//! processes (trust model T5; see [`crate::config::adapter_environment`]).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How a child process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcStatus {
    /// Normal exit with an exit code.
    Exit(i32),
    /// The deadline passed; the child was killed.
    TimedOut,
    /// The executable could not be spawned.
    SpawnFailed(String),
}

impl ProcStatus {
    /// Whether the process ran to completion (exited on its own).
    pub fn completed(&self) -> bool {
        matches!(self, ProcStatus::Exit(_))
    }

    /// Exit code when the process completed.
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            ProcStatus::Exit(code) => Some(*code),
            _ => None,
        }
    }
}

/// One bounded subprocess invocation.
pub struct ProcSpec<'a> {
    /// Executable name (resolved via PATH from the allowlisted environment).
    pub program: &'a str,
    /// Arguments; never shell-evaluated.
    pub args: &'a [String],
    /// Allowlisted environment (see `config::adapter_environment`).
    pub env: &'a BTreeMap<String, String>,
    /// Working directory for the child (inherited when `None`).
    pub cwd: Option<&'a Path>,
    /// Per-process deadline.
    pub timeout: Duration,
}

/// The outcome of one bounded invocation.
#[derive(Clone, Debug)]
pub struct ProcOut {
    /// How the process ended.
    pub status: ProcStatus,
    /// Captured stdout (lossy UTF-8).
    pub stdout: String,
    /// Captured stderr (lossy UTF-8).
    pub stderr: String,
    /// Wall time from spawn to exit/kill, in milliseconds.
    pub elapsed_ms: u64,
}

/// Run one bounded subprocess invocation (see module docs for the bounds).
pub fn run(spec: ProcSpec<'_>) -> ProcOut {
    let started = Instant::now();
    let mut command = Command::new(spec.program);
    command
        .args(spec.args)
        .env_clear()
        .envs(spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = spec.cwd {
        command.current_dir(cwd);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return ProcOut {
                status: ProcStatus::SpawnFailed(err.to_string()),
                stdout: String::new(),
                stderr: String::new(),
                elapsed_ms: elapsed_ms(started),
            };
        }
    };

    let status = loop {
        if started.elapsed() >= spec.timeout {
            let _ = child.kill();
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => {
                // The process went away in an unexpected way.
                return ProcOut {
                    status: ProcStatus::SpawnFailed(err.to_string()),
                    stdout: String::new(),
                    stderr: String::new(),
                    elapsed_ms: elapsed_ms(started),
                };
            }
        }
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let _ = child.wait();

    let timed_out = started.elapsed() >= spec.timeout;
    let final_status = if timed_out && !status.success() && status.code().is_none() {
        // Killed by our deadline: status has no exit code on unix.
        ProcStatus::TimedOut
    } else {
        ProcStatus::Exit(status.code().unwrap_or(-1))
    };
    let elapsed = elapsed_ms(started);

    ProcOut {
        status: final_status,
        stdout,
        stderr,
        elapsed_ms: elapsed,
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        let (key, value) = ("PATH", std::env::var_os("PATH").unwrap_or_default());
        env.insert(key.to_string(), value.to_string_lossy().to_string());
        env
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn runs_a_completed_process_and_measures_it() {
        let out = run(ProcSpec {
            program: "sh",
            args: &args(&["-c", "printf hello; printf err >&2; exit 3"]),
            env: &env(),
            cwd: None,
            timeout: Duration::from_secs(10),
        });
        assert_eq!(out.status.exit_code(), Some(3));
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.stderr, "err");
        assert!(out.elapsed_ms < 10_000);
    }

    #[test]
    fn spawn_failure_is_reported_without_panic() {
        let out = run(ProcSpec {
            program: "canter-no-such-tool-xyz",
            args: &[],
            env: &env(),
            cwd: None,
            timeout: Duration::from_secs(1),
        });
        assert!(matches!(out.status, ProcStatus::SpawnFailed(_)));
    }

    #[test]
    fn timeout_kills_a_sleeping_child() {
        let out = run(ProcSpec {
            program: "sh",
            args: &args(&["-c", "sleep 30"]),
            env: &env(),
            cwd: None,
            timeout: Duration::from_millis(200),
        });
        assert_eq!(out.status, ProcStatus::TimedOut);
        assert!(
            out.elapsed_ms >= 150,
            "deadline enforced, got {}ms",
            out.elapsed_ms
        );
    }

    #[test]
    fn environment_is_allowlisted_not_inherited() {
        let out = run(ProcSpec {
            program: "sh",
            args: &args(&["-c", "printf '%s' \"${HF_SECRET_ENV:-unset}\""]),
            env: &env(),
            cwd: None,
            timeout: Duration::from_secs(10),
        });
        assert_eq!(out.stdout, "unset", "host env must not reach the child");
    }

    #[test]
    fn argv_is_never_shell_evaluated() {
        // A semicolon inside an argument must stay a literal argument.
        let out = run(ProcSpec {
            program: "sh",
            args: &args(&["-c", "printf '%s' \"$1\"", "probe", "a;b && c"]),
            env: &env(),
            cwd: None,
            timeout: Duration::from_secs(10),
        });
        assert_eq!(out.stdout, "a;b && c");
    }
}
