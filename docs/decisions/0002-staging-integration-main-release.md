# ADR-0002: `staging` as permanent integration branch, `main` as stable/release

- Status: accepted
- Date: 2026-09-05
- Related: issue #2 bootstrap contract; ADR-0001

## Context

A public repository with one default branch cannot simultaneously be a calm
release surface and an active integration surface. A future Herdr
marketplace index (or any consumer) reading the default branch must never
see unreleased integration state. The bootstrap also needs a place where
agent-generated PRs can land behind CI without touching the stable branch.

## Decision

- `main` remains the default, stable, public/release branch. Its head is
  only ever advanced by human promotion PRs from `staging` (plus the narrow
  `hotfix/*` incident exception documented in [WORKFLOW.md](../WORKFLOW.md)).
- `staging` is a **permanent integration branch**, created from the exact
  current `main` head at bootstrap time. Feature/dependency PRs target
  `staging`; reviewed + green work squash-merges into it with linear
  history.
- Promotion is a dedicated, human-only `staging` → `main` PR; the CI
  `policy` job fails any other PR targeting `main`.
- Both long-lived branches reject deletion, direct pushes, and
  force/non-fast-forward updates (rulesets applied by maintainers after CI
  contexts are observed).

## Consequences

- Contributors get one obvious target (`staging`) and a stable reference
  (`main`).
- Release engineering (tags from `main`, per RELEASING.md when active) never
  races integration traffic.
- The promotion policy is enforced by CI from day one and mechanically
  (rulesets) once maintainers attach required checks.
- Incidents use the documented `hotfix/*` exception with fresh human
  approval and mandatory `main` → `staging` reconciliation.

## Links

- [WORKFLOW.md](../WORKFLOW.md) · [RELEASING.md](../RELEASING.md)
