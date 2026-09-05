//! CLI smoke tests against the REAL compiled binary.
//!
//! `env!("CARGO_BIN_EXE_herdr-fleet")` is provided by Cargo for integration
//! tests and points at the built binary of this package.

use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_herdr-fleet"))
        .args(args)
        .output()
        .expect("failed to spawn the herdr-fleet binary")
}

#[test]
fn help_exits_zero_and_prints_usage() {
    for flag in ["--help", "-h"] {
        let out = run(&[flag]);
        assert!(out.status.success(), "{flag} should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("herdr-fleet"),
            "{flag} stdout should name the program"
        );
        assert!(
            stdout.contains("USAGE"),
            "{flag} stdout should contain usage text"
        );
        assert!(out.stderr.is_empty(), "{flag} stderr should be empty");
    }
}

#[test]
fn version_exits_zero_and_prints_package_identity() {
    for flag in ["--version", "-V"] {
        let out = run(&[flag]);
        assert!(out.status.success(), "{flag} should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(env!("CARGO_PKG_NAME")),
            "{flag} stdout should contain the package name"
        );
        assert!(
            stdout.contains(env!("CARGO_PKG_VERSION")),
            "{flag} stdout should contain the package version"
        );
        assert!(out.stderr.is_empty(), "{flag} stderr should be empty");
    }
}

#[test]
fn no_arguments_exits_nonzero_with_usage_on_stderr() {
    let out = run(&[]);
    assert!(!out.status.success(), "no arguments should exit non-zero");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("USAGE"), "stderr should contain usage text");
    assert!(
        out.stdout.is_empty(),
        "stdout should be empty for the error path"
    );
}

#[test]
fn unknown_argument_exits_nonzero_with_usage_on_stderr() {
    let out = run(&["--definitely-not-a-command"]);
    assert!(
        !out.status.success(),
        "unknown argument should exit non-zero"
    );
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown argument"),
        "stderr should explain the error"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout should be empty for the error path"
    );
}

#[test]
fn usage_lists_only_help_and_version_forms() {
    // The bootstrap binary must not pretend any status/spawn/rearm/review/
    // plugin/release command exists: the USAGE block may list only the
    // --help and --version invocation forms.
    for flag in ["--help", "-h"] {
        let out = run(&[flag]);
        assert!(out.status.success(), "{flag} should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let usage_section = stdout
            .split("USAGE:")
            .nth(1)
            .expect("stdout should contain a USAGE section");
        let invocation_lines: Vec<&str> = usage_section
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("herdr-fleet "))
            .collect();
        assert_eq!(
            invocation_lines.len(),
            2,
            "USAGE must list exactly two invocation forms, got {invocation_lines:?}"
        );
        for line in invocation_lines {
            assert!(
                line == "herdr-fleet --help" || line == "herdr-fleet --version",
                "USAGE must not claim an unimplemented command: {line:?}"
            );
        }
    }
}
