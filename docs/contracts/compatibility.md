# Compatibility policy: product name, Herdr, `gh`, and the harness adapters

Refs #3, #7, #35 (deliverables: "A tested compatibility policy for Herdr
and `gh`: runtime capability negotiation plus declared minimum/current
versions; `canter doctor` diagnoses prerequisites but never installs,
starts, stops, or upgrades Herdr" and, for issue #7, the declared supported
version ranges of the official harness adapters). Design commitment for the
policy shape; version facts are measured from public release metadata. Issue
#35 adds isolated live Herdr 0.9.0/protocol 22 probes, SHA-256-verified 0.8.2
binary cross-version probes, and portable synthetic contract tests. The
harness rows retain their own evidence qualifications below.

## Policy shape

1. canter never embeds Herdr, `gh`, or any harness; all of them are
   runtime prerequisites for the operations that use them, discovered on
   PATH (never compiled-in paths).
2. **Runtime capability negotiation**: harness profiles declare the closed
   operation set through `hf-capability/v1`
   ([spec-capabilities.md](spec-capabilities.md)); version checks are the first
   gate and capability checks authorize each adapter operation. Herdr's own
   client/server endpoint negotiation remains Herdr-owned. canter probes
   the CLI version and does not reimplement or bypass that socket handshake.
3. **Declared versions** (this table is updated with each release that
   changes the declared range — updating it is a compatibility change, not
   a chore):

