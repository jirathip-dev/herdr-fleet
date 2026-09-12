#!/usr/bin/env python3
"""test-cutover-92.py — self-tests (mutation probes) for the #92 cutover tooling.

Proves, with real runs and no live side effects:

* the dry-run is GREEN against an intact synthetic fixture and RED (non-zero,
  named check) when a precondition is mutated — one probe per check class:
  artifacts, supervisor running/identity, rollback syntax/resolution/inputs,
  probe allowlist/envelope, and the zero-mutation fingerprint (a deliberately
  mutating probe must turn ``nomutation`` red);
* the dry-run mutates nothing: a source-level AST guard (no write/kill/shell
  path, one single ``subprocess.run`` call site) plus an external
  before/after sha256 comparison over the fixture's files and process list;
* the rehearsal is sandbox-contained and green, and a rehearsal root outside
  the OS temp dir is refused before anything is created;
* the public-data boundary of the new artifacts: no absolute home paths and
  no service-manager invocation in the dry-run; in the rehearsal every
  service-manager mention is a descriptor string, a printed line or a
  comment.

Stdlib only. Synthetic fixtures only; every runtime path comes from
``tempfile`` and is removed afterwards. Nothing here touches a service
manager, a launchd job, a cron entry or the live supervisor.

Usage: python3 scripts/test-cutover-92.py
Exit codes: 0 all self-tests passed · 1 a self-test failed.
"""

from __future__ import annotations

import ast
import hashlib
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

REPO = pathlib.Path(__file__).resolve().parent.parent
DRYRUN = REPO / "scripts" / "cutover-92-dryrun.py"
REHEARSAL = REPO / "scripts" / "cutover-92-rehearsal.sh"
PLAN = REPO / "docs" / "operations" / "cutover-92.md"

PATTERN = "fixture-supervisor --serve"
LABEL = "com.example.fixture-supervisor"
WATCHED_NAMES = ("unit", "backup_copy", "identity", "canter_stub", "config")

RESULTS: list[tuple[str, bool, str]] = []
SKIPS: list[str] = []


def record(name: str, ok: bool, detail: str = "") -> None:
    RESULTS.append((name, ok, detail))
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" — {detail}" if detail else ""))


def skip(name: str, detail: str) -> None:
    SKIPS.append(name)
    print(f"SKIP: {name} — {detail}")


