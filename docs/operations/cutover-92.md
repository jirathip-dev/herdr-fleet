# Cutover plan — retire the legacy queue supervisor for one Mac queue (issue #92)

**Status: PLAN + DRY-RUN + REHEARSAL ONLY. Nothing in this document has been
executed.** The live supervisor is running and untouched; the disable step
(section 6, S3) is owner-executed only after the approval gate (section 9).
This document is the reviewed artifact the gate approves; the executable
proof that precedes the gate is `scripts/cutover-92-dryrun.py` and the
rollback rehearsed against a disposable fixture by
`scripts/cutover-92-rehearsal.sh` (section 10).

Authority: issue #92 (closing acceptance) with the #81 reconciliation text —
*"With explicit scoped cutover approval disable only its fleet-ops
supervision, retain rollback and other fleets, and repeat the queue proof …
No authority to disable live supervisors or merge main is granted by this
spec edit. Execution/cutover approvals remain explicit."* This plan supplies
the reviewed steps, verification and rollback that approval is given against.

Concrete names, host paths, account identity and process identity never
appear in this public document. Every concrete value is a **binding key**
resolved from the private target descriptor (section 4), which is never
committed (repository public-data rule).

## 1. Scope — exactly what a cutover would touch

| Binding key | Meaning |
| --- | --- |
| `{queue_key}` | the one queue this cutover affects (the queue whose supervision is retired) |
| `{host_role}` | the single host role that runs that queue |
| `{supervisor_label}` | service-manager label of the legacy queue supervisor |
| `{supervisor_unit}` | unit file path of that label |
| `{supervisor_program}` | program path that label runs |
| `{supervisor_pattern}` | unique argv substring identifying the live supervisor process |
| `{identity_record}` | private JSON identity record captured at S1 |
| `{backup_dir}` | private backup directory for unit bytes, cron lines and evidence |
| `{canter_binary}` | the built `canter` binary that takes over the queue |
| `{canter_config}` | the queue's `canter` config file |
| `{canter_socket}` | the queue's daemon socket path |
| `{uid}` | the account the service manager domain belongs to |

**In scope** — exactly one queue, exactly one host role, exactly its
supervision set (section 3): its service-manager unit (disable, keep the
bytes), its cron entries (comment out, keep the lines), the live process
identity, and the private backup/record paths above.

**Explicitly NOT in scope** — every other queue and fleet on the host (their
units, labels and cron entries are never touched; the disable commands are
label-scoped), Hermes Agent as a platform, the Hermes scheduler, Herdr's
server, Corral, other hosts, other repositories, `main`, releases and every
production surface. No canter state is deleted or rewritten: queue
submissions are additive and journaled, and the backup directory is retained
until the owner accepts the cutover.

## 2. Why a cutover exists at all

`canter` owns admission for one approved selected-issue set on this queue:
the #83 read model, the #84 effect-free preview, the #85 durable
`queue.submit` executor, and the #90 Ratatui board. The legacy supervisor
still owns dispatch/rearm/watchdog for the same queue in parallel, so the
queue currently has **two** would-be dispatchers; the cutover removes the
legacy supervision for *this queue only* so the queue proof can be repeated
under canter alone (section 6, S5).

## 3. Current supervisor inventory (roles, by binding key)

Inventory of the supervision this plan retires, per the #81 item *"Inventory
legacy launch/dispatch/rearm/watchdog processes and external cron
dependencies for THIS queue"*. Concrete labels/paths/entries are recorded in
the private descriptor; only role keys appear here.

| Role | How it is realized | Disposition at S3 |
| --- | --- | --- |
| dispatch | the `{supervisor_label}` unit's program starts lanes for eligible issues of `{queue_key}` | disabled (S3), bytes retained |
| rearm | the same program re-arms stalled work for `{queue_key}` | disabled with the unit (S3) |
| watchdog | the same program restarts/kills stalled lane processes for `{queue_key}` | disabled with the unit (S3) |
| continuation | the cron entries below start the next eligible issue | cron lines commented out (S3) |
| cron dependencies | every crontab line whose command names `{queue_key}` (exact lines copied to `{backup_dir}` at S1) | commented out (S3), lines retained |
| notification side effects | any queue-facing notifier the descriptor records | left running (read-only, out of scope) |

Identity facts that must be captured before the disable (S1): pid, process
start time, full argv hash, unit bytes hash, and the running count (must be
exactly one — never two dispatch writers).

