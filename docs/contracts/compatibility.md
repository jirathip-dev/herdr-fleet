# Compatibility policy: Herdr, `gh`, and the harness adapters

Refs #3, #7 (deliverables: "A tested compatibility policy for Herdr and `gh`:
runtime capability negotiation plus declared minimum/current versions;
`herdr-fleet doctor` diagnoses prerequisites but never installs, starts,
stops, or upgrades Herdr" and, for issue #7, the declared supported version
ranges of the official harness adapters). Design commitment for the policy
shape; the version facts below are **measured** (Hermes/Claude Code/Codex
observed from the public GitHub/npm release metadata on 2026-09-06; the Pi
row was measured on 2026-09-08 against the SHA-256-verified linux-x64
prebuilt of the upstream release, with the real binary's version probe and
one-shot run exercised on the issue #33 sandbox; the Jcode row was
measured the same day against the SHA-256-verified linux-x64 prebuilt of
the upstream v0.84.0 release, with the real binary's version probe and
one-shot `run` argv row exercised on the issue #37 sandbox). Claims about
adapter behavior
against specific live versions are [awaiting-evidence] until the
human-gated clean-host smokes (issue #7 AC6) run on maintainer-controlled
hosts; this slice ships fake-executable exact-version contract tests
(`tests/harness_adapters.rs`) that public CI can run without credentials
(issue #7 AC2/AC7).

## Policy shape

1. herdr-fleet never embeds Herdr, `gh`, or any harness; all of them are
   runtime prerequisites for the operations that use them, discovered on
   PATH (never compiled-in paths).
2. **Runtime capability negotiation**: before using a prerequisite,
   herdr-fleet queries its declared capabilities over the same typed
   mechanism as every other adapter ([spec-capabilities.md](spec-capabilities.md),
   `hf-capability/v1`). Version checks are a first gate; capability checks
   are the actual contract.
3. **Declared versions** (this table is updated with each release that
   changes the declared range — updating it is a compatibility change, not
   a chore):

| Prerequisite | Declared minimum | Declared current | Evidence |
| --- | --- | --- | --- |
| Herdr CLI | 0.8.2 (minimum floor; exact API floor [awaiting-evidence] until clean-host contract tests confirm the CLI/socket surface) | v0.8.2 (latest semver release, 2026-08-19); preview builds (`preview-2026-08-31-b1ff4582e968`) are not a compatibility target | public release history of the upstream Herdr project |
| `gh` CLI | 2.x floor; exact floor [awaiting-evidence] (clean-host fork-PR read-back tests) | v2.100.0 (2026-09-03) | public release history of the upstream `gh` project |
| Hermes Agent (`hermes`) | 0.21.0 (provisional exact-version floor [awaiting-evidence]; the adapter contract was written against this release and older releases are refused until the clean-host matrix lowers the floor) | v0.21.0 (release tag `v2026.8.31`, 2026-08-31; PyPI `hermes-agent` 0.19.0 is not the compatibility target) | public release history of the upstream Hermes Agent project |
| Claude Code (`claude`) | 2.1.263 (provisional exact-version floor [awaiting-evidence], same rule) | v2.1.263 (npm `@anthropic-ai/claude-code`, 2026-09-06) | public npm/GitHub release history of the upstream Claude Code project |
| Codex CLI (`codex`) | 0.153.4 (provisional exact-version floor [awaiting-evidence], same rule) | v0.153.4 (npm `@openai/codex`, 2026-09-06) | public npm release history of the upstream Codex project |
| Pi (`pi`, earendil-works/pi) | 0.85.1 (exact-version floor measured live on 2026-09-08 against the SHA-verified linux-x64 prebuilt — the issue #33 lane ran the real binary; older versions are refused until the clean-host matrix lowers the floor; parity on other platforms [awaiting-evidence]) | v0.85.1 (2026-09-08; linux-x64 prebuilt verified and exercised; darwin arm64/x64 prebuilts are available at this version — a real darwin run is a human-gated clean-host smoke like #7 AC6) | public release metadata + measured real binary of the upstream Pi project (earendil-works/pi) |
| Jcode (`jcode`, 1jehuang/jcode) | 0.84.0 (exact-version floor measured live on 2026-09-08 against the SHA-verified linux-x64 prebuilt — the issue #37 lane ran the real binary's version probe and one-shot `run` argv row; older versions are refused until the clean-host matrix lowers the floor; parity on other platforms [awaiting-evidence]) | v0.84.0 (2026-09-08; linux-x64 prebuilt verified and exercised — version probe, `--` end-of-options guard, `--json` envelope shape and measured missing-key text; darwin arm64/x64 prebuilts are available at this version — a real darwin run is a human-gated clean-host smoke like #7 AC6; the issue #37 darwin arm64 canary measured peak RSS ~19.9 MB on the same INI task) | public release metadata + measured real binary of the upstream Jcode project (1jehuang/jcode) |

   "Declared minimum" is the version below which herdr-fleet refuses to
   operate (typed refusal + `doctor` diagnosis). "Declared current" is the
   version the release notes/support matrix names as the tested ceiling.
   Official adapters declare their ranges in `src/adapters.rs`
   (`OfficialSpec`) and their exact-version contract tests pin the declared
   current with fake executables; the floor rows for the first three harness
   adapters are provisional exact-version floors because no live harness
   runs were authorized on their lane (issue #7 stop condition). The Pi row
   is different: the issue #33 lane ran the SHA-verified v0.85.1 linux-x64
   prebuilt locally (probe + one-shot run, no credentials committed), so its
   floor is a measured exact-version floor for linux-x64 — parity on darwin
   and older-version behavior stay [awaiting-evidence] for the clean-host
   matrix (AC6), which is what measures macOS parity and lower floors. The
   Jcode row follows the same measured shape: the issue #37 lane ran the
   SHA-verified v0.84.0 linux-x64 prebuilt locally (version probe + the
   one-shot `run` argv row incl. its `--` end-of-options guard and `--json`
   envelope), so its floor is measured for linux-x64 with darwin parity and
   older-version behavior [awaiting-evidence] for the same clean-host
   matrix.

4. Negotiation outcome is versioned with the schema (`hf-capability/v1`)
   so a future minimum bump is an explicit, reviewable change rather than a
   silent behavior shift.
5. **`herdr-fleet doctor` boundary**: `doctor` diagnoses prerequisites —
   presence on PATH, version vs declared range, socket reachability where
   applicable, `gh` auth state. It **never installs, starts, stops, or
   upgrades** Herdr (or `gh`, or any harness). Installation/upgrade is the
   operator's toolchain concern; herdr-fleet only reports and refuses.
6. Herdr absence never breaks standalone help/version/config validation or
   supported non-dependent read-only operations (locked spec); the same
   applies to any single optional harness (issue #7 AC4).

## Measured vs awaiting evidence (AC10)

- Measured (this document): the version facts above and the policy shape.
- [awaiting-evidence]: minimum-version floors derived from tested APIs,
  socket-protocol compatibility per Herdr version, `gh` fork-PR behavior,
  real harness headless-flag parity per adapter (the `src/adapters.rs`
  invocation rows are v1 candidates), and any latency/resource claims —
  produced by the human-gated clean-host smokes (issue #7 AC6) +
  [benchmarks.md](benchmarks.md), then this table is updated with the
  evidence trail. Only redacted exact-version evidence is published.

## Probes in this slice

The adapter contract tests (issue #7) run fake executables that pin the
exact documented argv rows and version gates (`tests/harness_adapters.rs`),
so the policy is enforced structurally on public CI without any harness
credentials. Behavioral compatibility against real harness versions remains
[awaiting-evidence] until the clean-host smokes documented in
`.report-7.md` run on maintainer hosts.

## Capability-probe mapping (release readiness, issue #10 AC4)

Each row of the table above is tied to an in-tree probe that enforces or
pins it, so a release can state exactly which code asserts the declared
range (the mapping below is updated whenever a row or its probe moves —
updating it is a compatibility change, not a chore):

| Row | Enforcing/pinning probe |
| --- | --- |
| Herdr CLI minimum 0.8.2 | `src/observe.rs` `HERDR_MINIMUM = (0, 8, 2)` + `probe_version` compatibility gate (`doctor` refuses below the floor); behavior pinned by `tests/cli_readonly.rs` doctor rows with fake `herdr` executables |
| `gh` CLI 2.x floor | `src/observe.rs` `GH_MINIMUM = (2, 0, 0)` + the same `probe_version` gate; fork-PR read-back behavior is [awaiting-evidence] (clean-host fork-PR tests) |
| Hermes / Claude Code / Codex / Pi / Jcode minimum + current | `src/adapters.rs` `official_specs()` `VersionRange { minimum, current }` per adapter; the `hf-capability/v1` negotiation refuses below `minimum`; exact argv rows and gate behavior pinned by fake executables in `tests/harness_adapters.rs` (the pi fake pins the one-shot `--print` row with the `--` end-of-options guard; the jcode fake pins the one-shot `run --provider ... --model ... --json --` row with the same guard) |
| Negotiation envelope versioning | `hf-capability/v1` family fixtures + oracle (`scripts/check-contract-fixtures.py`) and `src/schema.rs` `SUPPORTED_FAMILIES` |
| Release-to-table binding | every release archive's `provenance.json` records the binary's schema facts (`herdr-fleet --version`: state schema version, migration chain, document families) and its exact source ref — the compatibility table's declared ranges live in that same binary and are exercised by the probes above (`scripts/build-archive.py`, `docs/RELEASING.md`) |

The release-readiness slice (issue #10) measured **no new live version
facts**: the 2026-09-06 declared-current rows above remain those observed
from public release metadata on that date, and the minimum-version floors
for the first three harness adapters stay provisional exact-version floors
([awaiting-evidence]) until the human-gated clean-host matrix (issue #7
AC6, `docs/RELEASING.md` clean-host command rows) runs on maintainer
hosts. Issue #33 subsequently added the Pi row (measured live on
2026-09-08 against the SHA-verified linux-x64 prebuilt; darwin parity
stays [awaiting-evidence]); issue #37 added the Jcode row the same way
(measured live on 2026-09-08 against the SHA-verified linux-x64
prebuilt; darwin parity stays [awaiting-evidence]). Removing an
[awaiting-evidence] marker
therefore requires a real measurement with its evidence trail — nothing in
this slice removes one.
