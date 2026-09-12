#!/usr/bin/env python3
"""cutover-92-dryrun.py — side-effect-free precondition proof for the #92 cutover plan.

Reads ONE private ``cutover-target/v1`` descriptor (never committed; the
schema is documented in ``docs/operations/cutover-92.md`` section 4) and
proves the plan's preconditions without changing anything:

* ``artifacts.*``   — the unit/artifacts it would change exist as the plan
  describes (unit parses, its program matches, the canter binary is
  executable).
* ``supervisor.*``  — exactly one supervisor process is running and its
  identity is recorded (the command-line hash is compared against the
  descriptor and the private identity record).
* ``rollback.*``    — every rollback command substitutes, is syntactically
  valid (``bash -n`` — parsed, never executed) and resolves to an existing
  binary; every restore input exists with the recorded sha256.
* ``target.*``      — the Canter-owned mechanism is reachable read-only
  (``--version``, ``status``, ``queue status`` — a fixed read-only
  allowlist), and each ``--json`` probe emits one ``hf-output/v1`` envelope.
* ``nomutation``    — the fingerprint over every inspected artifact and the
  observed process identity is identical before and after the checks.

The only processes this script starts are: ``ps`` (read the process table),
``bash -n`` (syntax check), and the descriptor's allow-listed canter probes.
It never writes, kills, loads, unloads, disables or reconfigures anything and
never executes a rollback command.

Usage:
  cutover-92-dryrun.py --target PATH [--json] [--print-rollback]

Output: ``PASS``/``FAIL`` lines per check (default mode) or one JSON document
(``--json``); the last stdout line in default mode is the machine-checkable
summary ``SUMMARY result=… checks=… passed=… failed=… mutations=0
fingerprint=…``.

``--print-rollback`` prints the substituted rollback commands
(``id<TAB>class<TAB>command``) and nothing else, so a plan runner (or the
rehearsal) copies the exact strings this script validated.

Exit codes: 0 all checks passed · 1 one or more checks failed · 2 usage error.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import plistlib
import re
import shutil
import subprocess
import sys

SCHEMA = "cutover-target/v1"
FINGERPRINT_ALGORITHM = "sha256(file-bytes|size|mtime_ns)+sha256(process-identity)"

PLACEHOLDER_RE = re.compile(r"\{([A-Za-z0-9_]+)\}")

# Probe verbs that are read-only by contract (docs/operations/cutover-92.md
# section 5). Anything else is refused before it is ever executed.
PROBE_VERBS = {
    "--version": None,
    "--help": None,
    "status": None,
    "doctor": None,
    "capabilities": None,
    "config": "show",
    "queue": "status",
}

RUN_TIMEOUT_SECS = 30
PROBE_TIMEOUT_SECS = 30


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: str) -> str:
    with open(path, "rb") as handle:
        return sha256_bytes(handle.read())


def expand(path: str) -> str:
    return os.path.expanduser(path)


def run(argv: list[str], timeout: int) -> tuple[int, str, str]:
    """Run one non-shell command; return (exit, stdout, stderr)."""
    try:
        proc = subprocess.run(
            argv,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=timeout,
            env={**os.environ, "LC_ALL": "C"},
        )
    except FileNotFoundError:
        return 127, "", "command not found"
    except subprocess.TimeoutExpired:
        return 124, "", "timed out"
    return (
        proc.returncode,
        proc.stdout.decode("utf-8", "replace"),
        proc.stderr.decode("utf-8", "replace"),
    )


class DryRun:
    def __init__(self, descriptor_path: str, quiet: bool) -> None:
        self.descriptor_path = descriptor_path
        self.quiet = quiet  # notes go to stderr; stdout stays machine-readable
        self.checks: list[dict] = []
        self.substitutions: dict[str, str] = {}
        self.descriptor: dict = {}
        self.watched_paths: list[str] = []
        self.fingerprint_before: str | None = None
        self.fingerprint_after: str | None = None

    # -- reporting ---------------------------------------------------------

    def note(self, text: str) -> None:
        stream = sys.stderr if self.quiet else sys.stdout
        print(text, file=stream)

    def check(self, check_id: str, ok: bool, detail: str = "") -> bool:
        self.checks.append(
            {"id": check_id, "status": "pass" if ok else "fail", "detail": detail}
        )
        self.note(f"{'PASS' if ok else 'FAIL'}: {check_id}" + (f" — {detail}" if detail else ""))
        return ok

    def failed_checks(self) -> list[dict]:
        return [check for check in self.checks if check["status"] == "fail"]

    # -- descriptor --------------------------------------------------------

    def load(self) -> bool:
        if not os.path.isfile(self.descriptor_path):
            return self.check("descriptor.read", False, f"{self.descriptor_path}: not a file")
        try:
            with open(self.descriptor_path, "rb") as handle:
                self.descriptor = json.loads(handle.read().decode("utf-8"))
        except (OSError, ValueError) as exc:
            return self.check("descriptor.read", False, f"unreadable/invalid JSON ({exc})")
        if not isinstance(self.descriptor, dict):
            return self.check("descriptor.read", False, "descriptor is not a JSON object")
        self.check("descriptor.read", True, self.descriptor_path)

        schema = self.descriptor.get("schema")
        if schema != SCHEMA:
            self.check("descriptor.schema", False, f"expected {SCHEMA!r}, got {schema!r}")
            return False
        self.check("descriptor.schema", True, SCHEMA)

        supervisor = self.descriptor.get("supervisor") or {}
        rollback = self.descriptor.get("rollback") or {}
        canter = self.descriptor.get("canter") or {}
        required = (
            ("queue_key", self.descriptor.get("queue_key")),
            ("host_role", self.descriptor.get("host_role")),
            ("supervisor.label", supervisor.get("label")),
            ("supervisor.unit_path", supervisor.get("unit_path")),
            ("supervisor.program", supervisor.get("program")),
            ("supervisor.process_pattern", supervisor.get("process_pattern")),
            (
                "supervisor.identity.argv_sha256",
                (supervisor.get("identity") or {}).get("argv_sha256"),
            ),
            ("rollback.backup_dir", rollback.get("backup_dir")),
            ("rollback.commands", rollback.get("commands")),
            ("canter.binary", canter.get("binary")),
            ("canter.probes", canter.get("probes")),
        )
        missing = [name for name, value in required if value in (None, "", [], {})]
        self.check(
            "descriptor.required",
            not missing,
            "missing: " + ", ".join(missing) if missing else "all present",
        )

        self.substitutions = {
            "queue_key": str(self.descriptor.get("queue_key", "")),
            "host_role": str(self.descriptor.get("host_role", "")),
            "supervisor_label": str(supervisor.get("label", "")),
            "unit_path": expand(str(supervisor.get("unit_path", ""))),
            "unit_basename": os.path.basename(expand(str(supervisor.get("unit_path", "")))),
            "supervisor_program": expand(str(supervisor.get("program", ""))),
            "supervisor_pattern": str(supervisor.get("process_pattern", "")),
            "identity_record": expand(str(supervisor.get("identity_record", ""))),
            "backup_dir": expand(str(rollback.get("backup_dir", ""))),
            "canter_binary": expand(str(canter.get("binary", ""))),
            "uid": str(os.getuid()),
        }

        # The zero-mutation fingerprint watches exactly these paths, so the
        # before/after comparison covers the same set regardless of what the
        # individual checks discover.
        self.watched_paths = [self.substitutions["unit_path"], self.substitutions["canter_binary"]]
        if supervisor.get("identity_record"):
            self.watched_paths.append(self.substitutions["identity_record"])
        if canter.get("config"):
            self.watched_paths.append(expand(str(canter["config"])))
        for item in rollback.get("inputs") or []:
            if isinstance(item, dict) and isinstance(item.get("path"), str):
                self.watched_paths.append(self.subst(item["path"]))

        # Path-bearing values must expand to absolute paths.
        path_keys = ["unit_path", "supervisor_program"]
        if supervisor.get("identity_record"):
            path_keys.append("identity_record")
        path_keys += ["backup_dir", "canter_binary"]
        relative = [
            key
            for key in path_keys
            if self.substitutions[key] and not os.path.isabs(self.substitutions[key])
        ]
        self.check(
            "descriptor.paths_absolute",
            not relative,
            "relative paths: " + ", ".join(relative) if relative else "all absolute",
        )

        # Placeholder scan over every command/input the descriptor carries.
        unknown: list[str] = []
        for text in self._substitution_texts():
            for token in PLACEHOLDER_RE.findall(text):
                if token not in self.substitutions:
                    unknown.append("{" + token + "}")
        self.check(
            "descriptor.placeholders",
            not unknown,
            "unknown tokens: " + ", ".join(sorted(set(unknown))) if unknown else "all tokens resolve",
        )
        return True

    def _substitution_texts(self) -> list[str]:
        rollback = self.descriptor.get("rollback") or {}
        texts: list[str] = []
        for command in rollback.get("commands") or []:
            if isinstance(command, dict) and isinstance(command.get("run"), str):
                texts.append(command["run"])
        for item in rollback.get("inputs") or []:
            if isinstance(item, dict) and isinstance(item.get("path"), str):
                texts.append(item["path"])
        return texts

    def subst(self, text: str) -> str:
        def replace(match: re.Match) -> str:
            return self.substitutions.get(match.group(1), match.group(0))

        return PLACEHOLDER_RE.sub(replace, text)

    # -- (a) artifacts -----------------------------------------------------

    def check_artifacts(self) -> None:
        supervisor = self.descriptor.get("supervisor") or {}
        unit_path = self.substitutions["unit_path"]
        if not os.path.isfile(unit_path):
            self.check("artifacts.unit", False, f"{unit_path}: missing")
            return
        try:
            with open(unit_path, "rb") as handle:
                unit_bytes = handle.read()
        except OSError as exc:
            self.check("artifacts.unit", False, f"{unit_path}: unreadable ({exc})")
            return
        if not unit_bytes:
            self.check("artifacts.unit", False, f"{unit_path}: empty")
            return
        unit_sha = sha256_bytes(unit_bytes)
        self.check(
            "artifacts.unit",
            True,
            f"{unit_path} sha256={unit_sha[:12]} bytes={len(unit_bytes)}",
        )

        if unit_path.endswith(".plist"):
            try:
                unit = plistlib.loads(unit_bytes)
            except Exception as exc:  # noqa: BLE001 - any parse failure is a FAIL
                self.check("artifacts.unit_parses", False, f"plist parse failed ({exc})")
                return
            self.check("artifacts.unit_parses", True, "plist parses")
            label = unit.get("Label") if isinstance(unit, dict) else None
            expected_label = supervisor.get("label")
            self.check(
                "artifacts.unit_label",
                label == expected_label,
                f"unit Label={label!r} descriptor label={expected_label!r}",
            )
            program_argv = unit.get("ProgramArguments") if isinstance(unit, dict) else None
            program = unit.get("Program") if isinstance(unit, dict) else None
            declared = self.substitutions["supervisor_program"]
            found = None
            if isinstance(program_argv, list) and program_argv:
                found = program_argv[0]
            elif isinstance(program, str):
                found = program
            self.check(
                "artifacts.unit_program",
                found == declared,
                f"unit runs {found!r} descriptor program={declared!r}",
            )

        binary = self.substitutions["canter_binary"]
        self.check(
            "artifacts.canter_binary",
            os.path.isfile(binary) and os.access(binary, os.X_OK),
            f"{binary}: " + ("executable" if os.path.isfile(binary) else "missing"),
        )
        config = (self.descriptor.get("canter") or {}).get("config")
        if config:
            config_path = expand(str(config))
            present = os.path.isfile(config_path)
            self.check(
                "artifacts.canter_config",
                present,
                f"{config_path}: " + ("present" if present else "missing"),
            )

    # -- (b) supervisor running + identity recorded ------------------------

    def scan_supervisor(self) -> list[tuple[str, str]]:
        """Read-only process table scan: [(pid, normalized command)]."""
        rc, out, _err = run(["ps", "-Aww", "-o", "pid=,command="], RUN_TIMEOUT_SECS)
        if rc != 0:
            return []
        pattern = self.substitutions["supervisor_pattern"]
        if not pattern:
            return []
        matches: list[tuple[str, str]] = []
        for line in out.splitlines():
            stripped = line.strip()
            if not stripped or pattern not in stripped:
                continue
            pid, _sep, command = stripped.partition(" ")
            if not command:
                continue
            matches.append((pid, " ".join(command.split())))
        return matches

    def check_supervisor(self) -> None:
        matches = self.scan_supervisor()
        self.check(
            "supervisor.running",
            len(matches) == 1,
            f"{len(matches)} process(es) match the declared pattern",
        )
        if len(matches) != 1:
            return
        pid, command = matches[0]
        argv_sha = sha256_bytes(command.encode("utf-8"))
        expected = ((self.descriptor.get("supervisor") or {}).get("identity") or {}).get(
            "argv_sha256"
        )
        self.check(
            "supervisor.identity_descriptor",
            argv_sha == expected,
            f"live command-line sha256={argv_sha[:12]} descriptor={str(expected)[:12]}",
        )

        record_value = (self.descriptor.get("supervisor") or {}).get("identity_record")
        if not record_value:
            self.note("note: supervisor.identity_record not declared; the descriptor hash is the only identity witness")
            return
        record_path = self.substitutions["identity_record"]
        if not os.path.isfile(record_path):
            self.check(
                "supervisor.identity_record",
                False,
                f"{record_path}: missing (run plan step S1 first)",
            )
            return
        try:
            with open(record_path, "rb") as handle:
                record = json.loads(handle.read().decode("utf-8"))
        except (OSError, ValueError) as exc:
            self.check("supervisor.identity_record", False, f"{record_path}: unreadable ({exc})")
            return
        recorded = record.get("argv_sha256")
        pid_note = ""
        if record.get("pid") not in (None, ""):
            pid_note = f" recorded_pid={record.get('pid')} live_pid={pid} (informational)"
        self.check(
            "supervisor.identity_record",
            recorded == argv_sha,
            f"record sha256={str(recorded)[:12]} live={argv_sha[:12]}{pid_note}",
        )

    # -- (c) rollback commands resolve + are syntactically valid -----------

    def check_rollback(self) -> None:
        rollback = self.descriptor.get("rollback") or {}
        commands = rollback.get("commands") or []
        syntax_failures: list[str] = []
        resolve_failures: list[str] = []
        for command in commands:
            if not isinstance(command, dict) or not isinstance(command.get("run"), str):
                syntax_failures.append("<malformed command entry>")
                continue
            text = self.subst(command["run"])
            rc, _out, err = run(["bash", "-n", "-c", text], RUN_TIMEOUT_SECS)
            if rc != 0:
                syntax_failures.append(f"{command.get('id', '?')} ({err.strip()[:80]})")
            first_word = text.split()[0] if text.split() else ""
            if first_word and not self._binary_resolves(first_word):
                resolve_failures.append(f"{command.get('id', '?')} -> {first_word}")
        self.check(
            "rollback.commands_syntax",
            bool(commands) and not syntax_failures,
            "bash -n clean for every command"
            if not syntax_failures
            else "; ".join(syntax_failures),
        )
        self.check(
            "rollback.commands_resolve",
            bool(commands) and not resolve_failures,
            "every command word resolves"
            if not resolve_failures
            else "; ".join(resolve_failures),
        )

        declared = rollback.get("binaries") or []
        unresolved = [name for name in declared if not self._binary_resolves(str(name))]
        self.check(
            "rollback.binaries",
            not unresolved,
            "all declared binaries resolve" if not unresolved else "unresolved: " + ", ".join(unresolved),
        )

        inputs = rollback.get("inputs") or []
        input_failures: list[str] = []
        for item in inputs:
            if not isinstance(item, dict) or not isinstance(item.get("path"), str):
                input_failures.append("<malformed input entry>")
                continue
            path = self.subst(item["path"])
            if not os.path.isfile(path):
                input_failures.append(f"{path}: missing")
                continue
            digest = sha256_file(path)
            if item.get("sha256") and digest != item["sha256"]:
                input_failures.append(f"{path}: sha256 mismatch")
        self.check(
            "rollback.inputs",
            bool(inputs) and not input_failures,
            "all restore inputs present and hash-matched"
            if not input_failures
            else "; ".join(input_failures),
        )

    def _binary_resolves(self, word: str) -> bool:
        if "/" in word:
            return os.path.isfile(word) and os.access(word, os.X_OK)
        return shutil.which(word) is not None

    # -- (d) target mechanism reachable read-only --------------------------

    def check_target(self) -> None:
        binary = self.substitutions["canter_binary"]
        rc, out, err = run([binary, "--version"], PROBE_TIMEOUT_SECS)
        self.check(
            "target.version",
            rc == 0 and bool(out.strip()),
            f"{binary} --version exit={rc}" + (f" stderr={err.strip()[:80]}" if rc != 0 else ""),
        )
        probes = (self.descriptor.get("canter") or {}).get("probes") or []
        for index, probe in enumerate(probes):
            if not isinstance(probe, list) or not probe:
                self.check(f"target.probe[{index}]", False, "malformed probe entry")
                continue
            argv = [str(part) for part in probe]
            allowed, reason = self._probe_allowed(argv)
            if not allowed:
                self.check(f"target.probe[{index}]", False, f"refused before execution: {reason}")
                continue
            rc, out, err = run([binary] + argv, PROBE_TIMEOUT_SECS)
            label = " ".join(argv)
            if rc != 0:
                self.check(
                    f"target.probe[{index}]",
                    False,
                    f"`{label}` exit={rc} stderr={err.strip()[:100]}",
                )
                continue
            if "--json" in argv:
                try:
                    doc = json.loads(out.strip())
                except ValueError as exc:
                    self.check(
                        f"target.probe[{index}]",
                        False,
                        f"`{label}` stdout is not one JSON document ({exc})",
                    )
                    continue
                schema = doc.get("schema") if isinstance(doc, dict) else None
                self.check(
                    f"target.probe[{index}]",
                    schema == "hf-output/v1",
                    f"`{label}` exit=0 envelope schema={schema!r}",
                )
            else:
                self.check(f"target.probe[{index}]", True, f"`{label}` exit=0")

    def _probe_allowed(self, argv: list[str]) -> tuple[bool, str]:
        verb = argv[0]
        if verb not in PROBE_VERBS:
            return False, f"{verb!r} is not on the read-only probe allowlist"
        required_second = PROBE_VERBS[verb]
        if required_second is not None and (len(argv) < 2 or argv[1] != required_second):
            return False, f"{verb!r} probes must be `{verb} {required_second}`"
        return True, ""

    # -- zero-mutation proof ----------------------------------------------

    def fingerprint(self) -> str:
        parts = []
        for path in sorted(set(self.watched_paths)):
            try:
                stat = os.stat(path)
                parts.append(f"{path}|{sha256_file(path)}|{stat.st_size}|{stat.st_mtime_ns}")
            except OSError:
                parts.append(f"{path}|absent")
        parts.append("process|" + "|".join(f"{pid}:{cmd}" for pid, cmd in sorted(self.scan_supervisor())))
        return sha256_bytes("\n".join(parts).encode("utf-8"))

    def check_nomutation(self) -> None:
        self.fingerprint_after = self.fingerprint()
        self.check(
            "nomutation",
            self.fingerprint_before is not None
            and self.fingerprint_before == self.fingerprint_after,
            f"{FINGERPRINT_ALGORITHM} unchanged across the run",
        )

    # -- driver ------------------------------------------------------------

    def run_checks(self) -> int:
        if not self.load():
            return self.finish()
        self.fingerprint_before = self.fingerprint()
        self.check_artifacts()
        self.check_supervisor()
        self.check_rollback()
        self.check_target()
        self.check_nomutation()
        return self.finish()

    def rollback_lines(self) -> int:
        """Print substituted rollback commands; exit 0/2 only."""
        if not self.load() or self.failed_checks():
            for check in self.failed_checks():
                sys.stderr.write(f"FAIL: {check['id']} — {check['detail']}\n")
            return 2
        for command in (self.descriptor.get("rollback") or {}).get("commands") or []:
            if not isinstance(command, dict) or not isinstance(command.get("run"), str):
                sys.stderr.write("error: malformed rollback command entry\n")
                return 2
            print(
                "{}\t{}\t{}".format(
                    command.get("id", "?"),
                    command.get("class", "unspecified"),
                    self.subst(command["run"]),
                )
            )
        return 0

    def finish(self) -> int:
        failed = self.failed_checks()
        passed = len(self.checks) - len(failed)
        mutations = 0  # by construction: no write/kill/load/disable path exists
        fingerprint = self.fingerprint_after or self.fingerprint_before or ""
        result = "ok" if self.checks and not failed else "failed"
        if self.quiet:
            doc = {
                "schema": "cutover-92-dryrun/v1",
                "result": result,
                "checks": self.checks,
                "summary": {
                    "checks": len(self.checks),
                    "passed": passed,
                    "failed": len(failed),
                    "mutations": mutations,
                    "fingerprint": fingerprint[:12],
                },
                "exit_code": 0 if result == "ok" else 1,
            }
            print(json.dumps(doc, sort_keys=True))
        else:
            for check in failed:
                self.note(f"FAILED: {check['id']} — {check['detail']}")
            print(
                "SUMMARY result={} checks={} passed={} failed={} mutations={} fingerprint={}".format(
                    result, len(self.checks), passed, len(failed), mutations, fingerprint[:12]
                )
            )
        return 0 if result == "ok" else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="cutover-92-dryrun.py",
        description="side-effect-free precondition proof for the #92 cutover plan",
    )
    parser.add_argument("--target", required=True, help="private cutover-target/v1 descriptor")
    parser.add_argument("--json", action="store_true", help="emit one JSON document instead of text")
    parser.add_argument(
        "--print-rollback",
        action="store_true",
        help="print the substituted rollback commands (id<TAB>class<TAB>command) and nothing else",
    )
    args = parser.parse_args(argv)

    if not os.path.isfile(args.target):
        sys.stderr.write(f"error: --target {args.target} is not a file\n")
        return 2
    try:
        if args.print_rollback:
            return DryRun(args.target, quiet=True).rollback_lines()
        return DryRun(args.target, quiet=args.json).run_checks()
    except KeyboardInterrupt:
        sys.stderr.write("error: interrupted\n")
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
