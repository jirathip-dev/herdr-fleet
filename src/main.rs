//! herdr-fleet command-line entry point.
//!
//! Pre-alpha bootstrap: the binary truthfully reports `--help` and
//! `--version` only. No status/spawn/rearm/review/plugin/release command
//! exists yet, and none is claimed by the usage text.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use herdr_fleet::{PACKAGE_NAME, PACKAGE_VERSION, about};

const USAGE: &str = "\
herdr-fleet — typed, plan-first companion CLI for Herdr coding-agent fleets

PRE-ALPHA BOOTSTRAP: no daemon, workflow, mutation, or release behavior is
implemented yet. This binary reports package metadata only.

USAGE:
    herdr-fleet --help
    herdr-fleet --version

OPTIONS:
    -h, --help       Print this usage text and exit.
    -V, --version    Print the package name, version, and description, and exit.

No other commands exist in this bootstrap.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [] => {
            eprintln!("{USAGE}");
            eprintln!("error: no arguments given; try `herdr-fleet --help`");
            ExitCode::from(2)
        }
        [flag] if flag == "--help" || flag == "-h" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        [flag] if flag == "--version" || flag == "-V" => {
            println!("{} {}", PACKAGE_NAME, PACKAGE_VERSION);
            println!("{}", about());
            ExitCode::SUCCESS
        }
        [unknown] => {
            eprintln!("{USAGE}");
            eprintln!("error: unknown argument `{unknown}`");
            ExitCode::from(2)
        }
        _ => {
            eprintln!("{USAGE}");
            eprintln!("error: expected at most one argument");
            ExitCode::from(2)
        }
    }
}
