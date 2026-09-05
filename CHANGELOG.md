# Changelog

All notable changes to this project are documented here. This project
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) until a
release process activates (docs/RELEASING.md), then semver applies.

## [Unreleased]

### Added (bootstrap)

- Public repository foundation for herdr-fleet (issue #2):
  - Single-package Rust scaffold: `herdr-fleet` library + binary with
    truthful `--help`/`--version` only; edition 2024, pinned toolchain
    1.97.1, committed `Cargo.lock`, zero external dependencies.
  - Real-binary CLI smoke tests (`tests/cli_smoke.rs`).
  - Canonical `just` developer gates (`just ci` aggregate) and strict
    `deny.toml` for cargo-deny.
  - Hosted CI (policy, rust-ubuntu, rust-macos, supply-chain, secret-scan)
    with stable job names, minimal permissions, full-SHA action pins, and
    no caching.
  - Public-tree privacy scanner (`scripts/check-public-tree.py`) with
    discriminating self-tests (`scripts/test-check-public-tree.py`).
  - Contributor tooling: issue/PR templates, CODEOWNERS (comment-only),
    Dependabot grouped weekly updates targeting `staging`.
  - Public docs: README, ARCHITECTURE, DEVELOPMENT, WORKFLOW, RELEASING,
    three ADRs, SECURITY, CONTRIBUTING, AGENTS, CODE_OF_CONDUCT, and the
    public `skills/herdr-fleet` skill.
  - Amendment-3 architecture artifact set committed under
    `docs/architecture/` (sanitized locked-target JSON/HTML + static
    light/dark previews + SHA-256 provenance README).

### Changed

- Nothing yet (pre-alpha foundation; no prior behavior exists).

### Fixed

- Nothing yet.

### Security

- Private vulnerability reporting via GitHub's private advisory flow
  (SECURITY.md); no secrets in public issues.
