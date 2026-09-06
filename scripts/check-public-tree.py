#!/usr/bin/env python3
"""check-public-tree.py — fail-closed privacy scanner for the tracked tree.

Inspects ONLY the git index (tracked files and their committed blobs), never
the working tree, .gitignore, or comments. Deterministic and stdlib-only.

Violations are reported one per line as ``RULE-ID<TAB>path`` — rule
identifiers and paths only, never matching secret values. Exit codes:

* 0 — no violations
* 1 — one or more violations
* 2 — usage error
* 3 — operational error (git missing/failed, scanner error) — never 0

Content patterns are assembled at runtime from fragments so this file's own
source text does not trip the rules it enforces.
"""

from __future__ import annotations

import re
import subprocess
import sys

# --------------------------------------------------------------------------
# Rule tables
# --------------------------------------------------------------------------

# Credential-class tracked filenames. Basename/exact checks are done on the
# tracked path (relative to the repository root). Files whose basename ends
# in ".example" are exempt from the FILENAME rules only; their content is
# still scanned.
ENV_FILE_NAMES = (".env",)
ENV_FILE_PREFIXES = (".env.",)
SECRET_SUFFIXES = (
    ".p8",
    ".p12",
    ".pfx",
    ".pem",
    ".key",
    ".ppk",
    ".mobileprovision",
)
SECRET_BASENAMES = (
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
)

# Content rules: (rule_id, compiled regex). Matching text is never printed.
_ABS_UNIX = "/" + "Users" + "/"
_ABS_UNIX_HOME = "/" + "home" + "/"
_ABS_WIN_BACK = "C:" + "\\" + "Users" + "\\"
_ABS_WIN_SLASH = "C:" + "/" + "Users" + "/"
_PEM_BEGIN = "-----BEGIN "
_PEM_ALGO = "(?:[A-Z0-9]+ )*"  # e.g. "RSA ", "ENCRYPTED ", or nothing
_PEM_END = "PRIVATE KEY-----"
_SK_PREFIX = "sk-"
_XOX_PREFIX = "xox[baprs]-"