**Carrier confirmation.** This plan is written carriers-by-key because the
public tree may not name live identities. The lane's read-only host
observation (host-local log; not committed, no identity reproduced) found
no per-user crontab entry and no process whose argv names a fleet-ops
supervisor script, so the expected carriers are the per-user service-manager
agent(s) plus any scheduler entries the owner names. The owner confirms the
carrier list at the approval gate, and the descriptor's `commands` are where
the exact disable/restore strings live: if a non-launchd carrier is
identified (for example a per-user scheduler entry or a supervisor process
started by another tool), only those strings change — the ordered steps,
verifications and rollback shape stay the same (quiesce → record → disable →
verify → queue proof → decide).

## 4. Private target descriptor — `cutover-target/v1`

The dry-run consumes one JSON descriptor from **outside the tracked tree**
(the file is never committed). Placeholder substitution uses exactly these
`{...}` tokens; an unknown token is a refusal. Synthetic example (values are
obviously fake):

```json
{
  "schema": "cutover-target/v1",
  "queue_key": "example-queue",
  "host_role": "example-host-role",
  "supervisor": {
    "label": "com.example.fleet-supervisor",
    "unit_path": "~/Library/LaunchAgents/com.example.fleet-supervisor.plist",
    "program": "~/bin/example-supervisor",
    "process_pattern": "example-supervisor",
    "identity": {"argv_sha256": "<64 hex>"},
    "identity_record": "~/cutover-92/identity.json"
  },
  "rollback": {
    "backup_dir": "~/cutover-92/backup",
    "binaries": ["cp", "cmp", "launchctl"],
    "inputs": [
      {"path": "{backup_dir}/com.example.fleet-supervisor.plist", "sha256": "<64 hex>"}
    ],
    "commands": [
      {"id": "R1", "class": "sandbox-runnable",
       "run": "cp -p {backup_dir}/com.example.fleet-supervisor.plist {unit_path}"},
      {"id": "R2", "class": "sandbox-runnable",
       "run": "cmp -s {backup_dir}/com.example.fleet-supervisor.plist {unit_path}"},
      {"id": "R3", "class": "owner-executed",
       "run": "launchctl bootstrap gui/{uid} {unit_path}"},
      {"id": "R4", "class": "owner-executed",
       "run": "launchctl enable gui/{uid}/{supervisor_label}"}
    ]
  },
  "canter": {
    "binary": "~/bin/canter",
    "config": "~/.config/canter/canter.toml",
    "probes": [["--version"], ["status", "--json"], ["queue", "status", "--submission", "qs_0000000000000000", "--json"]]
  }
}
```

`class` is a rehearsal label only: `sandbox-runnable` commands are executed
by the rehearsal inside its disposable fixture; `owner-executed` commands
are syntax-checked and binary-resolved by the dry-run and only printed (as
`SIMULATED`) by the rehearsal — they are never executed by either script.

Paths in the descriptor may be written with a leading `~`; the dry-run
expands it and then requires the result to be absolute (a relative path or
an unknown `{...}` token is a refusal). The example above is synthetic.
`process_pattern` must be a distinctive argv substring that matches exactly
one process — the supervisor's; the dry-run refuses zero or multiple
matches, because two dispatch writers for one queue is exactly what the
cutover must never create.

## 5. Target mechanism (merged work) and its honest limits

| Merged slice | Surface | Role in the cutover |
| --- | --- | --- |
| #83 read model | `canter board` (`src/board.rs`, `src/tui/**`) | bounded, authoritative read-back for the queue |
| #84 preview | `queue_preview` (`hf-queue-preview/v1`) | deterministic, effect-free preview of the selected issue set |
| #85 executor | `canter queue submit --request FILE --confirm-digest HEX64 …` → `queue.submit`; `canter queue status --submission qs_…` | durable submission, per-item verdict, unique work ownership, read-back |
| #90 operator surface | `canter board` (Ratatui, terminal-default) | live board: work items, runs, freshness, explicit failed read |

Command reference (as rendered by `canter --help` and `canter queue --help`
at the merged head): `canter daemon run`, `canter daemon status --json`,
`canter board`, `canter queue submit …`, `canter queue status --submission
qs_…`, and the `service *-plan` commands, which render unit text and never
activate the host service manager ([OPERATIONS.md](../OPERATIONS.md)
section 5.1).

