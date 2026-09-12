//! canter command-line entry point.
//!
//! The `canter` binary is the product entry point; the pre-rename
//! `herdr-fleet` binary (`src/bin/herdr-fleet.rs`) is a thin compatibility
//! alias over the same [`canter::commands::cli_main`] surface
//! (docs/contracts/compatibility.md, "Product rename (issue #106)").
//!
//! Read-only core: this binary reports package metadata, configures,
//! diagnoses, observes, and renders deterministic plans. It never mutates
//! fleet state, never installs/starts/stops Herdr, and never stores
//! credentials.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    canter::commands::cli_main()
}
