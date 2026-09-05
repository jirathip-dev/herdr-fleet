//! herdr-fleet library surface.
//!
//! Pre-alpha bootstrap: this crate deliberately exposes only package identity
//! helpers. No daemon, workflow, adapter, mutation, migration, or release
//! behavior exists yet (see the repository roadmap for the approved target
//! architecture).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// The package name, taken from Cargo metadata at compile time.
pub const PACKAGE_NAME: &str = env!("CARGO_PKG_NAME");

/// The package version, taken from Cargo metadata at compile time.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// One-line human-readable description of the project, used by `--version`.
///
/// Truthful for the bootstrap: this is a pre-alpha companion CLI; live fleet
/// mutations are not implemented.
pub fn about() -> &'static str {
    "herdr-fleet: typed, plan-first companion CLI for operating Herdr coding-agent fleets (pre-alpha; live fleet mutations are not implemented)"
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
