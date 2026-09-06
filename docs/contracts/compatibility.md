# Compatibility policy: Herdr, `gh`, and the harness adapters

Refs #3, #7 (deliverables: "A tested compatibility policy for Herdr and `gh`:
runtime capability negotiation plus declared minimum/current versions;
`herdr-fleet doctor` diagnoses prerequisites but never installs, starts,
stops, or upgrades Herdr" and, for issue #7, the declared supported version
ranges of the official harness adapters). Design commitment for the policy
shape; the version facts below are **measured** (observed from the public
GitHub/npm release metadata on 2026-09-06). Claims about adapter behavior
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

   "Declared minimum" is the version below which herdr-fleet refuses to
   operate (typed refusal + `doctor` diagnosis). "Declared current" is the
   version the release notes/support matrix names as the tested ceiling.
   Official adapters declare their ranges in `src/adapters.rs`
   (`OfficialSpec`) and their exact-version contract tests pin the declared
   current with fake executables; the floor rows for the three harness
   adapters are provisional exact-version floors because no live harness
   runs are authorized on this lane (issue #7 stop condition) — the
   clean-host matrix (AC6) is what measures older-version parity.

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
