//! herdr-fleet command-line entry point.
//!
//! Read-only core: this binary reports package metadata, configures,
//! diagnoses, observes, and renders deterministic plans. It never mutates
//! fleet state, never installs/starts/stops Herdr, and never stores
//! credentials.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use herdr_fleet::commands::{ParseError, USAGE, execute, parse_invocation, render_envelope};
use herdr_fleet::{PACKAGE_NAME, PACKAGE_VERSION, about};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Top-level metadata flags (the only flag forms before a command).
    if args.len() == 1 {
        match args[0].as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--version" | "-V" => {
                println!("{PACKAGE_NAME} {PACKAGE_VERSION}");
                println!("{}", about());
                return ExitCode::SUCCESS;
            }
            _ => {}
        }
    }
    if args.is_empty() {
        eprintln!("{USAGE}");
        eprintln!("error: no arguments given; try `herdr-fleet --help`");
        return ExitCode::from(2);
    }
    if args[0].starts_with('-') {
        eprintln!("{USAGE}");
        eprintln!("error: unknown argument `{}`", args[0]);
        return ExitCode::from(2);
    }

    let invocation = match parse_invocation(&args) {
        Ok(invocation) => invocation,
        Err(ParseError::Help(text)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(ParseError::Usage(message)) => {
            eprintln!("error: {message}");
            eprintln!();
            eprintln!("{}", per_command_usage(&args[0]));
            return ExitCode::from(2);
        }
    };

    let result = execute(&invocation);

    if invocation.json {
        let json = render_envelope(&invocation.command, &result);
        print!("{json}");
    } else if !result.human.is_empty() {
        print!("{}", result.human);
        if !result.human.ends_with('\n') {
            println!();
        }
    }
    if !result.diagnostics.is_empty() {
        eprintln!("{}", result.diagnostics);
    }
    ExitCode::from(result.exit_code)
}

/// Usage hint line printed under a usage error for the offending command.
fn per_command_usage(command: &str) -> &'static str {
    match command {
        "config" => "usage: herdr-fleet config <init|validate|show> [--config PATH] [--json]",
        "doctor" => "usage: herdr-fleet doctor [--json]",
        "status" => "usage: herdr-fleet status [--config PATH] [--json]",
        "plan" => {
            "usage: herdr-fleet plan <repository> <issue> [--revision HEX40] [--config PATH] [--json]"
        }
        "capabilities" => "usage: herdr-fleet capabilities [--json]",
        "daemon" => {
            "usage: herdr-fleet daemon run [--socket PATH] [--config PATH]\n       herdr-fleet daemon status [--config PATH] [--json]"
        }
        "service" => {
            "usage: herdr-fleet service <doctor|install-plan|status-plan|uninstall-plan> [--config PATH] [--json]"
        }
        _ => "usage: herdr-fleet [--help] [--version] | herdr-fleet <command> [options]",
    }
}
