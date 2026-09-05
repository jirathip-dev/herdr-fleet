# Releasing

**NOT YET ACTIVE.** This document describes the *future* release contract
for herdr-fleet. There is **no** release workflow, no tag, no publishing
command, and nothing runnable in this repository today (bootstrap scope —
see the umbrella issue). Do not treat any command here as executable now.

## Future contract (when releases are approved)

When a release slice is routed and implemented, releases must follow:

- **Source**: `main` only, after human promotion from `staging` and a
  human-approved release issue. GitHub Releases is the canonical
  distribution channel; the Rust library stays unpublished to crates.io
  through 1.0.
- **Artifacts**: platform-correct archives for macOS and Linux (per
  architecture), each with an **adjacent checksum file**; SBOM, Sigstore
  provenance, and completions/manpages land with the release slice that
  implements them.
- **No self-update**: shipped binaries never phone home or update
  themselves (a source-checkout must not depend on any mutable branch
  state).
- **Versioning**: promotion of a release tag happens from `main`; 0.x
  milestones are allowed after human promotion; the 1.0 process (attested
  `v1.0.0-rc.N`, real-system soak, then `v1.0.0` on the soaked commit) is
  defined in the umbrella's locked spec.
- **Checksum policy**: every archive ships with an adjacent
  `SHA256SUMS`-style file; publishing pipelines verify checksums before
  upload and never upload unverified artifacts.

Nothing above is implemented or scheduled in this bootstrap.
