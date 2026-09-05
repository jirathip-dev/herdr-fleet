# Architecture artifacts — locked target (v5)

This directory commits the approved **locked target architecture** for
herdr-fleet (issue #1's final architecture and delivery graph, Amendment 3 of
issue #2). It is the *target* model — **not** the current implementation.
The current bootstrap ships only the repository foundation described in
[`../ARCHITECTURE.md`](../ARCHITECTURE.md).

## Files

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

## Regeneration

The HTML is rendered from the committed JSON by Archify (showcase quality
profile, animation none). Static previews are re-rendered from the **local
sanitized HTML** with headless Chrome:

```
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
antialiasing-level raster noise (geometry and content are identical). If
dark byte-reproducibility is ever required, the exact renderer binary must
be pinned here.

## SHA-256 (of the committed files)

| File | SHA-256 |
| --- | --- |
| `herdr-fleet.locked-target.architecture.json` | `b41abe52315be38c815c6a2b49f2f55748adfefb56162a994aa68b92e52c5d0d` |
| `herdr-fleet.locked-target.architecture.html` | `04c2e8310dd58c7c62a6de51a41443fc10156b30f700b61ad7c73b77a0175d54` |
| `herdr-fleet.locked-target.architecture.preview.light.png` | `aaae221a2d9c17ae88a99293ea7c95606895e9592f3c64ce7b62113c0cb78da4` |
| `herdr-fleet.locked-target.architecture.preview.dark.png` | `dbe500272b95e8750166a08e9bfbcac643eb9bfbed743d89d0ab38e36388165a` |

## Original validation summary

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