def sha256_file(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def fixture_root_pids(root: pathlib.Path) -> list[int]:
    """Pids whose command line names this fixture root (self/ancestors excepted)."""
    prefix = str(root).rstrip("/") + "/"
    excluded = {os.getpid(), os.getppid()}
    pids: list[int] = []
    for line in ps_lines():
        pid, _sep, command = line.strip().partition(" ")
        if not pid.isdigit() or int(pid) in excluded:
            continue
        if prefix in command:
            pids.append(int(pid))
    return pids


def leftover_processes() -> list[str]:
    """Any process naming a rehearsal sandbox, a self-test fixture or the
    fixture program: all three are this suite's own, so any hit is a leak."""
    markers = ("cutover-92-rehearsal", "cutover-92-selftest", "fixture-supervisor --serve")
    return [line for line in ps_lines() if any(marker in line for marker in markers)]


def ps_lines() -> list[str]:
    out = subprocess.run(
        ["ps", "-Aww", "-o", "pid=,command="], capture_output=True, text=True, check=False
    ).stdout
    return out.splitlines()


def matching_commands(pattern: str) -> list[str]:
    matches = []
    for line in ps_lines():
        stripped = line.strip()
        if pattern not in stripped:
            continue
        _pid, _sep, command = stripped.partition(" ")
        matches.append(" ".join(command.split()))
    return matches


class Fixture:
    """One synthetic host: unit, fixture supervisor process, stubs, descriptor."""

    def __init__(self, root: pathlib.Path) -> None:
        self.root = root
        self.host = root / "host"
        self.units = self.host / "units"
        self.bin = self.host / "bin"
        self.backup = self.host / "backup"
        self.run = self.host / "run"
        self.unit = self.units / f"{LABEL}.plist"
        self.program = self.bin / "fixture-supervisor"
        self.canter = self.bin / "canter"
        self.config = self.host / "canter.toml"
        self.identity = self.host / "identity.json"
        self.backup_copy = self.backup / self.unit.name
        self.pidfile = self.run / "supervisor.pid"
        self.process: subprocess.Popen | None = None
        self.unit_sha = ""
        self.stop_verified = True

    # -- construction ------------------------------------------------------

    def build(self) -> None:
        for directory in (self.units, self.bin, self.backup, self.run, self.host / "cases"):
            directory.mkdir(parents=True, exist_ok=True)
        self.unit.write_text(self.unit_text(self.program), encoding="utf-8")
        self.backup_copy.write_bytes(self.unit.read_bytes())
        self.unit_sha = sha256_file(self.unit)
        self.program.write_text(
            "#!/usr/bin/env bash\n"
            "# Same parent watchdog as the rehearsal fixture (#92-R1): if the\n"
            "# self-test dies, the fixture removes its root and exits.\n"
            "echo \"$$\" >\"$CUTOVER92_FIXTURE_PIDFILE\"\n"
            "while :; do\n"
            "    parent=\"$(ps -ww -p \"${CUTOVER92_FIXTURE_PARENT_PID:-0}\" -o command= 2>/dev/null || true)\"\n"
            "    case \"$parent\" in\n"
            "        *\"${CUTOVER92_FIXTURE_PARENT_MATCH:-test-cutover-92}\"*) ;;\n"
            "        *)\n"
            "            exit 0\n"
            "            ;;\n"
            "    esac\n"
            "    sleep 1\n"
            "done\n",
            encoding="utf-8",
        )
        self.program.chmod(0o755)
        self.canter.write_text(
            "#!/usr/bin/env bash\n"
            "case \"${1:-}\" in\n"
            "    --version) echo \"canter-fixture 0.0.0 (self-test stub)\"; exit 0 ;;\n"
            "    status) printf '%s\\n' '{\"schema\":\"hf-output/v1\",\"command\":\"status\","
            "\"data\":{},\"exit_code\":0,\"kind\":\"ok\"}'; exit 0 ;;\n"
            "    *) echo \"canter-fixture: unsupported probe\" >&2; exit 2 ;;\n"
            "esac\n",
            encoding="utf-8",
        )
        self.canter.chmod(0o755)
        self.config.write_text("[daemon]\nenabled = false\n", encoding="utf-8")

    @staticmethod
    def unit_text(program: pathlib.Path, label: str = LABEL) -> str:
        return (
            '<?xml version="1.0" encoding="UTF-8"?>\n'
            '<plist version="1.0">\n'
            "<dict>\n"
            f"  <key>Label</key><string>{label}</string>\n"
            "  <key>ProgramArguments</key>\n"
            "  <array>\n"
            f"    <string>{program}</string>\n"
            "    <string>--serve</string>\n"
            "  </array>\n"
            "</dict>\n"
            "</plist>\n"
        )

    # -- process -----------------------------------------------------------

    def start(self) -> None:
        env = {
            **os.environ,
            "CUTOVER92_FIXTURE_PIDFILE": str(self.pidfile),
            "CUTOVER92_FIXTURE_PARENT_PID": str(os.getpid()),
            "CUTOVER92_FIXTURE_PARENT_MATCH": "test-cutover-92",
        }
        log = open(self.host / "fixture-supervisor.log", "wb")
        self.process = subprocess.Popen(
            [str(self.program), "--serve"],
            env=env,
            stdout=log,
            stderr=log,
            start_new_session=True,
        )
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if self.pidfile.exists() and self.pidfile.read_text().strip():
                break
            time.sleep(0.05)
        time.sleep(0.3)  # let ps observe the final argv

    def spawn_extra(self) -> subprocess.Popen:
        """A SECOND matching process (proves the exactly-one rule bites)."""
        env = {
            **os.environ,
            "CUTOVER92_FIXTURE_PIDFILE": str(self.host / "cases" / "second.pid"),
            "CUTOVER92_FIXTURE_PARENT_PID": str(os.getpid()),
            "CUTOVER92_FIXTURE_PARENT_MATCH": "test-cutover-92",
        }
        log = open(self.host / "cases" / "second.log", "wb")
        extra = subprocess.Popen(
            [str(self.program), "--serve"],
            env=env,
            stdout=log,
            stderr=log,
            start_new_session=True,
        )
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if len(matching_commands(PATTERN)) >= 2:
                return extra
            time.sleep(0.05)
        return extra

    @staticmethod
    def stop_process(process: subprocess.Popen) -> None:
        if process.poll() is None:
            try:
                os.killpg(os.getpgid(process.pid), signal.SIGTERM)
            except (ProcessLookupError, PermissionError):
                pass
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=10)

    def stop(self) -> None:
        """Stop every process naming the fixture root: TERM, wait, KILL, wait,
        then verify by re-scanning (never assume the kill worked)."""
        if self.process is not None and self.process.poll() is None:
            try:
                os.killpg(os.getpgid(self.process.pid), signal.SIGTERM)
            except (ProcessLookupError, PermissionError):
                pass
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            pids = fixture_root_pids(self.root)
            if not pids:
                return
            for pid in pids:
                try:
                    os.kill(pid, signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    pass
            time.sleep(0.1)
        self.stop_verified = not fixture_root_pids(self.root)

    def write_identity(self, argv_sha: str) -> None:
        self.identity.write_text(
            json.dumps(
                {
                    "schema": "cutover-identity/v1",
                    "pid": int(self.pidfile.read_text().strip()),
                    "argv_sha256": argv_sha,
                    "unit_sha256": self.unit_sha,
                    "count": 1,
                },
                indent=2,
            ),
            encoding="utf-8",
        )

    def live_argv_sha(self) -> str:
        matches = matching_commands(PATTERN)
        return hashlib.sha256(matches[0].encode("utf-8")).hexdigest() if len(matches) == 1 else ""

    # -- descriptor --------------------------------------------------------

    def base_descriptor(self) -> dict:
        return {
            "schema": "cutover-target/v1",
            "queue_key": "example-queue",
            "host_role": "example-host-role",
            "supervisor": {
                "label": LABEL,
                "unit_path": str(self.unit),
                "program": str(self.program),
                "process_pattern": PATTERN,
                "identity": {"argv_sha256": self.live_argv_sha()},
                "identity_record": str(self.identity),
            },
            "rollback": {
                "backup_dir": str(self.backup),
                "binaries": ["cp", "cmp", "true"],
                "inputs": [{"path": "{backup_dir}/" + self.unit.name, "sha256": self.unit_sha}],
                "commands": [
                    {"id": "R1", "class": "sandbox-runnable", "run": "cp -p {backup_dir}/" + self.unit.name + " {unit_path}"},
                    {"id": "R2", "class": "sandbox-runnable", "run": "cmp -s {backup_dir}/" + self.unit.name + " {unit_path}"},
                    {"id": "R3", "class": "owner-executed", "run": "true gui/{uid}/{supervisor_label}"},
                ],
            },
            "canter": {
                "binary": str(self.canter),
                "config": str(self.config),
                "probes": [["--version"], ["status", "--json"]],
            },
        }

    def variant(self, name: str, mutate) -> pathlib.Path:
        document = self.base_descriptor()
        mutate(document)
        path = self.host / "cases" / f"{name}.json"
        path.write_text(json.dumps(document, indent=2), encoding="utf-8")
        return path

    # -- fingerprints ------------------------------------------------------

    def watched_files(self) -> list[pathlib.Path]:
        return [self.unit, self.backup_copy, self.identity, self.canter, self.config]

    def file_fingerprint(self) -> str:
        parts = []
        for path in self.watched_files():
            try:
                stat = path.stat()
                parts.append(f"{path}|{sha256_file(path)}|{stat.st_size}|{stat.st_mtime_ns}")
            except OSError:
                parts.append(f"{path}|absent")
        return hashlib.sha256("\n".join(parts).encode("utf-8")).hexdigest()

    def fingerprint(self) -> str:
        process = "process|" + "|".join(sorted(matching_commands(PATTERN)))
        return hashlib.sha256(f"{self.file_fingerprint()}\n{process}".encode("utf-8")).hexdigest()


def run_dryrun(target: pathlib.Path, *extra: str) -> tuple[int, str, str]:
    proc = subprocess.run(
        [sys.executable, str(DRYRUN), "--target", str(target), *extra],
        capture_output=True,
        text=True,
        check=False,
        timeout=120,
    )
    return proc.returncode, proc.stdout, proc.stderr


def dryrun_case(name: str, target: pathlib.Path, expect_exit: int, expect_fail: str | None = None) -> None:
    rc, out, err = run_dryrun(target)
    problems = []
    if rc != expect_exit:
        problems.append(f"exit {rc} != {expect_exit} (stderr: {err.strip()[:120]})")
    if expect_fail is not None and f"FAIL: {expect_fail}" not in out:
        problems.append(f"missing `FAIL: {expect_fail}`")
    if expect_exit == 0 and "SUMMARY result=ok" not in out:
        problems.append("missing SUMMARY result=ok")
    record(name, not problems, "; ".join(problems) or f"exit={rc}")


# --------------------------------------------------------------------------
# Static guards (source-level, no execution)
# --------------------------------------------------------------------------

def static_guards() -> None:
    dry_source = DRYRUN.read_text(encoding="utf-8")
    tree = ast.parse(dry_source)
    forbidden: list[str] = []
    subprocess_calls = 0
    mutating = {
        "os.remove", "os.unlink", "os.rename", "os.replace", "os.chmod", "os.chown",
        "os.kill", "os.killpg", "os.system", "os.mkdir", "os.makedirs", "os.rmdir",
        "shutil.rmtree", "shutil.move", "shutil.copy", "shutil.copy2", "shutil.copyfile",
        "subprocess.Popen", "subprocess.call", "subprocess.check_output", "subprocess.check_call",
    }
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        name = None
        if isinstance(node.func, ast.Attribute) and isinstance(node.func.value, ast.Name):
            name = f"{node.func.value.id}.{node.func.attr}"
        elif isinstance(node.func, ast.Name):
            name = node.func.id
        if name in mutating:
            forbidden.append(name)
        if name == "subprocess.run":
            subprocess_calls += 1
        if name == "open":
            mode = node.args[1] if len(node.args) > 1 else None
            if isinstance(mode, ast.Constant) and isinstance(mode.value, str):
                if any(flag in mode.value for flag in ("w", "a", "x", "+")):
                    forbidden.append(f"open(mode={mode.value!r})")
        for keyword in node.keywords:
            if keyword.arg == "shell" and isinstance(keyword.value, ast.Constant) and keyword.value.value is True:
                forbidden.append("shell=True")
    record(
        "static guard: dry-run has no write/kill/shell path",
        not forbidden,
        "found: " + ", ".join(sorted(set(forbidden))) if forbidden else "clean",
    )
    record(
        "static guard: dry-run has exactly one subprocess call site",
        subprocess_calls == 1,
        f"subprocess.run call sites: {subprocess_calls}",
    )
    record(
        "static guard: dry-run never names a service manager",
        "launchctl" not in dry_source and "systemctl" not in dry_source,
        "no launchctl/systemctl token",
    )

    rehearsal_source = REHEARSAL.read_text(encoding="utf-8")
    mentions = [line for line in rehearsal_source.splitlines() if "launchctl" in line]
    offenders = [
        line.strip()
        for line in mentions
        if not line.lstrip().startswith("#")
        and '"run":' not in line
        and '"binaries":' not in line
    ]
    record(
        "static guard: rehearsal only mentions the service manager in data/echo lines",
        not offenders,
        "; ".join(offenders)
        if offenders
        else f"{len(mentions)} data/echo mention line(s), no invocation",
    )
    record(
        "static guard: rehearsal executes only sandbox-runnable commands via the path guard",
        'guard_command_paths "$RCOMMAND"' in rehearsal_source
        and rehearsal_source.index('guard_command_paths "$RCOMMAND"')
        < rehearsal_source.index('bash -u -c "$RCOMMAND"'),
        "guard precedes execution",
    )

    # Fragments are assembled at runtime so this file's own source does not
    # trip the rule it enforces (the same technique as check-public-tree.py).
    abs_unix = "/" + "Users" + "/"
    abs_home = "/" + "home" + "/"
    for path in (DRYRUN, REHEARSAL, PLAN):
        text = path.read_text(encoding="utf-8")
        record(
            f"static guard: {path.name} carries no absolute home path",
            abs_unix not in text and abs_home not in text,
            "no absolute home-path literal",
        )

    plan_text = PLAN.read_text(encoding="utf-8").lower()
    record(
        "static guard: plan documents the approval gate, rollback and blast radius",
        all(
            phrase in plan_text
            for phrase in ("sole approver", "rollback", "blast radius", "approval gate")
        ),
        "required sections present",
    )


# --------------------------------------------------------------------------
# Rehearsal runs
# --------------------------------------------------------------------------

def rehearsal_sandbox_dirs() -> set[str]:
    base = pathlib.Path(tempfile.gettempdir())
    return {str(path) for path in base.glob("cutover-92-rehearsal.*")}


def rehearsal_guards(tmp: pathlib.Path) -> None:
    sandboxes_before = rehearsal_sandbox_dirs()
    outside = tmp / "not-a-rehearsal" / "cutover-92-rehearsal.probe"
    proc = subprocess.run(
        ["bash", str(REHEARSAL)],
        capture_output=True,
        text=True,
        check=False,
        timeout=300,
        env={**os.environ, "CUTOVER92_REHEARSAL_ROOT": str(outside)},
    )
    record(
        "rehearsal refuses a root outside the OS temp dir (containment RED)",
        proc.returncode == 2 and "REFUSED" in proc.stderr and not outside.exists(),
        f"exit={proc.returncode}, refused={('REFUSED' in proc.stderr)}, created={outside.exists()}",
    )

    if shutil.which("launchctl") is None:
        for name in ("rehearsal green run", "rehearsal --dry-run-only", "rehearsal killed mid-run"):
            skip(name, "launchctl absent (non-macOS host class)")
        return

    proc = subprocess.run(
        ["bash", str(REHEARSAL)],
        capture_output=True,
        text=True,
        check=False,
        timeout=600,
    )
    out = proc.stdout
    problems = []
    if proc.returncode != 0:
        problems.append(f"exit {proc.returncode} (stderr: {proc.stderr.strip()[:200]})")
    for needle in (
        "REHEARSAL result=ok",
        "live_paths_touched=0",
        "fixtures_left=0",
        "sandbox_removed=yes",
        "SIMULATED (owner-executed on host, NOT run here)",
        "dry-run fails while the supervisor is down",
        "no fixture process references the sandbox and the sandbox is removed",
    ):
        if needle not in out:
            problems.append(f"missing {needle!r}")
    record("rehearsal green run (disposable fixture)", not problems, "; ".join(problems) or "exit=0")

    proc = subprocess.run(
        ["bash", str(REHEARSAL), "--dry-run-only"],
        capture_output=True,
        text=True,
        check=False,
        timeout=600,
    )
    out = proc.stdout
    problems = []
    if proc.returncode != 0:
        problems.append(f"exit {proc.returncode} (stderr: {proc.stderr.strip()[:200]})")
    for needle in (
        "RUN: python3",
        "dry-run raw exit: 0",
        "mode=dry-run-only",
        "fixtures_left=0",
        "sandbox_removed=yes",
    ):
        if needle not in out:
            problems.append(f"missing {needle!r}")
    record(
        "rehearsal --dry-run-only runs the documented fixture invocation and cleans up",
        not problems,
        "; ".join(problems) or "exit=0",
    )

    killed = subprocess.Popen(
        ["bash", str(REHEARSAL)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(1.2)
    killed.send_signal(signal.SIGKILL)
    killed.wait(timeout=60)
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if not leftover_processes() and not (rehearsal_sandbox_dirs() - sandboxes_before):
            break
        time.sleep(0.25)
    new_dirs = rehearsal_sandbox_dirs() - sandboxes_before
    stale = leftover_processes()
    record(
        "rehearsal killed mid-run (SIGKILL, no trap can run) leaves no process and no sandbox",
        not stale and not new_dirs,
        f"processes={len(stale)} new sandbox dirs={len(new_dirs)}"
        + (f": {stale[0].strip()[:100]}" if stale else ""),
    )

    deadline = time.monotonic() + 12
    stale = leftover_processes()
    while stale and time.monotonic() < deadline:
        time.sleep(0.25)
        stale = leftover_processes()
    record(
        "rehearsal leaves no fixture process behind",
        not stale,
        f"{len(stale)} straggler(s)" if not stale else f"{len(stale)} straggler(s): {stale[0].strip()[:120]}",
    )


# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------

def main() -> int:
    for script in (DRYRUN, REHEARSAL, PLAN):
        if not script.is_file():
            sys.stderr.write(f"error: missing {script}\n")
            return 1

    static_guards()

    with tempfile.TemporaryDirectory(prefix="cutover-92-selftest.") as tmp_name:
        tmp = pathlib.Path(tmp_name)
        fixture = Fixture(tmp / "fixture")
        fixture.build()
        fixture.start()

        matches = matching_commands(PATTERN)
        if len(matches) != 1:
            record(
                "fixture supervisor is observable exactly once",
                False,
                f"{len(matches)} match(es) — cannot run the dry-run cases",
            )
            fixture.stop()
            return 1
        fixture.write_identity(fixture.live_argv_sha())
        watched_before = fixture.fingerprint()
        files_before = fixture.file_fingerprint()

        base = fixture.variant("green", lambda doc: None)
        dryrun_case("dry-run GREEN against the intact fixture", base, 0)

        rc, out, _err = run_dryrun(base)
        print("    documented standalone invocation:")
        print(f"      python3 scripts/cutover-92-dryrun.py --target {base}")
        print(f"    raw exit: {rc}")
        for line in out.strip().splitlines():
            print(f"      {line}")

        rc, out, _err = run_dryrun(base, "--json")
        doc = json.loads(out) if out.strip() else {}
        record(
            "dry-run --json emits one machine-checkable document",
            rc == 0
            and doc.get("schema") == "cutover-92-dryrun/v1"
            and doc.get("result") == "ok"
            and doc.get("summary", {}).get("mutations") == 0
            and doc.get("summary", {}).get("failed") == 0,
            f"exit={rc}, result={doc.get('result')}, mutations={doc.get('summary', {}).get('mutations')}",
        )

        rc, out, _err = run_dryrun(base, "--print-rollback")
        lines = [line for line in out.splitlines() if line.strip()]
        fields_ok = all(len(line.split("\t")) == 3 for line in lines)
        unsubstituted = [line for line in lines if "{" in line or "}" in line]
        paths_substituted = str(fixture.unit) in lines[0] and str(fixture.backup) in lines[0] if lines else False
        record(
            "dry-run --print-rollback prints the substituted commands",
            rc == 0 and len(lines) == 3 and fields_ok and not unsubstituted and paths_substituted,
            f"exit={rc}, {len(lines)} command line(s), fields_ok={fields_ok}, "
            f"unsubstituted={len(unsubstituted)}, paths_substituted={paths_substituted}",
        )

        # -- artifacts ------------------------------------------------------
        dryrun_case(
            "RED artifacts.unit (unit missing)",
            fixture.variant(
                "unit-missing",
                lambda doc: doc["supervisor"].update(unit_path=str(fixture.units / "absent.plist")),
            ),
            1,
            "artifacts.unit",
        )
        other_label = fixture.units / "other-label.plist"
        other_label.write_text(Fixture.unit_text(fixture.program, label="com.example.other"), encoding="utf-8")
        dryrun_case(
            "RED artifacts.unit_label (label mismatch)",
            fixture.variant(
                "label-mismatch",
                lambda doc: doc["supervisor"].update(unit_path=str(other_label)),
            ),
            1,
            "artifacts.unit_label",
        )
        other_program = fixture.bin / "not-the-program"
        other_program.write_text("#!/usr/bin/env bash\nexit 0\n", encoding="utf-8")
        other_program.chmod(0o755)
        other_unit = fixture.units / "other-program.plist"
        other_unit.write_text(Fixture.unit_text(other_program), encoding="utf-8")
        dryrun_case(
            "RED artifacts.unit_program (program mismatch)",
            fixture.variant(
                "program-mismatch",
                lambda doc: doc["supervisor"].update(unit_path=str(other_unit)),
            ),
            1,
            "artifacts.unit_program",
        )
        dryrun_case(
            "RED artifacts.canter_binary (binary missing)",
            fixture.variant(
                "binary-missing",
                lambda doc: doc["canter"].update(binary=str(fixture.bin / "absent-canter")),
            ),
            1,
            "artifacts.canter_binary",
        )
        dryrun_case(
            "RED artifacts.canter_config (config missing)",
            fixture.variant(
                "config-missing",
                lambda doc: doc["canter"].update(config=str(fixture.host / "absent.toml")),
            ),
            1,
            "artifacts.canter_config",
        )

        # -- descriptor -----------------------------------------------------
        dryrun_case(
            "RED descriptor.schema (wrong schema id)",
            fixture.variant("schema-wrong", lambda doc: doc.update(schema="cutover-target/v0")),
            1,
            "descriptor.schema",
        )
        dryrun_case(
            "RED descriptor.paths_absolute (relative unit path)",
            fixture.variant("relative-path", lambda doc: doc["supervisor"].update(unit_path="units/rel.plist")),
            1,
            "descriptor.paths_absolute",
        )
        dryrun_case(
            "RED descriptor.placeholders (unknown token)",
            fixture.variant(
                "unknown-token",
                lambda doc: doc["rollback"]["commands"].append(
                    {"id": "RX", "class": "sandbox-runnable", "run": "true {no_such_key}"}
                ),
            ),
            1,
            "descriptor.placeholders",
        )

        # -- supervisor -----------------------------------------------------
        dryrun_case(
            "RED supervisor.running (no process matches the pattern)",
            fixture.variant(
                "pattern-absent",
                lambda doc: doc["supervisor"].update(process_pattern="pattern-that-does-not-exist-xyz"),
            ),
            1,
            "supervisor.running",
        )
        two_process_target = fixture.variant("pattern-two", lambda doc: None)
        extra = fixture.spawn_extra()
        try:
            dryrun_case(
                "RED supervisor.running (two matching processes refused)",
                two_process_target,
                1,
                "supervisor.running",
            )
            record(
                "the two-process probe saw two matching processes",
                len(matching_commands(PATTERN)) == 2,
                f"{len(matching_commands(PATTERN))} match(es) while the second process ran",
            )
        finally:
            Fixture.stop_process(extra)
        time.sleep(0.3)
        record(
            "fixture is observable exactly once again after the probe",
            len(matching_commands(PATTERN)) == 1,
            f"{len(matching_commands(PATTERN))} match(es)",
        )
        dryrun_case(
            "RED supervisor.identity_descriptor (wrong recorded argv hash)",
            fixture.variant(
                "identity-hash",
                lambda doc: doc["supervisor"]["identity"].update(argv_sha256="0" * 64),
            ),
            1,
            "supervisor.identity_descriptor",
        )
        dryrun_case(
            "RED supervisor.identity_record (record missing)",
            fixture.variant(
                "identity-missing",
                lambda doc: doc["supervisor"].update(identity_record=str(fixture.host / "absent-identity.json")),
            ),
            1,
            "supervisor.identity_record",
        )
        bad_record = fixture.host / "cases" / "identity-mismatch.json"
        bad_record.write_text(
            json.dumps({"argv_sha256": "1" * 64, "pid": 1}), encoding="utf-8"
        )
        dryrun_case(
            "RED supervisor.identity_record (record hash mismatch)",
            fixture.variant(
                "identity-record-mismatch",
                lambda doc: doc["supervisor"].update(identity_record=str(bad_record)),
            ),
            1,
            "supervisor.identity_record",
        )

        # -- rollback -------------------------------------------------------
        dryrun_case(
            "RED rollback.commands_syntax (unparsable command)",
            fixture.variant(
                "syntax-error",
                lambda doc: doc["rollback"]["commands"].append(
                    {"id": "RX", "class": "owner-executed", "run": "( unclosed subshell"}
                ),
            ),
            1,
            "rollback.commands_syntax",
        )
        dryrun_case(
            "RED rollback.commands_resolve (command word does not resolve)",
            fixture.variant(
                "unresolved-command",
                lambda doc: doc["rollback"]["commands"].append(
                    {"id": "RX", "class": "sandbox-runnable", "run": "definitely-not-a-real-binary-xyz"}
                ),
            ),
            1,
            "rollback.commands_resolve",
        )
        dryrun_case(
            "RED rollback.binaries (declared binary does not resolve)",
            fixture.variant(
                "unresolved-binary",
                lambda doc: doc["rollback"].update(binaries=["definitely-not-a-real-binary-xyz"]),
            ),
            1,
            "rollback.binaries",
        )
        dryrun_case(
            "RED rollback.inputs (restore input missing)",
            fixture.variant(
                "input-missing",
                lambda doc: doc["rollback"]["inputs"].__setitem__(
                    0, {"path": "{backup_dir}/absent.plist", "sha256": fixture.unit_sha}
                ),
            ),
            1,
            "rollback.inputs",
        )
        dryrun_case(
            "RED rollback.inputs (restore input hash mismatch)",
            fixture.variant(
                "input-hash",
                lambda doc: doc["rollback"]["inputs"].__setitem__(
                    0, {"path": "{backup_dir}/" + fixture.unit.name, "sha256": "2" * 64}
                ),
            ),
            1,
            "rollback.inputs",
        )

        # -- target mechanism ----------------------------------------------
        failing_canter = fixture.bin / "canter-failing"
        failing_canter.write_text(
            "#!/usr/bin/env bash\n"
            "case \"${1:-}\" in\n"
            "    --version) echo \"canter-fixture 0.0.0\"; exit 0 ;;\n"
            "    *) exit 3 ;;\n"
            "esac\n",
            encoding="utf-8",
        )
        failing_canter.chmod(0o755)
        dryrun_case(
            "RED target.probe (probe exits non-zero)",
            fixture.variant(
                "probe-failure",
                lambda doc: doc["canter"].update(binary=str(failing_canter)),
            ),
            1,
            "target.probe[1]",
        )
        dryrun_case(
            "RED target.probe (disallowed argv refused before execution)",
            fixture.variant(
                "probe-disallowed",
                lambda doc: doc["canter"].update(probes=[["queue", "submit"]]),
            ),
            1,
            "target.probe[0]",
        )
        bad_envelope = fixture.bin / "canter-bad-envelope"
        bad_envelope.write_text(
            "#!/usr/bin/env bash\n"
            "case \"${1:-}\" in\n"
            "    --version) echo \"canter-fixture 0.0.0\"; exit 0 ;;\n"
            "    status) printf '%s\\n' '{\"schema\":\"not-an-envelope\"}'; exit 0 ;;\n"
            "    *) exit 2 ;;\n"
            "esac\n",
            encoding="utf-8",
        )
        bad_envelope.chmod(0o755)
        dryrun_case(
            "RED target.probe (--json probe without an hf-output/v1 envelope)",
            fixture.variant(
                "probe-envelope",
                lambda doc: doc["canter"].update(binary=str(bad_envelope)),
            ),
            1,
            "target.probe[1]",
        )

        # -- zero-mutation fingerprint -------------------------------------
        mutation_target = fixture.units / "mutation-probe.plist"
        mutation_target.write_bytes(fixture.unit.read_bytes())
        mutating_canter = fixture.bin / "canter-mutating"
        mutating_canter.write_text(
            "#!/usr/bin/env bash\n"
            "case \"${1:-}\" in\n"
            "    --version) echo \"canter-fixture 0.0.0\"; exit 0 ;;\n"
            "    status) printf '\\n' >>\"$CUTOVER92_MUTATION_TARGET\"; "
            "printf '%s\\n' '{\"schema\":\"hf-output/v1\",\"command\":\"status\",\"data\":{},\"exit_code\":0,\"kind\":\"ok\"}'; exit 0 ;;\n"
            "    *) exit 2 ;;\n"
            "esac\n",
            encoding="utf-8",
        )
        mutating_canter.chmod(0o755)
        os.environ["CUTOVER92_MUTATION_TARGET"] = str(mutation_target)
        dryrun_case(
            "RED nomutation (a writing probe must fail the fingerprint check)",
            fixture.variant(
                "mutating-probe",
                lambda doc: (
                    doc["supervisor"].update(unit_path=str(mutation_target)),
                    doc["canter"].update(binary=str(mutating_canter)),
                ),
            ),
            1,
            "nomutation",
        )
        mutated = mutation_target.read_bytes() != fixture.unit.read_bytes()
        record(
            "the nomutation probe actually mutated its target (fingerprint guard bites)",
            mutated,
            "target file changed" if mutated else "target file unchanged — probe is inert!",
        )

        # -- external zero-mutation proof ----------------------------------
        watched_after = fixture.fingerprint()
        files_after = fixture.file_fingerprint()
        record(
            "dry-run run left the process identity unchanged",
            watched_before == watched_after,
            f"{watched_before[:12]} == {watched_after[:12]}" if watched_before == watched_after else "fingerprint drift",
        )
        record(
            "dry-run runs left every watched fixture file unchanged (external sha256 proof)",
            files_before == files_after,
            f"{files_before[:12]} == {files_after[:12]}" if files_before == files_after else "file drift",
        )

        fixture.stop()
        files_final = fixture.file_fingerprint()
        record(
            "fixture files unchanged after the whole suite (nothing wrote to the fixture)",
            files_final == files_before,
            "stable" if files_final == files_before else "drift",
        )

        rehearsal_guards(tmp)

    failed = [name for name, ok, _detail in RESULTS if not ok]
    print()
    print(
        "SELF-TEST SUMMARY result={} checks={} passed={} failed={} skipped={}".format(
            "ok" if not failed else "failed",
            len(RESULTS),
            len(RESULTS) - len(failed),
            len(failed),
            len(SKIPS),
        )
    )
    for name in failed:
        print(f"FAILED: {name}")
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
