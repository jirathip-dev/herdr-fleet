//! `herdr-fleet` — pre-rename product-name compatibility alias.
//!
//! The product is now `canter`; this binary exists so pre-rename
//! invocations (scripts, service units, muscle memory) keep working. It
//! prints a deprecation notice to stderr and delegates to the exact same
//! CLI surface as the canonical binary. The normative rule is
//! docs/contracts/compatibility.md, "Product rename (issue #106)".
//!
//! Invocation compatibility is the only thing this alias preserves: every
//! state/config path it touches is still the shared `canter` derivation
//! (which itself adopts a pre-rename state tree in place).

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!(
        "warning: `herdr-fleet` is the pre-rename name of `canter`; the alias runs the \
         `canter` CLI unchanged and will be removed in a future release \
         (docs/contracts/compatibility.md)"
    );
    canter::commands::cli_main()
}
