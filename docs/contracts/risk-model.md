# Daemon-owned fail-closed target/risk model (AC5)

Refs #3 (deliverable: daemon-owned fail-closed target/risk model; AC5:
production/destructive classification cannot be downgraded by a workflow
node or adapter). Locked by umbrella #1. Design commitment.

## Model

Every effect the daemon can produce is classified against a **target** and a
**risk class**. The classification table is daemon-owned and static: it
cannot be changed by configuration, workflow content, a grant, an adapter, a
route grant scope, or process output.

### Risk classes (lattice)

```text
READ            read-only effects; never mutate external state
PRODUCTION      mutates real shared state (branches, PRs, releases, grants,
                schedules) on configured repositories
DESTRUCTIVE     removes or overwrites state irrecoverably (force-push,
                branch/worktree deletion, cleanup of untracked content,
                restore rotations)
UNKNOWN         not classifiable -> inherits PRODUCTION + DESTRUCTIVE
                (fail closed; cannot be scheduled or automated)
```

## Target table (normative rows; the daemon extends it only via reviewed
schema/registry amendments)

| Target | Example effects | Base class | Notes |
| --- | --- | --- | --- |
| Local read state (cache, SQLite reads, plans) | compute plan, read status, list grants | READ | usable without daemon for read-only CLI ops |
| Config/state writes (daemon-owned) | lease, journal, audit, idempotency claim, epoch | PRODUCTION | journal-before-mutation (AC6); restore rotation = DESTRUCTIVE |
| Repository: read | fetch, log, status, diff | READ | — |
| Repository: shared refs | merge to integration/staging, tag, push | PRODUCTION | requires grant phase `merge` + review evidence (AC4) |
| Repository: destructive refs | force-push, delete branch/tag, rewrite | DESTRUCTIVE | fresh interactive TTY, never schedulable |
| Worktree | create/delete issue worktrees, salvage | create: PRODUCTION; delete: DESTRUCTIVE | path-contained, plan-bound |
| Process (harness/agent) | start, prompt, interrupt, collect outcome | PRODUCTION | argv-only, bounded, kill_on_drop, env allowlist |
| Process: kill/cleanup of unmatched processes | reaper-style termination | DESTRUCTIVE | requires process identity + grant cap `cleanup` |
| Filesystem (daemon state dir) | journal, migration, backup | PRODUCTION | backup = READ copy of daemon-owned state; restore = DESTRUCTIVE |
| Filesystem: external | any path outside daemon-owned state | DESTRUCTIVE by default | no adapter may lower this |
| Schedules | create/pause/resume scoped schedule | PRODUCTION | can never carry PRODUCTION/DESTRUCTIVE effects (locked spec) |
| Remote host (SSH, optional) | remote daemon ops via system SSH | PRODUCTION | host identity + remote revalidation plan-bound |
| Release | publish artifacts, tags, checksums | PRODUCTION | human promotion path only |
| Anything not listed | — | UNKNOWN → PRODUCTION + DESTRUCTIVE | fail closed |

## Non-downgrade rule (AC5)

- The risk class of an effect is determined by the **target table plus the
  effect kind**, statically, inside the daemon.
- Workflow DAG nodes declare effects; they cannot declare risk classes.
  A node that names an effect whose class is PRODUCTION/DESTRUCTIVE inherits
  that class — there is no field in `hf-workflow/v1` for a node to soften it
  ([spec-workflow.md](spec-workflow.md)).
- Adapters return typed results and typed refusals; they cannot classify
  their own effects. An adapter that cannot prove an effect's class is
  refused as UNKNOWN.
- Route grants select allowed caps from the closed set
  ([spec-plans.md](spec-plans.md)); a grant cannot widen an effect's class.
- The probe self-test pins the closed sets; a future change to the table is
  a schema/registry amendment with its own review.

## Operational rules for PRODUCTION and DESTRUCTIVE

1. **Plan-first**: no mutation without a digest-bound deterministic plan
   revalidated against fresh state ([spec-plans.md](spec-plans.md)).
2. **Journal-before-effect**: audit intent durably recorded before the
   mutation; journal failure aborts (AC6, [spec-state.md](spec-state.md)).
3. **Fresh interactive TTY confirmation** for production and destructive
   actions; same-user TTY is accidental-safety only (trust model T3).
4. **Never schedulable**: schedules and workflow automation cannot carry
   PRODUCTION/DESTRUCTIVE effects; unknown actions inherit them and are
   therefore unschedulable too.
5. **Idempotency-keyed and read back exactly**: effects carry an
   `ik_` key; after the effect the daemon verifies external state against
   the plan and records a typed outcome; exactly-once is never claimed
   across external systems.
6. **Restore is its own class**: restore creates a new state epoch,
   invalidates prior grants/digests, marks interrupted work ambiguous, and
   requires external reconciliation (AC7).
