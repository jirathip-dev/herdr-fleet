# Contributing

Thanks for considering a contribution to herdr-fleet. This is a public
repository with a strict public-data rule; please read this page and
[docs/WORKFLOW.md](docs/WORKFLOW.md) before opening anything.

## Scope

- This repository is in **bootstrap**: repository foundation, CI/security
  gates, documentation, and contributor tooling. There is no daemon,
  workflow engine, adapter, mutation, migration, or release behavior —
  implementing those is out of scope until their roadmap slices are routed.
- Durable work is **issue-first**: open/comment an issue before writing
  code, and reference it in your PR (`Refs #N`; never `Fixes`/`Closes`/
  `Resolves` — issues close only by maintainer decision).
- Roadmap children of the umbrella issue are unrouted; a PR implementing one
  will be declined regardless of quality.

## Process (short version)

1. Open/join an issue.
2. Branch from `staging` (never `main`).
3. Make small, focused changes; run `just ci` until green (see
   [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for the canonical gates and
   prerequisites).
4. Push and open a PR with **base `staging`**.
5. Respond to independent review; trusted-PR verdicts are recorded as
   evidence, external contributor PRs need one human maintainer approval.
6. Maintainers squash-merge to `staging`; promotion to `main` is a separate
   human-only PR.

## Public-data rule (binding)

Everything you commit is public. Never commit: host paths (e.g. absolute
user paths), private repository names, credentials or secret-shaped content,
provider/model policy, or live scheduler identity. CI enforces this with
`scripts/check-public-tree.py` (git-index parser with self-tests) and
gitleaks; a violation fails the PR. If you believe content must stay
private, it belongs in a private repository, not here.

## Tests and docs

- Rust changes: update/extend `tests/cli_smoke.rs` and unit/doc tests as
  appropriate; formatting is blocking (`cargo fmt --check` runs first in
  CI).
- Scanner changes: extend `scripts/test-check-public-tree.py` so every rule
  change has positive and negative fixture proof.
- Docs changes: keep one fact authoritative and link to it — do not repeat
  long command lists across README/DEVELOPMENT/CONTRIBUTING/AGENTS/skill.
  Check that relative links resolve (CI verifies README/docs links).

## Licensing

Licensed under either of Apache License, Version 2.0 or the MIT license, at
your option (see [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT)).
By contributing you agree your contribution is licensed under those terms.

**DCO is not adopted in this bootstrap** — no sign-off is required. If that
changes, it will be announced and documented here first.