CONTENT_RULES = (
    (
        "RULE-ABS-PATH-UNIX",
        re.compile(re.escape(_ABS_UNIX) + "|" + re.escape(_ABS_UNIX_HOME)),
    ),
    (
        "RULE-ABS-PATH-WINDOWS",
        re.compile(re.escape(_ABS_WIN_BACK) + "|" + re.escape(_ABS_WIN_SLASH)),
    ),
    (
        "RULE-PEM-PRIVATE-KEY",
        re.compile(re.escape(_PEM_BEGIN) + _PEM_ALGO + re.escape(_PEM_END)),
    ),
    ("RULE-GITHUB-TOKEN", re.compile(r"\bghp_[A-Za-z0-9]{36}\b")),
    ("RULE-AWS-ACCESS-KEY", re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
    ("RULE-SLACK-TOKEN", re.compile(r"\b" + _XOX_PREFIX + r"[A-Za-z0-9-]{10,}\b")),
    (
        "RULE-OPENAI-API-KEY",
        re.compile(r"\b" + _SK_PREFIX + r"[A-Za-z0-9]{20,}\b"),
    ),
)

# Binary extensions whose content is not text-scanned. Filename rules still
# apply to them. Everything else in the tracked tree is decoded as latin-1
# (byte-preserving, cannot raise) and scanned.
BINARY_SUFFIXES = (
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".webp",
    ".ico",
    ".icns",
    ".pdf",
    ".woff",
    ".woff2",
    ".ttf",
    ".otf",
    ".zip",
    ".gz",
)

_USAGE = "usage: check-public-tree.py [repo-root]  (default repo-root: .)"


def _run_git(root: str, *args: str) -> bytes:
    """Run git inside `root`; raise RuntimeError on failure (fail loud)."""
    try:
        proc = subprocess.run(
            ["git", "-C", root, *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
    except FileNotFoundError as exc:  # git binary itself missing
        raise RuntimeError("git executable not found on PATH") from exc
    if proc.returncode != 0:
        raise RuntimeError(
            "git {} failed with exit {}: {}".format(
                " ".join(args), proc.returncode, proc.stderr.decode("utf-8", "replace").strip()
            )
        )
    return proc.stdout


def _is_binary(relpath: str) -> bool:
    lowered = relpath.lower()
    return any(lowered.endswith(sfx) for sfx in BINARY_SUFFIXES)


def _check_filename(relpath: str) -> str | None:
    """Return a rule id for a credential-class tracked filename, or None."""
    basename = relpath.rsplit("/", 1)[-1]
    if basename.endswith(".example"):
        return None
    if basename in ENV_FILE_NAMES:
        return "RULE-CRED-FILENAME-ENV"
    if any(basename.startswith(pfx) for pfx in ENV_FILE_PREFIXES):
        return "RULE-CRED-FILENAME-ENV"
    lowered = basename.lower()
    if any(lowered.endswith(sfx) for sfx in SECRET_SUFFIXES):
        return "RULE-CRED-FILENAME-KEY"
    if basename in SECRET_BASENAMES:
        return "RULE-CRED-FILENAME-KEY"
    return None


def _check_content(relpath: str, blob: bytes) -> list[str]:
    if _is_binary(relpath):
        return []
    # latin-1 decodes every byte without raising; all patterns are ASCII, so
    # scanning latin-1 text is equivalent to scanning the raw bytes.
    text = blob.decode("latin-1")
    return [rule_id for rule_id, pattern in CONTENT_RULES if pattern.search(text)]


def scan_tracked_tree(root: str) -> tuple[list[tuple[str, str]], int]:
    """Scan the git index at `root`.

    Returns (violations, status) where status is 0, 1, or 3. Violations are
    sorted (rule_id, path) pairs.
    """
    # Fail loudly on git errors; never treat empty output as clean.
    top = _run_git(root, "rev-parse", "--show-toplevel").decode("utf-8", "replace").strip()
    if not top:
        raise RuntimeError("git rev-parse --show-toplevel returned nothing")

    listing = _run_git(root, "ls-files", "-s", "-z")
    if not listing:
        raise RuntimeError(
            "git ls-files returned an empty tracked tree; refusing to treat empty as clean"
        )

    violations: list[tuple[str, str]] = []
    for entry in listing.split(b"\0"):
        if not entry:
            continue
        try:
            _meta, _tab, relpath_bytes = entry.partition(b"\t")
            # meta is "<mode> <object> <stage>" (mode and object are sha-safe)
            _mode, blob_sha, _stage = _meta.decode("ascii").split(" ", 2)
        except (UnicodeDecodeError, ValueError) as exc:
            raise RuntimeError("unparseable git ls-files entry: {!r}".format(entry)) from exc
        relpath = relpath_bytes.decode("utf-8", "replace")

        filename_rule = _check_filename(relpath)
        if filename_rule is not None:
            violations.append((filename_rule, relpath))

        blob = _run_git(root, "cat-file", "blob", blob_sha)
        for rule_id in _check_content(relpath, blob):
            violations.append((rule_id, relpath))

    violations.sort()
    return violations, 1 if violations else 0


def main(argv: list[str]) -> int:
    if len(argv) > 2:
        print(_USAGE, file=sys.stderr)
        return 2
    root = argv[1] if len(argv) == 2 else "."

    try:
        violations, status = scan_tracked_tree(root)
    except RuntimeError as exc:
        print("check-public-tree: ERROR: {}".format(exc), file=sys.stderr)
        return 3

    for rule_id, relpath in violations:
        print("{}\t{}".format(rule_id, relpath))
    if status != 0:
        print(
            "check-public-tree: {} violation(s) found in the tracked tree".format(
                len(violations)
            ),
            file=sys.stderr,
        )
    return status


if __name__ == "__main__":
    sys.exit(main(sys.argv))
