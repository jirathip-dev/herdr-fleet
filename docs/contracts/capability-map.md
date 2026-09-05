# 1.0 capability map (AC1)

Refs #3 (deliverable: capability map). Organized by **portable behavior**,
not by historical executable names: a capability earns its own row when it
is behavior an operator composes, regardless of which earlier tool provided
it. The locked spec (umbrella #1) is authoritative; this map is its
capability-level decomposition.

## Decision classes

- **RETAIN** — 1.0 owns this portable behavior (module owner in the locked
  dependency graph, implemented by a later slice).
- **MERGE** — folded into another RETAIN capability as one surface (never
  two owners for one behavior).
- **SEPARATE** — behavior belongs to another product/owner; herdr-fleet only
  interoperates through a versioned contract if at all.
- **RETIRE** — deliberately omitted in 1.0, with the locked-spec reason.

Rules applied to every row: exactly one owner (AC1), a dependency position
(topological order, referenced by roadmap child issue), and a public
rationale grounded in the locked spec / ADR-0003. No owner-less or
position-less capability survives this table.

## Capabilities

| # | Capability (portable behavior) | Decision | Owner | Depends on | Public rationale |
| --- | --- | --- | --- | --- | --- |
| C1 | Read-only CLI: `doctor`, `status`, `plan` with `--json` and human output | RETAIN | CLI (binary) | C2 config, C17 exit/output contract | Locked spec: initial public work is read-only; read-only ops work without the daemon |
| C2 | Canonical XDG TOML config + one explicit optional policy overlay | RETAIN | Config (schemas/domain) | — | Locked spec: no implicit profile/repository merge stack |
| C3 | Harness/model/provider/role/skills/workflow/policy/repository/substrate as separate hash-recorded axes | RETAIN | Config (schemas/domain) | C2 | ADR-0003: axes compose, none inferred from another |
| C4 | Local daemon, per-user Unix socket, sole transition/retry/spawn/merge/state authority | RETAIN | Daemon | C2, C9 | Locked spec: one authority per layer; daemon owns mediated effects |
| C5 | SQLite-owned leases, idempotency claims, schedules, journals, recovery state | RETAIN | Daemon state | C4 | Locked spec: SQLite owns durable state; issues own intent, read-back owns reality |
| C6 | Deterministic plan/apply/verify machinery, digest-bound, idempotency-keyed | RETAIN | Plan engine (core) | C4, C5, C15 | Locked spec: plan-first, revalidated before apply, exactly read back |
| C7 | Route grants pinning repository/issue-revision/workflow-hash/policy-hash/phase/scope/caps/expiry/epoch | RETAIN | Grants (core) | C5, C6 | AC3; labels/comments alone never authorize |
| C8 | State epochs; restore rotates epoch and invalidates grants/digests | RETAIN | Grants/state (core) | C5, C7 | AC7; locked spec restore semantics |
| C9 | Closed typed workflow DAG engine + versioned bundled Doctrine workflow | RETAIN | Workflow engine (core) | C2, C4 | Locked spec: closed schema-versioned typed nodes only; running instances pin the version |
| C10 | Review evidence contract binding feature head / integration base / workflow hash / policy hash | RETAIN | Review gates (core) | C7, C9 | AC4; integration merge requires distinct exact-head reviewer + hosted checks |
| C11 | Herdr CLI/socket adapter (workspace/terminal/process execution substrate) | RETAIN | Adapter: herdr | C4, C18 compat | Locked spec: Herdr-first; external commands argv-only, bounded, scrubbed |
| C12 | Git/GitHub read-back + read-only status adapters | RETAIN | Adapter: git/forge | C18 compat | Locked spec: live Git/GitHub read-back owns observed reality |
| C13 | Official harness adapters: Hermes, Claude Code, Codex (+ constrained declarative argv adapter) | RETAIN | Adapter: harness | C3, C20 | Locked spec: first official adapters; others after 1.0; no shell templates |
| C14 | Agent-side skill packaging (`skills/herdr-fleet`) | RETAIN | Skill (docs) | C1 | Locked spec: generic skill installable from the repo, no machine-local policy |
| C15 | Bounded schedules (scoped, expiring, single-flight, coalesced ticks) | RETAIN | Daemon scheduler | C4, C5 | Locked spec runtime section; never schedules production/destructive (risk model) |
| C16 | SSH remote operation to a per-host daemon (system SSH only, no federation) | RETAIN | Daemon remote | C4 | Locked spec runtime section; optional after 1.0 core |
| C17 | Machine-readable output contract: JSON/JSONL, exit codes, human-vs-JSON, freshness/partial, redaction | RETAIN | CLI/daemon wire (schemas) | C1 | Issue #3 deliverable; stable surface list in locked spec |
| C18 | Versioned local read/event contract for optional read-only clients | RETAIN | Daemon wire (schemas) | C4, C17 | ADR-0003: dashed, optional future adapter; Corral absence never required |
| C19 | Release artifacts: checksums, SBOM, Sigstore provenance, completions/manpages | RETAIN | Release (repo ops) | C9 | Locked spec runtime/release; no self-update |
| C20 | Harness/forge capability negotiation with typed refusal | RETAIN | Adapter contract (core) | C13 | ADR-0003: unsupported capabilities return typed refusal, never shell guessing |
| C21 | Fleet Doctrine: judgment guidance (public) + machine-enforceable rules (CLI invariants/tests) | RETAIN | Doctrine (docs/domain) | C9 | ADR-0003: one canonical public home before the old Doctrine repo is archived |
| C22 | Corral visual board / notifications (optional human observability) | SEPARATE | Corral product | C18 (read contract only) | ADR-0003: Corral is a separate optional read-only product; no runtime dependency in either direction |
| C23 | iOS/notifications surface for fleet state | SEPARATE | Corral product | — | Same boundary as C22; owned downstream, not by herdr-fleet |
| C24 | Device-key registry, step-up approvals, signed-drive HTTP API (Corral-style remote control plane) | RETIRE | — | — | Locked spec: no network control API; same-user local trust + route grants replace it; SSH is the only remote path |
| C25 | Configless sidecar plugin engine / plugin cards | RETIRE | — | — | Locked spec: no dynamic plugin SDK in 1.0; native Herdr plugin optional and late, thin, never policy owner |
| C26 | TUI/web UI, hosted service, MCP server, notification provider, auto-update, Windows support | RETIRE | — | — | Locked spec product boundary non-goals |
| C27 | General personal/office automation and generic AI-agent platform | RETIRE | — | — | Locked spec non-goals; software-repository fleets only |

## Dependency ordering

Rows are laid out in dependency order (C1-C21 by child issue phase):

```text
C2/C3 config axes ─▶ C1 read-only CLI          (child #4 read-only core)
   │
   ├─▶ C4 daemon ─▶ C5 state ─▶ C6 plan engine (#5 daemon/state/audit)
   │                     └────▶ C7 grants, C8 epochs (#5)
C9 workflow engine (#6) ◀── C4/C5/C2
C11 herdr + C12 git/forge + C13 harness adapters (#7) ◀── C3, C20
C10 review evidence (#8) ◀── C7/C9
C15 schedules + C16 SSH (#9) ◀── C4/C5
C19 release + soak (#10) ◀── all RETAIN rows
C18 read contract + C22/C23 optional Corral adapter: post-1.0, demand-driven
```

Every proposed 1.0 capability (C1-C21) is RETAIN with exactly one owner and
a position in this order. C22-C27 are explicit non-1.0 placements, each with
a single owner or a RETIRE reason.

## Owner ↔ roadmap children

- CLI/domain/schemas core rows (C1-C3, C6-C10, C17, C20, C21): children #4,
  #5, #6, #8 (per locked-spec dependency graph).
- Daemon/state rows (C4, C5, C15, C16): child #5, lifecycle child #9.
- Adapters (C11-C13): child #7.
- Release (C19): child #10.
- SEPARATE rows: Corral product repositories (not this repo's roadmap).

Design commitments only; no module directories or code exist yet
(AGENTS.md bootstrap rule).
