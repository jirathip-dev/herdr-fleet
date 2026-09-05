## Summary

<!-- One or two sentences: what this PR does and why. -->

Refs #2

## Scope

<!-- Check what applies. This repository is in bootstrap: no daemon,
workflow, adapter, mutation, migration, or release behavior exists yet. -->

- [ ] Documentation
- [ ] Rust scaffold / CLI metadata
- [ ] CI / workflows / security gates
- [ ] Contributor tooling (justfile, scripts)
- [ ] Repository shape / templates
- [ ] Other: <!-- describe -->

## Checklist

- [ ] PR targets `staging` (never `main`; promotion PRs are human-only)
- [ ] No private material: no host paths, private repo names, credentials,
      provider/model policy, or live scheduler identity (public repo —
      stranger-utility test)
- [ ] `just ci` is green at this exact head (fmt-check, check, lint, test,
      doc, build-release, security) with raw exit codes recorded
- [ ] `scripts/test-check-public-tree.py` and
      `scripts/check-public-tree.py .` pass
- [ ] Docs updated where behavior/process changed; one fact authoritative,
      others link to it
- [ ] Commits use `Refs #N` wording (never `Fixes`/`Closes`/`Resolves`)

## Notes for reviewers

<!-- Independent exact-head review is the gate (verdict recorded as
evidence). External contributor PRs additionally require one human
maintainer approval. -->
