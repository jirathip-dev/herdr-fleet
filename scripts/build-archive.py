#!/usr/bin/env python3
"""build-archive.py — deterministic release archive builder and verifier.

Issue #10 (release readiness). From an EXACT recorded source ref + version,
builds a platform archive with the documented layout, an SHA-256 checksums
manifest, an SBOM derived from Cargo.lock (offline, no network), and a
provenance record binding source ref / binary-reported schema facts /
per-file checksums / SBOM digest. The lane tests this end-to-end on the
current host; release-time execution (tagging, uploading, attestation) is a
separate human gate documented in docs/RELEASING.md.

Stdlib only; runs on the maintainer's macOS/Linux release host with Python 3.

Exit codes: 0 ok, 1 operational/build error, 2 usage error.

Determinism scope (documented honestly, do not over-claim):
* The metadata layer (SBOM, SHA256SUMS, provenance.json and the archive
  framing: fixed member mtimes, fixed gzip header time, uid/gid 0) is
  byte-deterministic for the same source ref + version + input binary.
* The bundled binary's bytes depend on the exact toolchain/build host, so
  cross-toolchain archive reproducibility is NOT claimed; verification maps
  the artifact back to the provenance record instead (checksums + binary
  self-reported version/schema facts).
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

# The four release-blocking platforms (docs/RELEASING.md).
PLATFORMS = ("linux-x86_64", "linux-aarch64", "darwin-x86_64", "darwin-aarch64")

_ARCHIVE_RE = re.compile(r"^canter-(?P<version>[0-9]+[0-9a-zA-Z.\-]*)"
                         r"-(?P<platform>" + "|".join(PLATFORMS) + r")\.tar\.gz$")
_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
_COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
_VERSION_LINE_RE = re.compile(r"^canter (?P<version>[0-9][0-9a-zA-Z.\-]*)",
                              re.MULTILINE)
_SCHEMA_LINE_RE = re.compile(r"^state schema version: (?P<version>[0-9]+)$",
                             re.MULTILINE)
_MIGRATION_LINE_RE = re.compile(r"^migration chain: (?P<chain>.*)$",
                                re.MULTILINE)
_FAMILIES_LINE_RE = re.compile(r"^document schema families: (?P<families>.*)$",
                               re.MULTILINE)

# The pre-rename product-name alias (docs/contracts/compatibility.md,
# "Product rename (issue #106)"): every archive ships it next to `canter`.
LEGACY_ALIAS = "herdr-fleet"

ARCHIVE_MEMBERS = (
    "canter",
    LEGACY_ALIAS,
    "LICENSE-APACHE",
    "LICENSE-MIT",
    "SBOM.spdx.json",
    "SHA256SUMS",
    "provenance.json",
)


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run_checked(argv: list[str], repo: str) -> str:
    """Run one command in `repo`; return trimmed stdout or die loudly."""
    proc = subprocess.run(
        argv, cwd=repo, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        check=False,
    )
    if proc.returncode != 0:
        sys.stderr.write(
            f"error: command failed (exit {proc.returncode}): "
            + " ".join(argv)
            + "\n"
            + proc.stderr.decode("utf-8", "replace")
        )
        sys.exit(1)
    return proc.stdout.decode("utf-8", "replace").strip()


def parse_lockfile_packages(lock_text: str) -> list[dict[str, str]]:
    """Parse `[[package]]` blocks from Cargo.lock (stdlib, offline)."""
    packages: list[dict[str, str]] = []
    current: dict[str, str] = {}
    in_package = False
    for raw in lock_text.splitlines():
        line = raw.rstrip()
        if line.startswith("[[package]]"):
            in_package = True
            current = {}
        elif in_package and line.startswith("["):
            in_package = False
        elif in_package and "=" in line:
            key, _, value = line.partition("=")
            current[key.strip()] = value.strip().strip('"')
        elif in_package and line == "":
            if current.get("name"):
                packages.append(current)
            in_package = False
    if in_package and current.get("name"):
        packages.append(current)
    return packages


def make_sbom(packages: list[dict[str, str]], version: str, source_ref: str,
              created: str) -> dict:
    """SPDX 2.3 JSON SBOM derived from Cargo.lock package entries."""
    root_name = "canter"
    doc = {
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": f"canter-{version}-sbom",
        "documentNamespace": (
            "https://spdx.org/spdxdocs/canter/"
            f"{version}/{source_ref}"
        ),
        "creationInfo": {
            "created": created,
            "creators": ["Tool: scripts/build-archive.py (canter)"],
        },
        "packages": [],
        "relationships": [],
    }
    pkg_ids: dict[str, str] = {}
    # The root package is the release itself (not on crates.io; publish=false).
    doc["packages"].append({
        "name": root_name,
        "SPDXID": "SPDXRef-Package-canter",
        "versionInfo": version,
        "downloadLocation": "NOASSERTION",
        "filesAnalyzed": False,
        "licenseConcluded": "NOASSERTION",
        "licenseDeclared": "Apache-2.0 OR MIT",
        "copyrightText": "NOASSERTION",
        "externalRefs": [{
            "referenceCategory": "PACKAGE-MANAGER",
            "referenceType": "purl",
            "referenceLocator": f"pkg:cargo/{root_name}@{version}",
        }],
    })
    pkg_ids[root_name] = "SPDXRef-Package-canter"
    for package in packages:
        name = package.get("name", "")
        version_info = package.get("version", "")
        if not name or (name == root_name and version_info == version):
            continue
        slug = re.sub(r"[^A-Za-z0-9.\-]", "-", name)
        spdx_id = f"SPDXRef-Package-{slug}"
        if spdx_id in pkg_ids.values():
            spdx_id = f"{spdx_id}-{version_info}"
        entry = {
            "name": name,
            "SPDXID": spdx_id,
            "versionInfo": version_info,
            "downloadLocation": "NOASSERTION",
            "filesAnalyzed": False,
            "licenseConcluded": "NOASSERTION",
            "licenseDeclared": "NOASSERTION",
            "copyrightText": "NOASSERTION",
            "externalRefs": [{
                "referenceCategory": "PACKAGE-MANAGER",
                "referenceType": "purl",
                "referenceLocator": f"pkg:cargo/{name}@{version_info}",
            }],
        }
        # Cargo.lock `checksum` is the sha256 of the published crate bytes.
        checksum = package.get("checksum", "")
        if _SHA256_RE.match(checksum):
            entry["checksums"] = [{
                "algorithm": "SHA256",
                "checksumValue": checksum,
            }]
        doc["packages"].append(entry)
        pkg_ids[name] = spdx_id
        doc["relationships"].append({
            "spdxElementId": "SPDXRef-DOCUMENT",
            "relationshipType": "DESCRIBES",
            "relatedSpdxElement": spdx_id,
        })
    return doc


def binary_version_facts(binary: str) -> dict:
    """Ask the release binary for its own version + schema facts.

    Reads `--version` output; the schema-fact lines were added by issue #10
    exactly so a provenance chain can bind binary-reported facts without
    parsing Rust sources or trusting caller-supplied values.
    """
    proc = subprocess.run(
        [binary, "--version"], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        check=False,
    )
    if proc.returncode != 0:
        sys.stderr.write(
            f"error: release binary {binary} failed `--version` "
            f"(exit {proc.returncode}): "
            + proc.stderr.decode("utf-8", "replace")
        )
        sys.exit(1)
    text = proc.stdout.decode("utf-8", "replace")
    match = _VERSION_LINE_RE.search(text)
    schema = _SCHEMA_LINE_RE.search(text)
    chain = _MIGRATION_LINE_RE.search(text)
    families = _FAMILIES_LINE_RE.search(text)
    missing = [name for name, found in (
        ("version line", match),
        ("state schema version line", schema),
        ("migration chain line", chain),
        ("document schema families line", families),
    ) if not found]
    if missing:
        sys.stderr.write(
            "error: release binary --version output is missing: "
            + ", ".join(missing)
            + "\n--- output ---\n" + text
        )
        sys.exit(1)
    assert match is not None and schema is not None
    assert chain is not None and families is not None
    return {
        "product_version": match.group("version"),
        "state_schema_version": int(schema.group("version")),
        "migration_chain": [
            item.strip()
            for item in chain.group("chain").split(",")
            if item.strip()
        ],
        "document_schema_families": [
            item.strip()
            for item in families.group("families").split(",")
            if item.strip()
        ],
    }


def canonical_json_bytes(value: dict) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":"))
            + "\n").encode("utf-8")


def write_canonical_json(path: str, value: dict) -> None:
    with open(path, "wb") as handle:
        handle.write(canonical_json_bytes(value))


def cmd_build(args: argparse.Namespace) -> int:
    repo = os.path.abspath(args.repo)
    if not os.path.isfile(os.path.join(repo, "Cargo.lock")):
        sys.stderr.write(f"error: {repo} does not look like the canter repo "
                         "(no Cargo.lock)\n")
        return 1
    if not _COMMIT_RE.match(args.source_ref):
        sys.stderr.write("error: --source-ref must be a full 40-hex commit sha\n")
        return 2
    head = run_checked(["git", "rev-parse", "HEAD"], repo)
    if head != args.source_ref:
        sys.stderr.write(
            f"error: repo HEAD is {head} but --source-ref is {args.source_ref}; "
            "archives must be built from the EXACT recorded source commit\n"
        )
        return 1
    dirty = run_checked(
        ["git", "status", "--porcelain"], repo).splitlines()
    tracked_dirty = [line for line in dirty if not line.startswith("??")]
    if tracked_dirty:
        sys.stderr.write(
            "error: tracked working tree is dirty; build from a clean checkout "
            "of the recorded ref:\n" + "\n".join(tracked_dirty) + "\n"
        )
        return 1

    binary = os.path.abspath(args.binary)
    if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        sys.stderr.write(f"error: --binary {binary} is not an executable file "
                         "(run `cargo build --release --locked` first)\n")
        return 1
    # The pre-rename alias must sit next to the canonical binary: it is a
    # shipped member of every archive (docs/contracts/compatibility.md).
    alias = os.path.join(os.path.dirname(binary), LEGACY_ALIAS)
    if not os.path.isfile(alias) or not os.access(alias, os.X_OK):
        sys.stderr.write(
            f"error: the pre-rename alias {alias} is not an executable file; "
            "`cargo build --release --locked` must produce it next to "
            "--binary and every archive must ship it "
            "(docs/contracts/compatibility.md)\n")
        return 1
    if args.platform not in PLATFORMS:
        sys.stderr.write("error: --platform must be one of: "
                         + ", ".join(PLATFORMS) + "\n")
        return 2

    facts = binary_version_facts(binary)
    if facts["product_version"] != args.version:
        sys.stderr.write(
            f"error: binary reports version {facts['product_version']} but "
            f"--version {args.version} was requested; the archive must map to "
            "the binary it actually ships\n"
        )
        return 1

    committed_at = run_checked(
        ["git", "show", "-s", "--format=%cI", args.source_ref], repo)
    commit_ts = int(run_checked(
        ["git", "show", "-s", "--format=%ct", args.source_ref], repo))

    lock_path = os.path.join(repo, "Cargo.lock")
    with open(lock_path, "r", encoding="utf-8") as handle:
        packages = parse_lockfile_packages(handle.read())
    sbom = make_sbom(packages, args.version, args.source_ref, committed_at)

    stem = f"canter-{args.version}-{args.platform}"
    inner = f"canter-{args.version}"
    out_dir = os.path.abspath(args.out_dir)
    os.makedirs(out_dir, exist_ok=True)

    staging = tempfile.mkdtemp(prefix="hf-archive-", dir=out_dir)
    try:
        inner_dir = os.path.join(staging, inner)
        os.makedirs(inner_dir, mode=0o755, exist_ok=True)
        for name, source in (("canter", binary), (LEGACY_ALIAS, alias)):
            shutil.copy2(source, os.path.join(inner_dir, name))
            os.chmod(os.path.join(inner_dir, name), 0o755)
        for license_name in ("LICENSE-APACHE", "LICENSE-MIT"):
            shutil.copy2(os.path.join(repo, license_name),
                         os.path.join(inner_dir, license_name))
        sbom_bytes = canonical_json_bytes(sbom)
        with open(os.path.join(inner_dir, "SBOM.spdx.json"), "wb") as handle:
            handle.write(sbom_bytes)

        # Per-file checksums (deterministic content manifest).
        manifest: dict[str, str] = {}
        for member in ("canter", LEGACY_ALIAS, "LICENSE-APACHE",
                       "LICENSE-MIT", "SBOM.spdx.json"):
            manifest[member] = sha256_file(
                os.path.join(inner_dir, member))
        sha_lines = "".join(
            f"{manifest[name]}  {name}\n"
            for name in sorted(manifest)
        )
        with open(os.path.join(inner_dir, "SHA256SUMS"), "w",
                  encoding="utf-8") as handle:
            handle.write(sha_lines)
        manifest["SHA256SUMS"] = sha256_bytes(sha_lines.encode("utf-8"))

        provenance = {
            "record": "release-provenance/v1",
            "product": "canter",
            "version": args.version,
            "platform": args.platform,
            "source": {"ref": args.source_ref, "committed_at": committed_at},
            "schema_facts": {
                "state_schema_version": facts["state_schema_version"],
                "migration_chain": facts["migration_chain"],
                "document_schema_families": facts["document_schema_families"],
            },
            "files": {
                name: digest for name, digest in sorted(manifest.items())
            },
            "sbom": {
                "file": "SBOM.spdx.json",
                "sha256": manifest["SBOM.spdx.json"],
            },
        }
        write_canonical_json(
            os.path.join(inner_dir, "provenance.json"), provenance)

        archive_path = os.path.join(out_dir, stem + ".tar.gz")
        # Deterministic framing: fixed member mtime (source commit time),
        # uid/gid 0, empty names, and a fixed gzip header time.
        with open(archive_path, "wb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw,
                               mtime=commit_ts, compresslevel=9) as gz:
                with tarfile.open(fileobj=gz, mode="w",
                                  format=tarfile.GNU_FORMAT) as tar:
                    for name in sorted(os.listdir(inner_dir)):
                        member_path = os.path.join(inner_dir, name)
                        info = tarfile.TarInfo(os.path.join(inner, name))
                        info.size = os.path.getsize(member_path)
                        info.mtime = commit_ts
                        info.mode = (0o755 if name in ("canter", LEGACY_ALIAS)
                                     else 0o644)
                        info.uid = 0
                        info.gid = 0
                        info.uname = ""
                        info.gname = ""
                        with open(member_path, "rb") as source:
                            tar.addfile(info, source)
        archive_sha = sha256_file(archive_path)
        with open(archive_path + ".sha256", "w", encoding="utf-8") as handle:
            handle.write(f"{archive_sha}  {stem}.tar.gz\n")
        print(f"archive: {archive_path}")
        print(f"archive sha256: {archive_sha}")
        print(f"checksums: {os.path.join(out_dir, stem + '.tar.gz.sha256')}")
        print(f"provenance record: release-provenance/v1 (source {args.source_ref}, "
              f"state schema v{facts['state_schema_version']})")
        return 0
    finally:
        shutil.rmtree(staging, ignore_errors=True)


def parse_provenance(inner_dir: str) -> dict:
    with open(os.path.join(inner_dir, "provenance.json"), "r",
              encoding="utf-8") as handle:
        return json.load(handle)


def cmd_verify(args: argparse.Namespace) -> int:
    archive_path = os.path.abspath(args.archive)
    if not os.path.isfile(archive_path):
        sys.stderr.write(f"error: archive not found: {archive_path}\n")
        return 1
    match = _ARCHIVE_RE.match(os.path.basename(archive_path))
    if not match:
        sys.stderr.write(
            f"error: {os.path.basename(archive_path)} does not match "
            "canter-<version>-<platform>.tar.gz\n")
        return 2
    stem = match.group(0)[:-len(".tar.gz")]

    checks = 0

    def ok(message: str) -> None:
        nonlocal checks
        checks += 1
        print(f"OK: {message}")

    def fail(message: str) -> int:
        sys.stderr.write(f"FAIL: {message}\n")
        return 1

    archive_sha = sha256_file(archive_path)
    sha_path = archive_path + ".sha256"
    if os.path.isfile(sha_path):
        with open(sha_path, "r", encoding="utf-8") as handle:
            expected = handle.read().split()[0]
        if expected != archive_sha:
            return fail(f"{os.path.basename(sha_path)} names {expected} but "
                        f"the archive hashes to {archive_sha}")
        ok(f"adjacent {os.path.basename(sha_path)} matches the archive sha256")

    with tarfile.open(archive_path, "r:gz") as tar:
        members = [m for m in tar.getmembers() if m.isfile()]
        names = [os.path.basename(m.name) for m in members]
        if sorted(names) != sorted(ARCHIVE_MEMBERS):
            return fail(f"archive layout mismatch; expected "
                        f"{sorted(ARCHIVE_MEMBERS)}, found {sorted(names)}")
        ok("archive contains exactly the documented members")
        inner_dir = tempfile.mkdtemp(prefix="hf-verify-")
        try:
            # Verify runs on UNTRUSTED archives (a downloaded release
            # artifact), so extraction must be sanitized. Pin the 'data'
            # filter explicitly: it rejects absolute paths, '..' traversal,
            # device nodes, and links whose target escapes the extraction
            # directory. The default is interpreter-version-dependent
            # (Python >= 3.14 defaults to 'data'; older interpreters default
            # to the permissive legacy mode), so never rely on it.
            tar.extractall(inner_dir, filter="data")
        except Exception as exc:  # tarfile errors surface as verification fails
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail(f"archive extraction failed: {exc}")
        # Locate the provenance member inside the extracted tree.
        provenance_path = None
        inner_root = None
        for root, _dirs, files in os.walk(inner_dir):
            if "provenance.json" in files:
                provenance_path = os.path.join(root, "provenance.json")
                inner_root = root
                break
        if provenance_path is None or inner_root is None:
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail("provenance.json missing after extraction")
        assert provenance_path is not None and inner_root is not None

        with open(provenance_path, "r", encoding="utf-8") as handle:
            provenance = json.load(handle)
        if provenance.get("record") != "release-provenance/v1":
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail("provenance record marker is not release-provenance/v1")
        ok("provenance record parses with the release-provenance/v1 marker")

        # Per-file manifest: SHA256SUMS == provenance.files == disk bytes.
        # SHA256SUMS lists the payload files (not itself); provenance.files
        # additionally carries the SHA256SUMS content digest.
        with open(os.path.join(inner_root, "SHA256SUMS"), "r",
                  encoding="utf-8") as handle:
            sha_lines = handle.read()
        from_manifest: dict[str, str] = {}
        for line in sha_lines.splitlines():
            digest, _, name = line.partition("  ")
            from_manifest[name] = digest
        expected_payload = sorted(
            name for name in provenance.get("files", {}) if name != "SHA256SUMS")
        if sorted(from_manifest) != expected_payload:
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail("SHA256SUMS and provenance.files name different files")
        for name, digest in from_manifest.items():
            on_disk = sha256_file(os.path.join(inner_root, name))
            if on_disk != digest:
                shutil.rmtree(inner_dir, ignore_errors=True)
                return fail(f"{name} hashes to {on_disk}, manifest names {digest}")
        if sha256_bytes(sha_lines.encode("utf-8")) != provenance["files"]["SHA256SUMS"]:
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail("provenance.files[SHA256SUMS] does not match its bytes")
        if sha256_file(os.path.join(inner_root, "SBOM.spdx.json")) != \
                provenance["sbom"]["sha256"]:
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail("SBOM digest does not match the provenance record")
        ok("every archived file matches SHA256SUMS and provenance.files; "
           "SBOM digest matches provenance.sbom.sha256")

        # SBOM is structurally valid SPDX 2.3 JSON.
        try:
            with open(os.path.join(inner_root, "SBOM.spdx.json"), "r",
                      encoding="utf-8") as handle:
                sbom = json.load(handle)
            assert sbom.get("spdxVersion") == "SPDX-2.3"
            assert sbom.get("SPDXID") == "SPDXRef-DOCUMENT"
            assert sbom.get("packages")
        except (OSError, ValueError, AssertionError) as exc:
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail(f"SBOM is not valid SPDX 2.3 JSON: {exc}")
        ok("SBOM parses as SPDX 2.3 with a non-empty package list")

        # Binary self-reported facts must match the provenance record.
        binary_path = os.path.join(inner_root, "canter")
        proc = subprocess.run(
            [binary_path, "--version"], stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, check=False)
        if proc.returncode != 0:
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail(f"archived binary --version exits {proc.returncode}")
        text = proc.stdout.decode("utf-8", "replace")
        facts = binary_version_facts(binary_path)
        schema_facts = provenance.get("schema_facts", {})
        if facts["product_version"] != provenance.get("version"):
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail("archived binary version differs from the provenance "
                        "record")
        if str(facts["state_schema_version"]) != str(
                schema_facts.get("state_schema_version")):
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail("archived binary state schema version differs from the "
                        "provenance record")
        if facts["migration_chain"] != schema_facts.get("migration_chain"):
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail("archived binary migration chain differs from the "
                        "provenance record")
        if facts["document_schema_families"] != schema_facts.get(
                "document_schema_families"):
            shutil.rmtree(inner_dir, ignore_errors=True)
            return fail("archived binary document schema families differ from "
                        "the provenance record")
        ok("archived binary --version maps to the provenance record "
           "(version, state schema version, migration chain, families)")
        shutil.rmtree(inner_dir, ignore_errors=True)

    print(f"verify: {checks} checks passed for {os.path.basename(archive_path)}")
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="build-archive.py",
        description=(
            "Deterministic release archive builder/verifier (issue #10). "
            "See docs/RELEASING.md for the documented command rows."
        ),
    )
    sub = parser.add_subparsers(dest="command", required=True)

    build = sub.add_parser("build", help="build a platform release archive")
    build.add_argument("--repo", default=".",
                       help="release checkout whose HEAD must equal --source-ref")
    build.add_argument("--source-ref", required=True,
                       help="exact recorded 40-hex source commit")
    build.add_argument("--version", required=True,
                       help="product version (must match the binary's own)")
    build.add_argument("--binary", required=True,
                       help="path to the release binary "
                            "(cargo build --release --locked)")
    build.add_argument("--platform", required=True,
                       choices=PLATFORMS, help="target platform token")
    build.add_argument("--out-dir", default="target/release-archives",
                       help="directory for the .tar.gz and .sha256 outputs")

    verify = sub.add_parser("verify", help="verify an archive against its "
                                           "provenance record")
    verify.add_argument("--archive", required=True,
                        help="path to canter-<version>-<platform>.tar.gz")

    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.command == "build":
        return cmd_build(args)
    if args.command == "verify":
        return cmd_verify(args)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
