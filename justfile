# herdr-fleet — canonical developer entry point.
# Canonical documentation: docs/DEVELOPMENT.md (single authoritative home).
# Recipes preserve raw failures and raw exit statuses; never pipe a gate
# through grep as a PASS/FAIL test.

set shell := ["bash", "-uc"]

# Apply rustfmt.
fmt:
    cargo fmt

# Blocking formatting check.
fmt-check:
    cargo fmt --check

# Compile/check all targets against the committed lockfile.
check:
    cargo check --locked --all-targets

# Clippy with warnings denied, all targets, committed lockfile.
lint:
    cargo clippy --locked --all-targets -- -D warnings

# All unit/doc/integration tests against the committed lockfile.
test:
    cargo test --locked

# Library documentation, no dependencies fetched.
doc:
    cargo doc --locked --no-deps

# Optimized release build against the committed lockfile.
build-release:
    cargo build --release --locked

# Public-tree/privacy scanner (self-test + scan) + supply-chain checks.
# gitleaks is REQUIRED: a missing gitleaks fails this recipe with an
# actionable message instead of silently skipping the content scan.
security:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "==> scanner self-test (test-check-public-tree.py)"
    python3 scripts/test-check-public-tree.py
    echo "==> public-tree scan (check-public-tree.py)"
    python3 scripts/check-public-tree.py .
    echo "==> cargo-deny (licenses/bans/sources/advisories)"
    cargo deny check
    echo "==> cargo-audit (RustSec advisory DB)"
    cargo audit
    if ! command -v gitleaks >/dev/null 2>&1; then
        echo "ERROR: gitleaks is not installed or not on PATH." >&2
        echo "gitleaks is a required gate for this public repository." >&2
        echo "Install it with: brew install gitleaks" >&2
        echo "or from a pinned GitHub release binary: https://github.com/gitleaks/gitleaks/releases" >&2
        echo "See docs/DEVELOPMENT.md (Security tooling) for the pinned version." >&2
        exit 1
    fi
    echo "==> gitleaks directory scan (checked-out tree)"
    gitleaks dir --no-banner --redact --exit-code 1 .

# Exact local aggregate mirroring hosted CI, fail-fast ordering.
ci: fmt-check check lint test doc build-release security

# List the recipes.
default:
    @just --list