**Not replaced by merged work** (this plan does not claim full
replacement): automatic continuation / next-eligible-issue dispatch (#96),
safe-boundary pause/resume (#86), durable supervision reconciliation (#95),
restart/pause integrity proof (#98), the guarded supervision controls
(#97), and the TUI select/preview/authorize/start controls (#91) are all
still open upstream. Consequence for this plan: the operator path at S5 is
the CLI (`canter queue submit` plus `canter board` for observation), and
after S3 the queue is **operator-driven** for the cutover window — the
operator selects the next issue; nothing starts it automatically. That limit
is stated in the approval wording (section 9) and in section 11.

**PENDING INTEGRATION** (report-only; this lane must not take the files):

1. `src/commands.rs` — no read-only CLI verb renders the #84 preview
   document or prints its digest, so an operator cannot obtain the
   `--request` file and `--confirm-digest` value through the product surface
   (the preview is library-only today). Exact change: a read-only
   `canter queue preview … --json` that renders the `hf-queue-preview/v1`
   document and its digest, mutating nothing. Why: S5's operator path cannot
   be exercised end-to-end otherwise.
2. `src/daemon.rs` + `src/commands.rs` — no canter-owned continuation
   (issues #95/#96 unmerged), so the legacy supervisor's rearm/continuation
   role has no replacement yet; the cutover window is operator-driven and
   the queue is quiet between issues. Exact change: the #95/#96 slices
   (durable event-driven reconciliation + one bounded orchestrator
   continuation). Why: without it, retiring the supervisor removes
   automatic continuation, which is a real capability loss during the
   window (accepted only because the owner-approved window is bounded and
   rollback restores it).

## 6. Ordered cutover steps, each with its verification

All host commands are owner-executed. `{...}` tokens resolve from the
private descriptor. Before every step the runner appends the raw exit codes
to the private evidence log under `{backup_dir}`.

| # | Step | Exact action | Verification (must pass before the next step) |
| --- | --- | --- | --- |
| S0 | Approval + preconditions | owner records the section 9 approval; announce the window; confirm the queue is **quiescent** (no in-flight lanes for `{queue_key}`, no second dispatch writer, next-eligible set empty or the operator will drive it) | approval text recorded; `canter board` shows no active run for the queue; exactly one `{supervisor_pattern}` process exists |
| S1 | Record identity + back up the unit (the only writes before S3, all inside `{backup_dir}`) | `cp -p {supervisor_unit} {backup_dir}/` ; `shasum -a 256 {supervisor_unit} {backup_dir}/$(basename {supervisor_unit})` ; write `{identity_record}` = `{pid, started_at, argv_sha256, unit_sha256, count:1}` ; copy the queue's crontab lines | `cmp -s` byte-identical; both hashes equal; the record parses; the recorded pid is still the live pid |
| S2 | Pre-cutover dry-run | `python3 scripts/cutover-92-dryrun.py --target {private_descriptor}` | raw exit 0 and `SUMMARY result=ok … mutations=0` (section 10) |
| S3 | Disable **this queue's** legacy supervision only | `launchctl bootout gui/{uid}/{supervisor_label}` ; `launchctl disable gui/{uid}/{supervisor_label}` ; comment out exactly the queue's cron lines (kept in the private copy). Never uninstall, never delete the unit, never touch another label | `launchctl print gui/{uid}/{supervisor_label}` reports the label absent; the process table shows zero `{supervisor_pattern}` processes; the queue's cron lines are inactive |
| S4 | Verify the queue's product path under canter only | `canter daemon status --json` ; `canter board` ; `canter queue status --submission qs_…` (a previous submission if one exists) | raw exits 0; the board renders; the daemon reports running |
| S5 | Queue proof (bounded; this is #92's acceptance run) | operator selects one already-approved issue of the queue → preview document → `canter queue submit --request … --confirm-digest …` → read back the submission and the board; the operator starts the *next* issue by hand (no automatic continuation — section 5) | submission read-back shows the admitted item(s) with stable verdicts; the board shows the work item/run; the delivery evidence the owner accepts (#81: publish safe evidence) |
| S6 | Close or roll back | on PASS: owner records the accepted cutover, the backup stays until the owner says otherwise; on FAIL: execute section 7 immediately | recorded decision + raw evidence; on FAIL the rollback verification (R6) must pass |

Stop conditions: any verification above failing, any second supervisor
process appearing, any unexpected process starting lanes for `{queue_key}`,
or the window closing → stop and execute section 7.

## 7. Rollback — exact restore steps

Every step in section 6 is reversible; nothing is deleted at any point, so
rollback is a byte-restore plus a re-registration, all owner-executed.

| # | Step | Exact command | Verification |
| --- | --- | --- | --- |
| R1 | Restore the unit bytes | `cp -p {backup_dir}/{unit_basename} {unit_path}` | `cmp -s {backup_dir}/{unit_basename} {unit_path}` exits 0 and `shasum -a 256` equals the S1 hash |
| R2 | Re-register the unit | `launchctl bootstrap gui/{uid} {unit_path}` then `launchctl enable gui/{uid}/{supervisor_label}` | `launchctl print gui/{uid}/{supervisor_label}` reports the label present |
| R3 | Restore the queue's cron entries | re-enable exactly the lines copied at S1 (from the private copy) | the queue's lines are active again; no other line changed |
| R4 | Confirm the live identity | compare the running process against `{identity_record}` | exactly one `{supervisor_pattern}` process; argv hash equals the S1 value |
| R5 | Bounded restart if R4 fails | `launchctl kickstart -k gui/{uid}/{supervisor_label}` then repeat R4 | R4 passes on the retry; if it still fails, stop and report the failed edge — do not improvise |
| R6 | Post-rollback proof | re-run `python3 scripts/cutover-92-dryrun.py --target {private_descriptor}` | raw exit 0 (the preconditions hold again: artifacts, running supervisor, recorded identity, resolving rollback, reachable target) |

Rollback trigger: any S0–S6 verification failing, a second dispatch writer,
or the owner calling the window off. Expected bound: R1–R6 is a six-command
sequence; the only slow part is the service manager's own start latency.

## 8. Blast radius

- **Affected**: supervision of the single queue `{queue_key}` on the single
  host role `{host_role}`; between S3 and a passing S5/S6 that queue has no
  automatic dispatch, rearm, watchdog or continuation.
- **Not affected**: every other queue/fleet and every other service-manager
  label (the disable is label-scoped and cron lines are edited by exact
  match), Hermes/Herdr/Corral processes, canter state (additive, journaled,
  idempotent — no deletion), repositories, `main`, releases, production.
- **Worst case**: the queue sits unsupervised until rollback; no committed
  work is lost (quiescence is a precondition, the durable state keeps every
  committed row, and the unit bytes and identity are recorded before the
  disable). The window is bounded by the owner's announcement and closed by
  R1–R6 in minutes.
- **Writes performed by the cutover**: `{backup_dir}` + `{identity_record}`
  only, plus canter's own journaled state rows. Nothing else on the host is
  written.
- **Reversibility**: S3 → R1–R4; S1 → nothing to undo (additive copies);
  S5 → no rollback needed for canter state (additive); S4/S6 → read-only.

## 9. Approval gate — the owner is the sole approver

Execution may not begin until the owner records this, in their own words or
verbatim:

> Cutover approval (#92): I authorize the scoped disable of the legacy
> supervision for queue `{queue_key}` on `{host_role}` exactly as
> `docs/operations/cutover-92.md` describes. Rollback (section 7) is
> retained and I understand it restores the supervisor. No other fleet or
> queue is affected. I understand the merged canter surface does not include
> automatic continuation/supervision (#95/#96), so this window is
> operator-driven. Recorded: <owner>, <date/time>.
> — sole approver: the owner.

No other party — lane, reviewer, orchestrator or agent — may execute S3, or
authorize it, and nothing in this repository does so: the dry-run and the
rehearsal are side-effect-free by construction (section 10) and the disable
commands exist only as text in this plan.

## 10. How the plan is proven without executing it

```console
$ python3 scripts/cutover-92-dryrun.py --target <private descriptor>     # zero mutations
$ bash scripts/cutover-92-rehearsal.sh                                   # disposable fixture
$ python3 scripts/test-cutover-92.py                                     # mutation probes
```

- **Dry-run** (`--target` is required — a missing `--target` is a usage
  error, exit 2): proves (a) the unit/artifacts exist as
  this plan describes (unit parses, program matches, canter binary
  executable), (b) exactly one supervisor process is running and its argv
  hash matches both the descriptor and `{identity_record}`, (c) every
  rollback command substitutes, passes `bash -n` (syntax only — never
  executed) and resolves to an existing binary, with every restore input
  present and hash-matched, (d) the target mechanism answers read-only
  probes (`--version`, `status --json`, `queue status`) inside a fixed
  read-only allowlist. It performs **zero mutations** and proves it: the
  fingerprint (unit bytes, identity record, restore inputs, process
  identity) taken before the checks must equal the one taken after, or the
  run fails. Exit 0 only when every check passes; the last stdout line is
  the machine-checkable `SUMMARY result=… checks=… mutations=0`.
- **Rehearsal** (`bash scripts/cutover-92-rehearsal.sh`): builds a
  disposable sandbox under the OS temp dir, plants a fixture unit, a fixture
  supervisor process and a fixture descriptor, runs the dry-run against the
  sandbox, performs a simulated cutover **inside the sandbox**, then executes
  the descriptor's `sandbox-runnable` rollback commands for real and prints
  the `owner-executed` ones as `SIMULATED`. Containment is enforced at
  runtime: every path it touches must resolve under the sandbox root (and
  every path inside an executed command too), the fixture pid must be its own
  process, a non-temp root is refused, and an existing root is never reused.
  It never references the live label, the live unit or the service manager.
  `CUTOVER92_REHEARSAL_ROOT=<temp path>` picks the root;
  `CUTOVER92_REHEARSAL_KEEP=1` keeps the sandbox directory for inspection
  but still stops every fixture process, so no mode leaves a process behind.

  **Nothing this run did not create is ever deleted or killed (issue
  #92-R2):** the run writes an ownership marker in the sandbox it creates,
  and every removal — its own teardown and the reaper — requires that marker,
  so a refused or pre-existing operator path (including an existing
  `cutover-92-rehearsal.*` directory) is never reused, never emptied and
  never deleted; refusal clears the sandbox variable, and the teardown
  refuses rather than deletes when ownership cannot be proven. Only
  processes this run started (the fixture instances it spawned and its
  reaper) are ever signalled; a path-naming process it did not start — the
  operator's own shell, for example — is reported, never killed.

  **Zero fixture processes on every exit path (issue #92-R1):** teardown is
  verified, not assumed — the recorded fixture pids are stopped (TERM,
  bounded wait, KILL, bounded wait, re-scan), a sandbox *reaper* started
  before any fixture survives the rehearsal's own kills and finishes the job
  even when the rehearsal is killed with SIGKILL (where no trap can run), and
  the fixture itself self-destructs as soon as the rehearsal is gone.
  INT/TERM/HUP are trapped and re-raised through the exit trap. The final stdout line reports it:
  `REHEARSAL result=… steps=… verifications=… failures=… sandbox=…
  live_paths_touched=0 fixtures_left=0 sandbox_removed=yes mode=full
  dry_run_exit=…`; a non-zero exit means a verification failed or something
  survived teardown.

  `bash scripts/cutover-92-rehearsal.sh --dry-run-only` is the documented way
  to run the plan's dry-run invocation against a fixture descriptor: it
  plants the fixture, prints the exact command it runs
  (`python3 scripts/cutover-92-dryrun.py --target <sandbox>/host/target.json`),
  prints the dry-run's raw exit, tears everything down, and exits with the
  dry-run's code.
- **Self-test** (`python3 scripts/test-cutover-92.py`): mutation probes that
  prove each dry-run check fails closed (missing unit, hash mismatch,
  unresolved binary, syntax error, unknown placeholder, missing/extra
  supervisor process, identity mismatch, non-zero probe, disallowed probe
  argv, and an artifact mutated during the run → `nomutation` fails), plus a
  RED probe for the rehearsal's containment guard.

## 11. What this plan does not prove

- **No runtime behaviour after a disable exists yet.** Nothing here has been
  executed; the cutover's real behaviour is unproven until the owner runs
  S3–S5 on the host and records the outcome. This plan is not a claim of
  full replacement.
- **The live values are private and not inspected by this document.** The
  dry-run is where the concrete unit/program/identity are resolved, on the
  host, at run time.
- **The service-manager steps are unexecuted.** `launchctl` steps are
  syntax-checked and resolved, and the rollback path is rehearsed as a
  simulation; only the fixture lifecycle is exercised.
- **Automatic continuation is not replaced** (#95/#96/#97/#98 unmerged);
  the queue is operator-driven in the cutover window (sections 5 and 9).
- **#92's queue proof itself is not part of this lane** — it is S5, run
  after the gate.
- **The pre-existing admission gap** (`harness_lanes.unwrap_or(0)` in the
  #48 admission gate, reported open by the #85 lane) is out of scope and
  unchanged by this plan.