| Prerequisite | Declared minimum | Declared current | Evidence |
| --- | --- | --- | --- |
| Herdr CLI | 0.8.2 (minimum retained: the issue #35 mixed-version matrix is red, so it does not authorize a floor bump; this is a CLI floor, not a claim that protocol-20 and protocol-22 endpoints interoperate) | v0.9.0 (2026-09-07; same-version Linux client/server and the issue #35 behavior surface exercised live) | [upstream v0.9.0 release notes](https://github.com/herdrdev/herdr/releases/tag/v0.9.0) + isolated live and portable contract probes below |
| `gh` CLI | 2.x floor; exact floor [awaiting-evidence] (clean-host fork-PR read-back tests) | v2.100.0 (2026-09-03) | public release history of the upstream `gh` project |
| Hermes Agent (`hermes`) | 0.21.0 (provisional exact-version floor [awaiting-evidence]; the adapter contract was written against this release and older releases are refused until the clean-host matrix lowers the floor) | v0.21.0 (release tag `v2026.8.31`, 2026-08-31; PyPI `hermes-agent` 0.19.0 is not the compatibility target) | public release history of the upstream Hermes Agent project |
| Claude Code (`claude`) | 2.1.263 (provisional exact-version floor [awaiting-evidence], same rule) | v2.1.263 (npm `@anthropic-ai/claude-code`, 2026-09-06) | public npm/GitHub release history of the upstream Claude Code project |
| Codex CLI (`codex`) | 0.153.4 (provisional exact-version floor [awaiting-evidence], same rule) | v0.153.4 (npm `@openai/codex`, 2026-09-06) | public npm release history of the upstream Codex project |
| Pi (`pi`, earendil-works/pi) | 0.85.1 (exact-version floor measured live on 2026-09-08 against the SHA-verified linux-x64 prebuilt — the issue #33 lane ran the real binary; older versions are refused until the clean-host matrix lowers the floor; parity on other platforms [awaiting-evidence]) | v0.85.1 (2026-09-08; linux-x64 prebuilt verified and exercised; darwin arm64/x64 prebuilts are available at this version — a real darwin run is a human-gated clean-host smoke like #7 AC6) | public release metadata + measured real binary of the upstream Pi project (earendil-works/pi) |
| Jcode (`jcode`, 1jehuang/jcode) | 0.84.0 (exact-version floor measured live on 2026-09-08 against the SHA-verified linux-x64 prebuilt — the issue #37 lane ran the real binary's version probe and one-shot `run` argv row; older versions are refused until the clean-host matrix lowers the floor; parity on other platforms [awaiting-evidence]) | v0.84.0 (2026-09-08; linux-x64 prebuilt verified and exercised — version probe, `--` end-of-options guard, `--json` envelope shape and measured missing-key text; darwin arm64/x64 prebuilts are available at this version — a real darwin run is a human-gated clean-host smoke like #7 AC6; the issue #37 darwin arm64 canary measured peak RSS ~19.9 MB on the same INI task) | public release metadata + measured real binary of the upstream Jcode project (1jehuang/jcode) |

   "Declared minimum" is the version below which canter refuses to
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

4. Harness negotiation outcomes are versioned with the schema
   (`hf-capability/v1`) so a future minimum bump is an explicit, reviewable
   change rather than a silent behavior shift.
5. **`canter doctor` boundary**: `doctor` diagnoses prerequisites —
   presence on PATH and version vs the declared range for Herdr, plus `gh`
   presence/auth state. It does not open Herdr's socket; server compatibility
   is reported by Herdr's own status surface and measured separately below.
   It **never installs, starts, stops, or upgrades** Herdr, `gh`, or any
   harness. Installation/upgrade is the operator's toolchain concern;
   canter only reports and refuses.
6. Herdr absence never breaks standalone help/version/config validation or
   supported non-dependent read-only operations (locked spec); the same
   applies to any single optional harness (issue #7 AC4).

## Herdr 0.9.0 compatibility (issue #35)

Herdr 0.9.0 uses numbered protocol 22. Its release notes introduce endpoint
compatibility generation 1, but explicitly require servers predating that
generation to receive one final upgrade. Herdr 0.8.2 uses numbered protocol
20 and reports no endpoint generation. The measured matrix is therefore:

| Direction / area | Result | Evidence and boundary |
| --- | --- | --- |
| 0.9.0 client → 0.9.0 server | PASS | Live isolated Linux scratch server: `status --json` reported protocol 22, `compatible:true`, `endpoint_compatible:true`, no restart needed. |
| 0.8.2 client → 0.9.0 server | RED | Live SHA-256-verified upstream 0.8.2 client: status reported protocol 20 → 22 and `compatible:false`; scoped `workspace get` and `pane read` returned `protocol_mismatch`. |
| 0.9.0 client → 0.8.2 server | RED | Live isolated 0.8.2 scratch server: status reported protocol 22 → 20, endpoint generation absent, and `compatible:false`; scoped `workspace list` returned `protocol_mismatch`. |
| Daemon RPC | PASS (portable) | `tests/daemon_rpc.rs` exercises the independent canter Unix-socket RPC. It does not call Herdr; the live 0.9.0 server remained running and untouched during the suite. |
| Events | PASS (live + portable) | A 0.9.0 scratch subscription acknowledged before `session.snapshot`, replayed no workspace event retained before subscription, and delivered the first post-subscription event, confirming live-only semantics. `tests/herdr_compatibility.rs` rejects snapshot-first and retained-replay traces. |
| Service plans | PASS (portable) | `tests/service_plans.rs` renders doctor/install/status/uninstall plans in fixture XDG dirs. These plans do not call Herdr or activate a host service. |
| Lifecycle | PASS (live + portable) | 0.9.0 accepted isolated custom `pane report-agent` working/idle rows. Issue #9 scheduling is daemon-owned and never calls upstream `events.subscribe`; its coalesced no-backlog behavior remains covered by `tests/daemon_lifecycle.rs`. |
| Primary workspace close | PASS (live + portable) | With a linked worktree workspace open, default close returned `workspace_group_close_required` and left both workspaces open; `workspace close --group` closed both. The portable model covers `close_group:false/true`. |
| `agent prompt --wait` | PASS (live + portable) | An isolated fake agent received the submitted text. No lifecycle activity returned `agent_prompt_stalled`; working → idle activity returned success. The portable trace model rejects submission-only idle. |
| `pane read` | PASS (live + portable) | `--source recent` returned a marker still in the unscrolled viewport. The portable response model rejects the pre-fix empty-text behavior. |

The mixed-version failures are an upstream endpoint boundary, not an adapter
serialization defect: neither direction can issue normal protocol-20/22 API
calls. No adapter-side retry or replay is safe. Update the client and server
together (or complete Herdr's supported handoff outside canter); do not
restart them from this CLI. Consequently `HERDR_MINIMUM` remains 0.8.2: issue
#35 is red in both required mixed-version directions and does not authorize
the conditional doctor-floor bump. That minimum describes the same-install
CLI surface and must not be read as mixed-server compatibility.

Evidence classes stay explicit:

- **Live 0.9.0:** isolated scratch server/workspaces only; event ordering,
  group close, prompt wait, pane read, and lifecycle reports were exercised.
- **Live mixed binaries:** the upstream 0.8.2 Linux asset was checksum-verified;
  both mixed directions used isolated/scoped reads, with the 0.8.2 server in a
  disposable scratch process. No installed binary or running user server was
  replaced, restarted, or reconfigured.
- **Portable contract tests:** `tests/herdr_compatibility.rs` contains public
  synthetic status/trace models and negative controls. It neither requires
  Herdr nor claims to be a second live run.
- **Not exercised:** other operating systems, a model-backed agent prompt,
  service-manager activation, and any non-scratch workspace remain manual
  evidence. They are not claimed PASS here.

## Measured vs awaiting evidence (AC10)

- Measured: declared-current version facts plus the Herdr 0.9.0 same-version
  behavior probes and both 0.8.2/0.9.0 mixed-version directions above.
- [awaiting-evidence]: other Herdr version/platform combinations, `gh`
  fork-PR behavior, real harness headless-flag parity per adapter (the
  `src/adapters.rs` invocation rows are v1 candidates), and any
  latency/resource claims — produced by the human-gated clean-host smokes
  (issue #7 AC6) + [benchmarks.md](benchmarks.md), then this table is updated
  with the evidence trail. Only redacted exact-version evidence is published.

## Probes in this slice

`tests/herdr_compatibility.rs` is the portable issue #35 contract suite. Its
negative controls reject mixed protocol status, snapshot-before-subscribe,
retained-history replay, implicit primary-workspace group close, prompt waits
without post-submission activity, and empty recent reads for unscrolled
output. It also guards that issue #9's runtime does not acquire an upstream
`events.subscribe` dependency. The live evidence above is complementary: the
portable test never pretends to run a Herdr server.

The adapter contract tests (issue #7) separately run fake executables that pin
exact harness argv rows and version gates (`tests/harness_adapters.rs`), so
that policy remains enforceable on public CI without harness credentials.
Behavioral compatibility against real model-backed harness versions remains
[awaiting-evidence] until the clean-host smokes documented in `.report-7.md`
run on maintainer hosts.

## Capability-probe mapping (release readiness, issue #10 AC4)

Each row of the table above is tied to an in-tree probe that enforces or
pins it, so a release can state exactly which code asserts the declared
range (the mapping below is updated whenever a row or its probe moves —
updating it is a compatibility change, not a chore):

| Row | Enforcing/pinning probe |
| --- | --- |
| Herdr CLI minimum/current + mixed-version matrix | `src/observe.rs` retains `HERDR_MINIMUM = (0, 8, 2)` because the mixed matrix is red; `tests/cli_readonly.rs` pins the doctor gate, while `tests/herdr_compatibility.rs` pins protocol-20/22 refusal and the 0.9.0 behavior contracts above |
| `gh` CLI 2.x floor | `src/observe.rs` `GH_MINIMUM = (2, 0, 0)` + the same `probe_version` gate; fork-PR read-back behavior is [awaiting-evidence] (clean-host fork-PR tests) |
| Hermes / Claude Code / Codex / Pi / Jcode minimum + current | `src/adapters.rs` `official_specs()` `VersionRange { minimum, current }` per adapter; the `hf-capability/v1` negotiation refuses below `minimum`; exact argv rows and gate behavior pinned by fake executables in `tests/harness_adapters.rs` (the pi fake pins the one-shot `--print` row with the `--` end-of-options guard; the jcode fake pins the one-shot `run --provider ... --model ... --json --` row with the same guard) |
| Negotiation envelope versioning | `hf-capability/v1` family fixtures + oracle (`scripts/check-contract-fixtures.py`) and `src/schema.rs` `SUPPORTED_FAMILIES` |
| Release-to-table binding | every release archive's `provenance.json` records the binary's schema facts (`canter --version`: state schema version, migration chain, document families) and its exact source ref — the compatibility table's declared ranges live in that same binary and are exercised by the probes above (`scripts/build-archive.py`, `docs/RELEASING.md`) |

The release-readiness slice (issue #10) added the probe mapping without new
live version facts. Issues #33 and #37 subsequently measured the Pi and Jcode
Linux rows. Issue #35 now advances Herdr's declared current to 0.9.0 and
records the live same-version and red mixed-version matrix above; it does not
change any harness row or remove that row's [awaiting-evidence] qualification.

## Product rename (issue #106)

The product was renamed **herdr-fleet → canter** (the GitHub repository was
renamed earlier; old URLs redirect). This section is the **authoritative**
compatibility contract for that rename — other documents link here instead of
restating it. Nothing that is a live or persisted external contract is
migrated destructively, and every pre-rename form below either keeps working
or is retained unchanged; the alias/fallbacks are removed only by a future
release that announces the removal in CHANGELOG.md (itself a compatibility
change).

| Surface | Pre-rename form | Delivered behavior |
| --- | --- | --- |
| Binary name | `herdr-fleet` | The `herdr-fleet` alias binary is built and shipped next to `canter`; it prints a one-line deprecation warning on stderr and runs the identical CLI surface (`src/bin/herdr-fleet.rs`, shared `commands::cli_main`). |
| State tree | `$XDG_STATE_HOME/herdr-fleet/` (default `~/.local/state/herdr-fleet/`) | **Adopted in place** when it exists and `canter/` does not: the daemon opens and (non-destructively) migrates the pre-rename tree and keeps using it — nothing is copied, moved, or deleted. Once a `canter/` tree exists it is authoritative; the leftover pre-rename tree is ignored and never touched. |
| Runtime / socket dir | `$XDG_RUNTIME_DIR/herdr-fleet/` | Follows the state-tree choice (socket and state stay together), so a running pre-rename daemon's socket is still found by the renamed CLI. |
| Config path | `$XDG_CONFIG_HOME/herdr-fleet/config.toml` | Still discovered as a fallback when no `canter/config.toml` exists, and read in place. An explicit `--config PATH` is unchanged. |
| Crash-point env var (debug builds only) | `HERDR_FLEET_CRASH_POINT` | Both `CANTER_CRASH_POINT` and the pre-rename name are honored; release binaries ignore both. |
| Rendered service units | `com.herdr-fleet.daemon` (launchd), `herdr-fleet.service` (systemd) | New `service *-plan` output renders `com.canter.daemon` / `canter.service`. An already-installed pre-rename unit keeps working because its program path is the binary the operator installed and the pre-rename binary name remains available (alias above); the CLI never touches the service manager, so retiring a pre-rename unit is a deliberate operator step. |
| Herdr integration source ids | `custom:herdr-fleet-pi`, `custom:herdr-fleet-jcode` | Retained unchanged: these are live Herdr registry identifiers for running lanes, and renaming live registry identity is out of scope for the product rename. |
| Default Herdr/terminal session name | `herdr-fleet-lane` | Retained unchanged for the same live-identity reason. |
| Machine contract | `hf-*` schema families, envelopes, exit codes, daemon RPC | Unchanged: schema ids and the wire contract are versioned independently of the product name. |
| `capabilities --json` self-report | `"actor":"herdr-fleet"` | Now `"actor":"canter"` (the CLI's own declaration; not a persisted contract). |
| Frozen architecture renders | `docs/architecture/herdr-fleet.*.architecture.{json,html,png}` | Kept byte-frozen with their pinned SHA-256 and filenames: they are the dated v0.1.0 / locked-target records (their rendered titles show the name as it was then). |
| Historical reports | `.report-*.md` | Kept as written (historical evidence), old name included. |

The rename sweep (`tests/rename_sweep.rs`) machine-checks exactly this table:
any `herdr-fleet` / `herdr_fleet` / `HERDR_FLEET` occurrence in the tracked
tree outside the enumerated legacy/historical set fails the test.
