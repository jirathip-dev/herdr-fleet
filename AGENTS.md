# AGENTS.md — repository contract for coding agents

canter is a **public** repository in **bootstrap**: repository
foundation, CI/security gates, docs, and contributor tooling only. No
daemon, workflow engine, adapter, mutation, migration, or release behavior
exists or may be added here until its roadmap slice is routed (umbrella
issue #1; children #3–#10 are unrouted).

## Ownership map

| Area | Canonical files |
| --- | --- |
| Rust library surface | `src/lib.rs` |
| CLI entry (help/version only) | `src/main.rs` |
| CLI behavior tests | `tests/cli_smoke.rs` |
| Privacy scanner + self-tests | `scripts/check-public-tree.py`, `scripts/test-check-public-tree.py` |
| Developer gates | `justfile` (canonical docs: `docs/DEVELOPMENT.md`) |
| CI + security workflows | `.github/workflows/ci.yml`, `.github/workflows/security.yml` |
| Architecture docs + ADRs | `docs/ARCHITECTURE.md`, `docs/decisions/`, `docs/architecture/` |
| Process docs | `docs/WORKFLOW.md`, `docs/RELEASING.md` |
| Public skill | `skills/canter/SKILL.md` |

## Mandatory gates

Run the exact local aggregate before any PR: `just ci` (fmt-check → check →
lint → test → doc → build-release → security) and record raw exit codes.
`just security` requires gitleaks installed — a missing gitleaks is a
failure, not a skip. CI mirrors these gates plus policy checks (promotion
policy, forbidden workflow triggers, full-SHA action pins, scanner
self-test, required files, documentation links) and secret scans.

## Public-data boundary (hard rule)

Never commit host paths, private repository names, credentials/secret-shaped
content, provider/model policy, or live scheduler identity. The tracked
tree is machine-scanned on every PR (git-index parser + gitleaks); rules are
proven by self-tests. Synthetic `.example` files are permitted; real
credential-class files are rejected. When in doubt, keep it out.

## Branch discipline

- Feature/dependency PRs target `staging` (never `main`). No direct pushes
  to `staging` or `main`.
- Promotion to `main` is a dedicated human-only PR; a narrow `hotfix/*`
  exception exists for incidents (fresh human approval + mandatory
  reconciliation). Details: `docs/WORKFLOW.md`.
- No releases, tags, crates.io/Homebrew publication, or plugin manifests in
  this bootstrap. `docs/RELEASING.md` is a future contract, not active.

## Smallest-change discipline

- Prefer the smallest correct diff that satisfies the issue; stdlib first;
  zero external dependencies until a slice proves the need.
- No speculative directories (`domain/`, `adapters/`, `commands/`,
  `schemas/`, `examples/`, plugin dirs). No clap/async/HTTP/tracing/config
  dependencies in the bootstrap.
- Keep one fact authoritative and link to it (docs/DEVELOPMENT.md is the
  canonical gate home).
- Commit wording: `Refs #N` (never `Fixes`/`Closes`/`Resolves`).

## License

Apache-2.0 OR MIT (see LICENSE-APACHE, LICENSE-MIT). DCO not adopted.
