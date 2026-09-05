# Compatibility policy: Herdr and `gh`

Refs #3 (deliverable: "A tested compatibility policy for Herdr and `gh`:
runtime capability negotiation plus declared minimum/current versions;
`herdr-fleet doctor` diagnoses prerequisites but never installs, starts,
stops, or upgrades Herdr"). Design commitment for the policy shape; the
version facts below are **measured** (observed from the public GitHub API
on 2026-09-06). Claims about adapter behavior against specific versions are
[awaiting-evidence] until child #7 runs adapter contract tests.

## Policy shape

1. herdr-fleet never embeds Herdr or `gh`; both are runtime prerequisites
   for the operations that use them, discovered on PATH (never compiled-in
   paths).
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
| Herdr CLI | 0.8.2 (minimum floor; exact API floor [awaiting-evidence] until child #7 tests `herdr --version`/CLI surface against the live socket) | v0.8.2 (latest semver release, 2026-08-19); preview builds (`preview-2026-08-31-b1ff4582e968`) are not a compatibility target | public release history of the upstream Herdr project |
| `gh` CLI | 2.x floor; exact floor [awaiting-evidence] (fork-PR read-back tests in child #7) | v2.100.0 (2026-09-03) | public release history of the upstream `gh` project |

   "Declared minimum" is the version below which herdr-fleet refuses to
   operate (typed refusal + `doctor` diagnosis). "Declared current" is the
   version the release notes/support matrix names as the tested ceiling.

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
   applies to any single optional harness.

## Measured vs awaiting evidence (AC10)

- Measured (this document): the version facts above and the policy shape.
- [awaiting-evidence]: minimum-version floors derived from tested APIs,
   socket-protocol compatibility per Herdr version, `gh` fork-PR behavior,
   and any latency/resource claims — produced by child #7 adapter contract
   tests + [benchmarks.md](benchmarks.md), then this table is updated with
   the evidence trail.

## Probes in this slice

No implementation exists to probe (issue non-goals). The compatibility
policy is enforced structurally by the fixture gates only in the sense that
capability documents and refusal envelopes are closed sets; behavioral
compatibility tests land with the adapters (child #7).
