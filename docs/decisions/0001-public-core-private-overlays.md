# ADR-0001: Public reusable core versus private downstream overlays

- Status: accepted
- Date: 2026-09-05
- Related: umbrella issue #1 architecture decision; ADR-0003

## Context

herdr-fleet is a public, community project aimed at the stranger-utility
test: every byte committed must be useful and safe for a stranger. The
operating environment for the real fleet (which repositories to touch, which
models/providers/roles to use, credentials, host paths, schedules, incident
policy) is private by nature. If the two are mixed, the public project
either leaks private material or becomes unusable as a reference.

## Decision

Split the system along a **public core / private overlay** boundary:

- **Public core** (this repository): portable behavior, typed plans and
  domain types, default workflows, framework-neutral doctrine, public docs,
  CI and security gates. Everything here is reusable by anyone.
- **Private downstream overlays**: deployment policy, model/provider and
  profile choices, credentials, private routing, repository mappings, host
  paths, schedules, and incident policy. These live in private
  repositories/configuration and are never required by the public core.

Configuration composes the two: one canonical XDG TOML config plus one
explicit optional policy overlay. The overlay is always downstream-owned and
its absence can never break standalone CLI help, configuration validation,
planning, or supported non-dependent operations.

## Consequences

- Public code must not embed downstream assumptions: no model/provider names
  in core domain types, no required profile layout, no host paths, no
  private repository identifiers.
- Enforcement is mechanical, not aspirational: the `policy` CI job and the
  secret-scan job (git-index filename/content parser + gitleaks) run on
  every PR; the scanner has discriminating self-tests.
- The committed locked-target diagram's "Private Policy Overlay" is labeled
  with a neutral example name for the same reason.
- Downstream consumers get a stable, clean core to overlay their policy on.

## Links

- [ARCHITECTURE.md](../ARCHITECTURE.md) · [WORKFLOW.md](../WORKFLOW.md)
