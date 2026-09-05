# Spec: identities, observations, errors, exit codes, and output

Refs #3. Families: `hf-output/v1`, `hf-error/v1`, `hf-observation/v1`.
Fixtures: [`output/`](../../schemas/fixtures/output/output.valid.json),
[`error/`](../../schemas/fixtures/error/error.valid.json),
[`observation/`](../../schemas/fixtures/observation/observation.valid.json). Design commitment.

## 1. Normalized identities (embedded scalar formats)

Identities are typed strings, never free-form labels, and never host paths:

| Identity | Format | Example (synthetic) |
| --- | --- | --- |
| Repository identity | `owner/name` (no protocol prefix) | `example-org/widgets` |
| Plan id | `hf_plan_` + 16 hex | `hf_plan_0123456789abcdef` |
| Grant id | `gr_` + 16 hex | `gr_0123456789abcdef` |
| Evidence id | `ev_` + 16 hex | `ev_0123456789abcdef` |
| Daemon request id | 8-64 lowercase hex | `0123456789abcdef0123` |
| Idempotency key | `ik_` + 8-64 `[a-z0-9-]` | `ik_apply-20260906-0001` |
| Commit/digest | 40-hex (SHA-1) / 64-hex (SHA-256) | synthetic hex only in fixtures |
| Workflow id | lowercase slug | `fleet-doctrine-1` |
| Timestamps | RFC3339 UTC seconds + `Z` | `2026-09-06T00:00:00Z` |

## 2. Observations: `hf-observation/v1`

An observation is a normalized, timestamped statement about a subject
(`repository` | `agent` | `host`) produced by an adapter read-back, never an
untrusted claim.

```json
{"schema":"hf-observation/v1","subject":{"type":"repository","id":"example-org/widgets"},
 "observed_at":"2026-09-06T00:00:00Z","freshness":"stale","completeness":"partial",
 "payload":{"refs":{"staging":"<40hex>"}}}
```

- `freshness`: closed set `fresh` | `stale` | `unknown` — how current the
  read-back is relative to the daemon's last verified contact.
- `completeness`: closed set `complete` | `partial` — whether the read-back
  covered every requested target. Multi-repository status never hides
  partial results: a partial observation is explicit, and the CLI exit code
  reflects it.
- Anything outside the closed sets is refused (fixture
  `observation.malformed.json` uses `freshness:"fuzzy"`).

## 3. Errors: `hf-error/v1`

Typed error envelope with a stable `code` (lowercase dotted), a human
`message`, `retryable`, and optional structured `details`:

```json
{"schema":"hf-error/v1","code":"refusal.grant.expired","message":"route grant expired",
 "retryable":false,"details":null}
```

Refusal codes are part of the stable surface (e.g. `refusal.*` for typed
refusals, `stale_state`, `grant.expired`, `unknown.capability`). Adapters
map their failures into this envelope; unknown or unclassifiable failures
are refused as unknown (risk model).

## 4. Exit codes (CLI contract)

| Code | Meaning |
| --- | --- |
| 0 | ok (complete) |
| 1 | runtime/operational error (daemon unreachable, adapter failure) |
| 2 | usage error (bad arguments/precedence) |
| 3 | partial result — some targets observed, some not (never hidden) |
| 4 | refusal — stale state, expired/absent grant, unknown capability, closed issue |
| 5 | config/policy error (invalid `hf-config/v1`/`hf-policy/v1`) |
| 6-63 | reserved for typed command families (defined per command later) |
| 64+ | reserved (sysexits range respected) |

`--json` output carries the same semantics inside the envelope's
`exit_code` field; scripts should parse JSON, humans may use codes.

## 5. Human versus JSON output: `hf-output/v1`

Every stable command that can emit JSON wraps it in one envelope:

```json
{"schema":"hf-output/v1","command":"status","kind":"ok","exit_code":0,
 "data":{"repository":"example-org/widgets","freshness":"fresh"}}
```

- `kind`: closed set `ok` | `error` | `partial`.
- `ok`/`partial` carry `data` (an object); `error` carries an `error`
  object shaped like `hf-error/v1` (`code` + `message` minimum).
- Human output is a rendering of the same data; it is never a second,
  contradicting contract. Human output goes to stdout for results and
  stderr for diagnostics/progress; JSON output is the complete result on
  stdout and nothing else on stdout.
- `doctor` and read-only commands are usable with no daemon and no Herdr
  (Herdr/plugin absence never breaks the standalone CLI).

## 6. Redaction and privacy of output

One shared redaction pass at the adapter boundary strips secret-shaped text
before anything becomes a canonical record, so every downstream
serialization — output envelopes, events, journal/audit records — is redacted
by construction. Redaction is conservative: false positives cost nothing,
false negatives leak; when in doubt, redact. Fixtures contain no real
secrets and no machine paths (AC8).

## Fixtures (accept / refuse discrimination)

- accept: `output.valid.json` (ok), `output.partial.valid.json` (partial,
  exit 3), `output.error.valid.json` (error, exit 1), `error.valid.json`,
  `observation.valid.json` (stale + partial).
- refuse: unknown `kind` (`output.malformed.json`), non-boolean `retryable`
  (`error.malformed.json`), unknown `freshness` value
  (`observation.malformed.json`), and unknown-version variants of each
  family. The probe self-test additionally proves each refusal bites.
