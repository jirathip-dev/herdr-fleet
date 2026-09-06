# Workflow

Contributor and maintainer workflow for herdr-fleet. This is the process
contract; mechanical enforcement lives in CI (`policy` job) and in
repository rulesets applied by maintainers.

## Issue-first

Durable work starts from an issue. The umbrella
[#1](https://github.com/jirathip-dev/herdr-fleet/issues/1) tracks the
approved architecture and delivery graph; its children are routed one slice
at a time. In this bootstrap, `ready-to-work` is **bootstrap-process state**
recording that an issue is authorized and queued under today's external
process — it is **not** the future product's route-grant mechanism (route
grants are a locked-target concept, not yet implemented).

## Contributor flow

1. **Issue first** — comment or open an issue describing intent; never open
   a PR without one.
2. **Branch from `staging`** — feature/dependency branches cut from the
   current `staging` head: `git fetch origin && git checkout -b my-change
   origin/staging`.
3. **Commit** — small, focused commits; `Refs #N` wording (never
   `Fixes`/`Closes`/`Resolves`; issues stay open until a maintainer closes
   them).
4. **Gates** — run `just ci` locally until green, then push and open a PR
   with **base `staging`**.
5. **Review** — trusted fleet PRs are reviewed by an **independent
   exact-head agent**; its verdict is recorded as evidence. Zero formal
   GitHub approvals are required for those PRs (repository rulesets allow
   squash merges without approvals).
6. **External contributors** — a PR whose head repository differs from this
   repository is marked `EXTERNAL_CONTRIBUTOR — human maintainer approval
   required` by CI (an annotation, not a failure) and **additionally
   requires one human maintainer approval** before merge.
7. **CI** — every intended check must pass at the exact reviewed head
   (`policy`, `rust-ubuntu`, `rust-macos`, `supply-chain`, `secret-scan`).
8. **Merge** — reviewed + green PRs squash-merge into `staging`. Squash
   commits keep history linear; branch deletion follows automatically.

## Maintainer flow

### Promotion (staging → main)

`main` is the stable, default, release branch; `staging` is the permanent
integration branch.

- Promotion is a **dedicated PR from `staging` to `main`** — the only
  ordinary PR to `main` that exists. It is **human-only** and requires the
  full CI suite on `main`-targeting PRs.
- The always-present promotion-policy check fails any PR to `main` whose
  head is not `staging` (or a `hotfix/*` branch).
- Direct pushes, force/non-fast-forward updates, and branch deletion are
  forbidden on both long-lived branches by repository rulesets; only PR
  squash merges are allowed, with linear history.

### Hotfix exception (narrow, fail-closed)

An incident/security fix may target `main` directly from a `hotfix/*`
branch **based on current `main`**, and only with:

1. fresh human approval (CI never grants it — the policy job annotates
   exactly this),
2. focused review,
3. required CI green,
4. patch-release evidence, and
5. **mandatory `main` → `staging` reconciliation** immediately after.

The policy check allows `hotfix/*` heads with a warning annotation
recording that the path requires that human approval and reconciliation;
it does not itself authorize anything.

### Releases

Releases happen from `main` only, on demand, per
[RELEASING.md](RELEASING.md). The release-*readiness* machinery
(deterministic archives + checksums/SBOM/provenance, verification and
clean-host scripts, version/schema policy) is active in-repo; every actual
release execution (promotion, tag, upload, attestation, soak) is a
separate human decision and never runs from CI or agent lanes.

### Required checks and metadata

Required status checks and branch rulesets are maintainer-owned metadata
applied **after** CI check names are observed from real runs — never
guessed. This bootstrap ships source + workflows only.

### Recovery when CI is unavailable

If hosted CI is down or unusable:

1. State it on the PR — do not merge on self-reported green alone.
2. Run `just ci` locally on the exact head and paste the raw exit codes
   into the PR (evidence, not replacement).
3. Wait for hosted CI to confirm before merge; if a required check cannot
   run (infrastructure), escalate to a maintainer rather than bypassing.

### Branch deletion

After a squash merge to `staging`, delete the feature branch. `staging` and
`main` are never deleted. Worktrees/checkouts pointing at deleted branches
are pruned by their owner.

## Public-data rule

Everything in this repository is public. Never commit host paths, private
repository names, credentials, provider/model policy, or live scheduler
identity. The `policy` CI job and the secret-scan job enforce this
mechanically (`scripts/check-public-tree.py`, gitleaks); if you think you
need to commit something private, you need a different (private) repository.

## Links

- [CONTRIBUTING.md](../CONTRIBUTING.md) — contribution rules and DCO note
- [DEVELOPMENT.md](DEVELOPMENT.md) — canonical gates
- [RELEASING.md](RELEASING.md) — release readiness; execution human-gated
- ADR-0002: [staging integration, main release](decisions/0002-staging-integration-main-release.md)
