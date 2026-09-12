# Spec: XDG TOML configuration and policy overlay

Refs #3, #80. Family: `hf-config/v1` (config), `hf-policy/v1` (overlay). Fixtures:
[`config/`](../../schemas/fixtures/config/config.valid.toml), [`policy/`](../../schemas/fixtures/policy/policy.valid.toml),
manifest rows in [`schemas/fixtures/manifest.jsonl`](../../schemas/fixtures/manifest.jsonl).
Design commitment (locked spec: "One canonical XDG TOML config plus one
explicit optional policy overlay; no implicit profile/repository merge
stack").

## Config: `hf-config/v1` (TOML)

- Location: explicit `--config` path wins; otherwise XDG default
  (`$XDG_CONFIG_HOME/canter/config.toml`; a pre-rename
  `herdr-fleet/config.toml` is still discovered when the new path holds
  none — see [compatibility.md](compatibility.md#product-rename-issue-106)).
  Values below are portable; no machine-specific default is compiled in.
- The config is **one canonical file**. There is no implicit merge stack of
  profiles/repositories/hosts. Additional input arrives only through the
  single explicit overlay (below).
- Unknown keys anywhere in the document are refused (closed v1 surface).
- Axes compose but never infer: harness, model/provider, role, skills,
  workflow, policy, repository, and execution substrate are separate
  tables/entries; none is derived from another, and no model/provider name is
  a core assumption.

### Normative fields

| Key | Type | Required | Meaning |
| --- | --- | --- | --- |
| `schema` | string | yes | `"hf-config/v1"` |
| `daemon.enabled` | bool | no | run the local daemon (defaults apply when absent) |
| `daemon.socket` | string | no | explicit socket path override; default derives from the XDG runtime dir — portable default only |
| `policy.overlay` | string | no | explicit relative/absolute path of the optional policy overlay; no implicit discovery |
| `repository.<key>` | table | no | one configured repository per key (slug); `origin` (string URL) required; `branch`, `enabled` optional |
| `harness.<key>` | table | no | adapter profile per configured harness: `kind` (string; e.g. `argv`), `executable` (name resolved via PATH, never an absolute path), `env_allow` (array of environment variable names — the explicit allowlist), and the optional `provider`/`model` binding pair: bare tokens, declared together, used by the official prompt rows that carry the pair on argv (`pi`, `jcode`). Without the binding the terminal prompt refuses (`refusal.binding.missing`) — there is no default and no substitution |
| `workflow.<key>` | table | no | pinned workflow selection: `id` + `hash` (64-hex sha256 over the canonical workflow document) |
| `role.<key>` | table | no | custom roles only, explicit and hash-pinned: `hash` (64-hex) |

Synthetic valid example: `config/config.valid.toml` (one repository
`example-org/widgets`, one harness, one workflow pin — all values fictional).

## Policy overlay: `hf-policy/v1` (TOML)

- The overlay is **explicit, optional, and constrain-only**: it can add
  restrictions on top of the canonical config and can never relax or remove
  a core key. "No implicit profile/repository merge stack" means exactly
  one overlay, named by `policy.overlay`, applied in addition to the one
  canonical file.
- Overlay content is downstream-owned policy (ADR-0001: models/providers,
  organizational role policy, allowlists live downstream); the public repo
  only specifies the shape.

### Normative fields

| Key | Type | Meaning |
| --- | --- | --- |
| `schema` | string | `"hf-policy/v1"` |
| `repositories` | [string] | repository allowlist (`owner/name`); narrows which configured repositories may be acted on |
| `production_confirmation` | string | `tty` (fresh interactive TTY confirmation required, the base rule) or `deny` (block production/destructive entirely); any other value refused — the overlay may only tighten |
| `role.<key>.hash` | string | pins/hash-locks a role that the overlay may add; 64-hex |

An overlay that constrains nothing is refused (an empty overlay is a config
error, not a no-op).

## Compatibility and refusal

- A declared `harness.<key>.provider`/`model` binding must be a bare-token
  pair: non-empty, no whitespace, no path separators. Malformed, blank, or
  half-declared values are refused at load (`config.invalid`, naming the
  `config.harness.<key>.<field>` path — issue #80). An absent binding is
  not a config error; the terminal prompt refuses instead
  (`refusal.binding.missing`, [spec-capabilities.md](spec-capabilities.md)) —
  no default and no fallback model are inferred.
- Exact version match required: `hf-config/v2` or any other version is
  refused (`REFUSE_VERSION`), as is a missing/foreign `schema`
  (`REFUSE_SCHEMA`).
- Malformed examples exercised by fixtures: unknown top-level table
  (`config.malformed.toml`), overlay value outside `tty|deny`
  (`policy.malformed.toml`).
- TOML documents carry no canonical-bytes rule; ordering is not semantic.

## Relationship to other surfaces

- Repository identity inside config entries is validated against the shared
  `owner/name` format (registry scalar table; also used by plans/grants).
- The env allowlist here is the only environment channel to subprocesses
  (trust model T5).
