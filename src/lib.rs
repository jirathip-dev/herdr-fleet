//! herdr-fleet library surface.
//!
//! Read-only core (issue #4): configuration, doctor, read-only status/plan
//! rendering, and stable versioned JSON. No daemon, workflow execution,
//! mutation, migration, or release behavior exists in this slice.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod backup;
pub mod canonical;
pub mod client;
pub mod commands;
pub mod config;
pub mod daemon;
pub mod dirs;
pub mod formats;
pub mod lock;
pub mod observe;
pub mod plan;
pub mod process;
pub mod redact;
pub mod schema;
pub mod service;
pub mod state;
pub mod time;
pub mod value;

/// The package name, taken from Cargo metadata at compile time.
pub const PACKAGE_NAME: &str = env!("CARGO_PKG_NAME");

/// The package version, taken from Cargo metadata at compile time.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// One-line human-readable description of the project, used by `--version`.
///
/// Truthful for the read-only core: this is a read-only companion CLI; no
/// daemon or live fleet mutation is implemented.
pub fn about() -> &'static str {
    "herdr-fleet: typed, plan-first companion CLI for operating Herdr coding-agent fleets (read-only core; no daemon, no live fleet mutations)"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_identity_matches_cargo_metadata() {
        assert_eq!(PACKAGE_NAME, "herdr-fleet");
        assert_eq!(PACKAGE_VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!about().is_empty());
    }
}
