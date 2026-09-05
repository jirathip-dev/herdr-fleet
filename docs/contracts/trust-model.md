# Trust model

Refs #3 (deliverable: trust model). Locked by umbrella #1 (trust and safety
section). Design commitment; implementation evidence arrives with the
daemon/adapters slices.

## Statements

### T1. Configured repositories and executables are trusted to execute

Anything the operator explicitly configured — repository clones and the
harness executables named in config — is inside the trusted set: the daemon
may start them and read/write their data per grants. Trust is granted by
**explicit configuration plus a route grant**, never by discovery or
inference.

### T2. Issue text, names, paths, remote records, and process output are untrusted data

Everything arriving from outside the daemon — GitHub issue bodies/comments,
workflow names, repository/branch strings, remote refs, PR titles, agent
process output, harness transcripts — is **data, never code**. Rules:

- No shell evaluation of any untrusted string: subprocesses run from argv
  arrays only ([spec-capabilities.md](spec-capabilities.md), ADR-0003).
- Untrusted strings may appear in typed fields and documents, but they can
  never select an executable, a workflow node, a capability, a policy, or a
  risk class.
- Output is capped and redacted at the adapter boundary before it becomes a
  canonical record ([spec-cli.md](spec-cli.md) redaction rule).
- Names/paths from remote records are validated against closed formats and
  path-containment rules before they address local state.

### T3. Same-user mode is an accidental-safety boundary, not isolation

A same-user TTY approval, config file, or Unix socket grants protection
against **accidental and cooperative misuse only**. It is not protection
against a malicious process running as the same OS user: that peer can read
the same files, the same socket, and the same config. herdr-fleet never
claims otherwise (ADR-0003 consequence).

### T4. Hardened deployments require a separate OS principal or external sandbox

Where an operator needs real isolation — untrusted workflow content,
multi-tenant exposure, malicious-peer resistance — the documented hardening
path is a dedicated OS principal/container plus external branch controls
(and, for remote, system SSH identity boundaries). This is deployment
policy, owned downstream; the public repo specifies the boundary, not the
deployment.

### T5. herdr-fleet stores no harness/GitHub credentials

Credentials stay in their owning tools (Herdr, `gh`, harness CLIs, OS
keychains). herdr-fleet:

- never persists tokens, keys, or secrets in its SQLite state or journals;
- never reads credential files out of other tools' config;
- passes **only explicit environment allowlists** to subprocesses
  (`env_allow` in `hf-config/v1`, [spec-config.md](spec-config.md));
- redacts secret-shaped text from every canonical record before it can be
  serialized ([spec-cli.md](spec-cli.md)).

### T6. Labels and comments do not authorize

GitHub labels, comments, or issue status alone never constitute a route
grant. Authorization is a `hf-grant/v1` record binding repository identity,
exact issue/acceptance revision, workflow hash, policy hash, phase, scope,
caps, expiry, and state epoch (AC3, [spec-plans.md](spec-plans.md)).

### T7. Unknown inherits the worst class

Anything the daemon cannot classify — unknown action, unknown target, unknown
capability, unparsable document, unsupported schema version — is refused and,
where an action class is at stake, inherits production-and-destructive
treatment ([risk-model.md](risk-model.md), AC5). Typed refusal beats
guessing in every adapter ([spec-capabilities.md](spec-capabilities.md)).

## Boundaries implied

| Boundary | Inside | Outside |
| --- | --- | --- |
| Execution | Explicitly configured repositories + harness executables, plan-bound, grant-checked | Everything else |
| Data | Issue/acceptance revision, grant, workflow/policy hashes (as recorded) | Issue text semantics, remote claims, process output meaning |
| Authority | Daemon (sole transition authority) + human TTY confirmations | LLM orchestrators (typed advisory steps only), labels/comments, adapters |
| Secrets | Owning tools; explicit env allowlists | herdr-fleet state, logs, journals, events |
