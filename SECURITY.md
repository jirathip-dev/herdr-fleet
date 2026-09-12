# Security Policy

## Supported versions

canter is **pre-alpha** (no tagged releases yet). Until the first
release exists, only the current head of `staging` and `main` receive
security fixes. Once releases begin (per [docs/RELEASING.md](docs/RELEASING.md)),
this section will list supported release lines; until then, security fixes
land on `staging` and are promoted per the workflow.

## Reporting a vulnerability

**Do not open a public issue for a vulnerability.** Report it privately
through GitHub's private advisory flow:

- https://github.com/jirathip-dev/canter/security/advisories/new

Private advisories let maintainers triage and fix before public disclosure.
What we ask in a report:

- repository (this one), affected commit/version if known;
- a public-safe description (no credentials or private configuration);
- reproduction steps, impact, and any suggested fix (optional).

Maintainers aim to acknowledge reports within 5 business days and to keep
the reporter informed of progress. If the report is not a vulnerability
(e.g. a regular bug), please open a normal issue instead — without secrets.

## No secrets in issues

This repository is public. Never include credentials, tokens, private host
paths, private repository names, or provider/model policy in issues, PRs,
or comments. The CI `policy` and `secret-scan` jobs mechanically reject
credential-class files and secret-shaped content from the tracked tree.
