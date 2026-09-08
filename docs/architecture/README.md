# Architecture artifacts

This directory commits two architecture artifact sets for herdr-fleet:

- **As-shipped (v0.1.0)** — what the current release ships today (first
  section below; the picture embedded at the top of the repository README).
- **Locked target (v5)** — the approved target architecture (issue #1's final
  architecture and delivery graph, Amendment 3 of issue #2), committed for
  reference. It is the *target* model — **not** the current implementation.

## As-shipped architecture (v0.1.0)

Renders the v0.1.0 shipped surface: operator → read-only CLI → local daemon →
workflow engine → typed adapters → Git/GitHub and agent harnesses, with the
plans/grants SQLite state below the engine and Herdr observed (never
controlled) from the CLI. It is deliberately distinct from the locked-target
set below: it shows implemented v0.1.0 flows only — no future adapters, no
optional read-only Corral edge, no workspace/pane edges into Herdr.

## Files

| File | What it is |
| --- | --- |
| `herdr-fleet.as-shipped.architecture.json` | Architecture source (Archify schema v1, showcase quality profile). |
| `herdr-fleet.as-shipped.architecture.html` | Self-contained interactive HTML artifact. |
| `herdr-fleet.as-shipped.architecture.preview.light.png` | Static 1440×900 preview, light theme (embedded in the README; light-safe on GitHub dark mode). |
| `herdr-fleet.as-shipped.architecture.preview.dark.png` | Static 1440×900 preview, dark theme. |

## Provenance (honest-renderer note, per the #12 lesson)

- **JSON**: authored to the same Archify architecture schema v1 as the locked
  target (component/boundary/connection objects, showcase profile). No archify
  CLI was available on this host, so this JSON was **not** validated by an
  archify binary here; it is the source the HTML diagram body was generated
  from.
- **HTML**: the archify CLI was not available on this host, so the artifact
  reuses the committed locked-target Archify **export shell** — the same
  self-contained runtime, pre-paint theme resolver, toolbar, and CSS
  (generator `archify 2.17.0-dev.1`) — with the diagram body (SVG) authored in
  the export's own markup conventions from the committed JSON. The shell's
  rendered chrome was **re-authored to the as-shipped identity as well**: the
  page `<h1>` reads "herdr-fleet — As-Shipped Architecture (v0.1.0)" and the
  footer info card ("Shipped v0.1.0") carries as-shipped prose and the
  as-shipped arrow legend — no locked-target title, "Target contract" card, or
  locked-target arrow legend remains anywhere in the file. The
  `<meta name="generator">` line states the shell reuse exactly; no archify
  regeneration was performed.
- **PNG previews**: rendered from the local HTML (`file://`) with
  chrome-headless-shell (reports itself as Google Chrome for Testing
  151.0.7922.34), window 1440×900, `--force-device-scale-factor=1`, using the
  locked-target regeneration commands with `?theme=light` / `?theme=dark`,
  plus `--virtual-time-budget=8000 --run-all-compositor-stages-before-draw` so
  the artifact's async JetBrains Mono webfont load settles before capture.
- **Reproducibility**: on this host both previews re-render **byte-identically**
  (verified twice, raster-compared) with the commands below. Byte-reproducibility
  on a different renderer binary is not guaranteed — as with the locked-target
  dark preview note, the exact renderer binary must be pinned here if
  byte-reproducibility is ever required.

## Regeneration

```text
# from this directory, after committing the HTML:
chrome-headless-shell --headless --disable-gpu --hide-scrollbars \
  --force-device-scale-factor=1 --window-size=1440,900 \
  --default-background-color=FFFFFFFF --virtual-time-budget=8000 \
  --run-all-compositor-stages-before-draw \
  --screenshot=<out>.light.png "file://<abs path to html>?theme=light"
chrome-headless-shell --headless --disable-gpu --hide-scrollbars \
  --force-device-scale-factor=1 --window-size=1440,900 \
  --virtual-time-budget=8000 --run-all-compositor-stages-before-draw \
  --screenshot=<out>.dark.png "file://<abs path to html>?theme=dark"
```

(`?theme=` is honored by the artifact's own pre-paint theme resolver; the
file's default theme is dark.)

## SHA-256 (of the committed files)

| File | SHA-256 |
| --- | --- |
| `herdr-fleet.as-shipped.architecture.json` | `2ed3feb919d1fcd5b38f050089157d6b82e58e9434520fdabd9eb0624e665a48` |
| `herdr-fleet.as-shipped.architecture.html` | `4320f36196c6ea9e18f12552f9c3d78d471fba922c2eec2de02b3e1799f75011` |
| `herdr-fleet.as-shipped.architecture.preview.light.png` | `8083da1b6117635bb0addbc2950cee5967b34e8c25e7610288c0fcc4fd95e9c2` |
| `herdr-fleet.as-shipped.architecture.preview.dark.png` | `15db4361ee5ac8da80835088f7c6912a2a2301e4a95c3d28da7e824d96d9ea84` |

## Locked target architecture (v5)

The approved **locked target architecture** for herdr-fleet (issue #1's final
architecture and delivery graph, Amendment 3 of issue #2). It is the *target*
model — **not** the current implementation; the current v0.1.0 shipped surface
has its own committed artifact set (above) and is described in the repository
README. [`../ARCHITECTURE.md`](../ARCHITECTURE.md) retains the bootstrap-era
scaffold description and the approved target model.

## Files (locked target)

| File | What it is |
| --- | --- |
| `herdr-fleet.locked-target.architecture.json` | Archify architecture source (schema v1, showcase quality profile). |
| `herdr-fleet.locked-target.architecture.html` | Self-contained interactive HTML artifact rendered from the JSON. |
| `herdr-fleet.locked-target.architecture.preview.light.png` | Static 1440×900 preview, light theme (GitHub-readable). |
| `herdr-fleet.locked-target.architecture.preview.dark.png` | Static 1440×900 preview, dark theme (GitHub-readable). |

## Sanitization note (deviation from the maintainer's validated v5 artifact)

The approved v5 artifact carried, on the **Private Policy Overlay** component,
a sublabel of the form `Optional · <name of a private repository> example`.
Public committed artifacts must not contain private repository identifiers,
so that trailing name was replaced with `downstream example` (shorter, same
label class) in **every** occurrence:

- the JSON `sublabel` field of the `privatePolicy` component, and
- all four HTML occurrences (`aria-label`, `data-node-sublabel`, `<title>`,
  and the visible `<text>`).

The component now reads "Private Policy Overlay — Optional · downstream
example". No other content, geometry, or coordinates were altered. The
original unmodified artifact set remains in the maintainer's private artifact
store, which is not part of this repository.

## Regeneration (locked target)

The HTML is rendered from the committed JSON by Archify (showcase quality
profile, animation none). Static previews are re-rendered from the **local
sanitized HTML** with headless Chrome:

```text
chrome-headless-shell --headless --disable-gpu --hide-scrollbars \
  --force-device-scale-factor=1 --window-size=1440,900 \
  --default-background-color=FFFFFFFF \
  --screenshot=<out>.light.png "file://<abs path to html>?theme=light"
chrome-headless-shell --headless --disable-gpu --hide-scrollbars \
  --force-device-scale-factor=1 --window-size=1440,900 \
  --screenshot=<out>.dark.png "file://<abs path to html>?theme=dark"
```

(`?theme=` is honored by the artifact's own pre-paint theme resolver; the
file's default theme is dark.)

The light preview re-renders byte-exactly. The dark preview is not
byte-reproducible on re-render with the current local Chromium /
chrome-headless-shell: re-renders differ from the committed PNG only by
antialiasing-level raster noise (geometry and content are identical). If dark
byte-reproducibility is ever required, the exact renderer binary must be
pinned here.

## SHA-256 (of the committed locked-target files)

| File | SHA-256 |
| --- | --- |
| `herdr-fleet.locked-target.architecture.json` | `b41abe52315be38c815c6a2b49f2f55748adfefb56162a994aa68b92e52c5d0d` |
| `herdr-fleet.locked-target.architecture.html` | `04c2e8310dd58c7c62a6de51a41443fc10156b30f700b61ad7c73b77a0175d54` |
| `herdr-fleet.locked-target.architecture.preview.light.png` | `aaae221a2d9c17ae88a99293ea7c95606895e9592f3c64ce7b62113c0cb78da4` |
| `herdr-fleet.locked-target.architecture.preview.dark.png` | `dbe500272b95e8750166a08e9bfbcac643eb9bfbed743d89d0ab38e36388165a` |

## Original validation summary (locked target)

The maintainer validated the pre-sanitization artifact set under Archify's
showcase quality profile before this lane committed it:

- schema validation exit 0 (showcase profile, 0 errors / 0 warnings);
- visual check exit 0 at 1440×900, 1600×1000, 1920×1080, and 2048×1320 with
  no horizontal overflow;
- generation receipts and validation receipts are retained in the
  maintainer's private artifact store (not committed here: they contain
  absolute host paths).

The only committed change versus the validated set is the sanitization
described above; a same-or-shorter label cannot introduce overflow, and the
committed previews re-rendered from the sanitized HTML confirm 1440×900
renders with no overflow.

## Privacy gate

The repository's public-tree scanner (`scripts/check-public-tree.py`) and the
CI policy documentation-link check cover this directory. Before commit, a
manual sweep confirmed zero matches for private path, host, repository, or
scheduler identifiers in these files.
