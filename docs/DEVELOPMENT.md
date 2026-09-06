# Development

Canonical developer documentation. One fact is authoritative here; other
documents link here instead of repeating command lists.

## Prerequisites (exact versions)

| Tool | Version | Install |
| --- | --- | --- |
| Rust toolchain | 1.97.1 (pinned in `rust-toolchain.toml`) | `rustup toolchain install 1.97.1 --profile minimal --component rustfmt,clippy` (rustup: https://rustup.rs) |
| just | any recent (1.x) | `brew install just` (macOS) or https://github.com/casey/just |
| cargo-deny | 0.20.2 | `cargo install cargo-deny --version 0.20.2 --locked` |
| cargo-audit | 0.22.2 | `cargo install cargo-audit --version 0.22.2 --locked` |
| gitleaks | v8.30.1 | `brew install gitleaks` (macOS), or a pinned binary from https://github.com/gitleaks/gitleaks/releases — **required**, not optional |
| Python | 3.x (stdlib only) | your system/uv Python is fine |

All Rust commands below automatically select the pinned toolchain from
`rust-toolchain.toml` (rustup honors the file; install the toolchain once
with the command above).

## Canonical gate list (`just`)

The `justfile` is the canonical entry point. `just --list` shows every
recipe; `docs/DEVELOPMENT.md` is the single authoritative description.

| Recipe | Command behind it | Blocking |
| --- | --- | --- |
| `just fmt` | `cargo fmt` | no (applies formatting) |
| `just fmt-check` | `cargo fmt --check` | yes |
| `just check` | `cargo check --locked --all-targets` | yes |
| `just lint` | `cargo clippy --locked --all-targets -- -D warnings` | yes |
| `just test` | `cargo test --locked` | yes |
| `just doc` | `cargo doc --locked --no-deps` | yes |
| `just build-release` | `cargo build --release --locked` | yes |
| `just security` | scanner self-test + public-tree scan, `cargo deny check`, `cargo audit`, gitleaks dir scan | yes |
| `just ci` | fmt-check → check → lint → test → doc → build-release → security (fail-fast) | yes |

Raw failures and raw exit codes are preserved: recipes never pipe a gate
through grep as a pass/fail test, and `just` stops at the first failing
recipe.

### Missing optional tool behavior

`just security` requires `gitleaks` on `PATH`. If it is missing the recipe
**fails** with an actionable message (install commands above); it never
silently skips the content scan. cargo-deny and cargo-audit are installed via
`cargo install ... --locked`; if they are missing, `cargo deny`/`cargo audit`
fail with rustup/cargo's own actionable errors.

## Toolchain pin and update process

- The channel, profile, and components live **only** in `rust-toolchain.toml`
  (single source of truth; CI reads the same file).
- To update: change the file on a branch, prove the full local gate list
  green on this toolchain, then land it through the normal review flow. Do
  not float versions in workflow prose.
- MSRV policy: the declared `rust-version` in `Cargo.toml` matches the
  pinned toolchain. If an older MSRV is ever declared, CI must actually test
  it.

## Test layout

- Unit tests live beside their modules under `src/` (`cargo test` unit phase):
  strict JSON/TOML parsing and the closed `Val` model (`value.rs`), canonical
  form including the trailing newline and digest rules (`canonical.rs`),
  RFC3339 formatting (`time.rs`), identity/hex/slug validators (`formats.rs`),
  adapter-boundary redaction (`redact.rs`), the seven `hf-*` family
  validators with per-family discrimination (`schema.rs`), config
  discovery/typing/policy overlay (`config.rs`), the bounded process runner
  (`process.rs`), observation adapters and revision binding (`observe.rs`),
  deterministic plan rendering (`plan.rs`), and command parsing/envelope
  emission (`commands.rs`).
- `tests/cli_smoke.rs` — integration tests against the **real compiled
  binary** (`CARGO_BIN_EXE_herdr-fleet`): help exits 0 and prints usage,
  version exits 0 and prints name + version, unknown flag and no-args exit
  non-zero (2) with usage on stderr, usage lists only implemented commands,
  and `--json` mode writes exactly one envelope to stdout without prompting.
- `tests/cli_readonly.rs` — behavior tests against the real binary with
  **fake `herdr`/`gh` executables on a controlled PATH and synthetic local
  git repositories**: doctor rows (ok / missing / invalid config exit 5),
  status completeness + degradation semantics, deterministic `plan`
  rendering whose digest equals sha256 over the canonical plan bytes, plan
  revisions that bind the *redacted* acceptance text (and differ from the
  raw-text hash), untrusted input (hostile config paths, issue text with
  secret-shaped tokens) never breaking argv or JSON framing, and exit codes
  matching envelope `exit_code` fields.
- `tests/daemon_rpc.rs` (issue #5) — spawns the real daemon binary over a
  fixture Unix socket: single-writer refusal, stale-socket recovery, socket
  perms + symlink containment, RPC status/capabilities, crash-boundary
  restart/reconcile (AC4), fail-closed readonly state dir (AC5), restore
  epoch rotation + spent-claim semantics (AC6), event subscribe
  snapshot/replay/resnapshot/backpressure (AC7).
- `tests/service_plans.rs` (issue #5) — `service doctor|install-plan|
  status-plan|uninstall-plan` on fixture XDG homes; asserts plans never
  activate the host service manager.
- `tests/no_network_surface.rs` (issue #5) — static scan proving the daemon
  slice adds no TCP/UDP/telemetry/auto-update surface.
- `scripts/test-check-public-tree.py` — self-tests for the privacy scanner;
  each test builds a temporary git repo proving a rule bites (or that a
  clean fixture passes).

## CI parity

Hosted CI mirrors the local gates exactly (see `.github/workflows/ci.yml`):

| Hosted job | Local equivalent |
| --- | --- |
| `policy` | scanner self-test + scan, doc-link/required-file/promotion/action-pin checks (extra policy checks have no local recipe; run the workflow on your PR) |
| `rust-ubuntu` | `just fmt-check`, `just check`, `just lint`, `just test`, `just doc`, `just build-release` |
| `rust-macos` | locked build + CLI smoke + tests on macOS |
| `supply-chain` | `cargo deny check`, `cargo audit` (pinned installs) |
| `secret-scan` | gitleaks dir scan + `python3 scripts/check-public-tree.py .` |

Formatting runs **first** in `rust-ubuntu` — before any crate download or
compile.

## Coverage — Phase 1 decision

**Phase 1: no coverage gate.** The scaffold's test surface was a CLI metadata
smoke suite; there was no domain logic yet whose branches a coverage floor
would meaningfully protect. Rather than publish a fake 100% badge or an
empty-profile pass, coverage measurement is deferred until a behavioral
slice lands with an explicit coverage contract in its route grant; that
slice must establish a measured baseline and a floor below it, prove RED on
an unobserved branch, and fail on absent/empty profiles.

## Troubleshooting

- **`cargo` uses the wrong toolchain**: run `rustup show` in the repo root;
  `rust-toolchain.toml` must be present. Reinstall with the prerequisite
  command above.
- **`cargo fmt`/`clippy` not found**: the toolchain was installed with a
  profile lacking those components — add them:
  `rustup component add rustfmt clippy --toolchain 1.97.1`.
- **`just security` fails on gitleaks**: gitleaks is required; install it
  (see prerequisites). It is not optional and will not be skipped.
- **Locked commands fail with "lock file needs to be updated"**: commit the
  updated `Cargo.lock` (or run `cargo generate-lockfile` when changing
  `Cargo.toml`); `--locked` is intentional for reproducibility.
- **Scanner reports a violation**: run
  `python3 scripts/check-public-tree.py .` and read the `RULE-ID<TAB>path`
  lines (values are never printed). Remove the offending file/content or
  (for deliberate synthetic examples) keep the `.example` suffix and ensure
  content stays synthetic.
- **A gate passes locally but fails in CI**: CI runs the exact same
  commands on clean checkouts of both host families; retry once (transient
  runner issues happen), then compare raw output with your local log.

## Links

- [WORKFLOW.md](WORKFLOW.md) — branch/PR/merge process
- [ARCHITECTURE.md](ARCHITECTURE.md) — design and boundaries
- [CONTRIBUTING.md](../CONTRIBUTING.md) — contribution rules
