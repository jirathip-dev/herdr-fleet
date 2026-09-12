#!/usr/bin/env bash
# cutover-92-rehearsal.sh — disposable-fixture rollback rehearsal for the
# #92 cutover plan (docs/operations/cutover-92.md, section 10).
#
# WHAT IT PROVES: the rollback *path* works. It plants a fixture unit, a
# fixture supervisor process and a fixture descriptor inside a disposable
# sandbox, runs the dry-run against it, performs a simulated cutover INSIDE
# the sandbox (unit replaced, fixture process stopped), executes the plan's
# sandbox-runnable rollback commands for real (byte restore + compare), and
# re-verifies the supervisor identity and the dry-run afterwards. The plan's
# owner-executed service-manager commands are printed as SIMULATED.
#
# WHY IT IS PROVABLY HARMLESS:
#   * every path it touches must resolve under one mktemp -d sandbox root
#     (the `contain` guard refuses anything else; a root outside the OS temp
#     dir is refused up front; an existing root is never reused);
#   * the only process it stops is the fixture process it started itself
#     (pid file + command-line check under the same sandbox root);
#   * the owner-executed service-manager commands are only printed — the
#     script contains no service-manager invocation and names no live label
#     or unit path (the self-test asserts both statically);
#   * executed command text is path-guarded: every absolute path inside a
#     command it runs must be inside the sandbox.
#
# ZERO FIXTURE PROCESSES ON EVERY EXIT PATH (issue #92-R1). Teardown is
# verified, not assumed:
#   * the fixture process is started with a parent watchdog: as soon as the
#     rehearsal process is gone — including SIGKILL, which no trap can catch —
#     the fixture removes its sandbox and exits on its own (bounded by its
#     1 s poll), so it can never outlive the rehearsal;
#   * teardown kills by *path*, not by remembered pid: every process whose
#     command line names this run's sandbox root is ours (a fresh mktemp
#     directory), and they are all stopped with TERM, then KILL after a
#     bounded wait, then the process table is re-scanned and must be empty;
#   * INT/TERM/HUP are trapped and re-raised through the EXIT trap, so the
#     cleanup runs on refusal, error, interrupt and normal exit alike;
#   * the final `REHEARSAL result=...` line reports `fixtures_left=` and the
#     exit status is non-zero if anything survived or the sandbox survived
#     its removal. There is no mode that leaves a fixture process running.
#
# Host class: the macOS/launchd queue host (the fixture mirrors a launchd
# user agent). Requires python3 (stdlib only) and shasum.
#
# Usage:
#   bash scripts/cutover-92-rehearsal.sh
#   bash scripts/cutover-92-rehearsal.sh --dry-run-only     # plant fixture + run the dry-run,
#                                                          # then tear everything down and exit
#                                                          # with the dry-run's raw exit code
#   CUTOVER92_REHEARSAL_ROOT=<temp-dir path> bash scripts/cutover-92-rehearsal.sh
#
# Output: one PASS/FAIL line per verification, the per-step evidence, and a
# final `REHEARSAL result=... steps=... verifications=... failures=...
# sandbox=... live_paths_touched=0 fixtures_left=...` line.
# Exit codes: 0 all verifications passed · 1 a verification failed ·
# 2 usage error or containment refusal.

set -u

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DRY_RUN="$REPO/scripts/cutover-92-dryrun.py"
TMP_BASE="${TMPDIR:-/tmp}"
TMP_BASE="${TMP_BASE%/}"

SANDBOX=""
SANDBOX_OWNED=0
OWNERSHIP_MARKER=".cutover-92-rehearsal-owned"
FIXTURE_PID=""
FIXTURE_PIDS=""
STEPS=0
VERIFICATIONS=0
FAILURES=0
TEARDOWN_DONE=0
TEARDOWN_FAILED=0
DRY_RUN_ONLY=""
DRY_RUN_EXIT=""

usage() {
    echo "usage: cutover-92-rehearsal.sh [--dry-run-only]" >&2
    exit 2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run-only) DRY_RUN_ONLY="yes" ;;
        *) usage ;;
    esac
    shift
done

refuse() {
    echo "REFUSED: $*" >&2
    exit 2
}

fail() {
    FAILURES=$((FAILURES + 1))
    echo "FAIL: $*" >&2
}

ok() {
    VERIFICATIONS=$((VERIFICATIONS + 1))
    echo "PASS: $*"
}

step() {
    STEPS=$((STEPS + 1))
    echo
    echo "==> step $STEPS: $*"
}

contain() {
    for path in "$@"; do
        case "$path" in
            "$SANDBOX"/*) ;;
            *) refuse "path outside the rehearsal sandbox: $path" ;;
        esac
    done
}

# Guard the text of a command before executing it: every absolute path in it
# must live inside the sandbox.
guard_command_paths() {
    for path in $(printf '%s\n' "$1" | grep -o '/[^[:space:]]*' || true); do
        case "$path" in
            "$SANDBOX"/*) ;;
            *) refuse "executed command touches a path outside the sandbox: $path" ;;
        esac
    done
}

fixture_pid_is_ours() {
    [ -n "${FIXTURE_PID:-}" ] || return 1
    COMMAND_LINE="$(ps -ww -p "$FIXTURE_PID" -o command= 2>/dev/null || true)"
    [ -n "$COMMAND_LINE" ] || return 1
    case "$COMMAND_LINE" in
        *"$SANDBOX"*) return 0 ;;
        *) return 1 ;;
    esac
}

# Read-only, informational: processes whose command line names this run's
# sandbox root. This is a report, never a kill list — a process this run did
# not start (for example the operator's own shell) is none of its business.
sandbox_naming_pids() {
    [ -n "${SANDBOX:-}" ] || return 0
    python3 - "$SANDBOX" <<'PY'
import os
import subprocess
import sys

root = sys.argv[1].rstrip("/") + "/"
excluded = {os.getpid(), os.getppid()}
out = subprocess.run(["ps", "-Aww", "-o", "pid=,command="],
                     capture_output=True, text=True).stdout
for line in out.splitlines():
    pid, _sep, command = line.strip().partition(" ")
    if not pid.isdigit() or int(pid) in excluded:
        continue
    if root in command:
        print(pid)
PY
}

# Processes this run started: the fixture instances ($!) plus the reaper.
our_recorded_pids() {
    pids="$FIXTURE_PIDS"
    if [ "${1:-}" = "with-reaper" ] && [ -n "${REAPER_PID:-}" ]; then
        pids="$pids $REAPER_PID"
    fi
    printf '%s\n' "$pids"
}

our_pids_alive() {
    count=0
    for pid in $(our_recorded_pids "$@"); do
        if kill -0 "$pid" 2>/dev/null; then
            count=$((count + 1))
        fi
    done
    printf '%s' "$count"
}

# Stop exactly the pids this run started (never a path scan — that could hit a
# process this run did not create): TERM, bounded wait, KILL, bounded wait.
stop_our_processes() {
    pids="$(our_recorded_pids "$@")"
    for pid in $pids; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in $pids; do
        wait "$pid" 2>/dev/null || true
    done
    tries=0
    while [ "$tries" -lt 100 ]; do
        [ "$(our_pids_alive "$@")" -eq 0 ] && return 0
        sleep 0.1
        tries=$((tries + 1))
    done
    for pid in $pids; do
        kill -9 "$pid" 2>/dev/null || true
    done
    for pid in $pids; do
        wait "$pid" 2>/dev/null || true
    done
    tries=0
    while [ "$tries" -lt 100 ]; do
        [ "$(our_pids_alive "$@")" -eq 0 ] && return 0
        sleep 0.1
        tries=$((tries + 1))
    done
    return 1
}

# The reaper exits by itself once the sandbox it watches is gone; wait for it
# (bounded) and force it down only if it does not.
wait_for_reaper() {
    [ -n "${REAPER_PID:-}" ] || return 0
    tries=0
    while [ "$tries" -lt 30 ]; do
        kill -0 "$REAPER_PID" 2>/dev/null || return 0
        sleep 0.1
        tries=$((tries + 1))
    done
    kill "$REAPER_PID" 2>/dev/null || true
    sleep 0.3
    kill -9 "$REAPER_PID" 2>/dev/null || true
    tries=0
    while [ "$tries" -lt 30 ]; do
        kill -0 "$REAPER_PID" 2>/dev/null || return 1
        sleep 0.1
        tries=$((tries + 1))
    done
    return 1
}

# Idempotent, verified teardown. Nothing this run did not create is ever
# removed: without the ownership marker written at creation time there is
# nothing of ours to clean and the function refuses to touch the path.
teardown() {
    if [ "$TEARDOWN_DONE" -eq 1 ]; then
        return "$TEARDOWN_FAILED"
    fi
    TEARDOWN_DONE=1
    if [ "$SANDBOX_OWNED" -ne 1 ] || [ -z "${SANDBOX:-}" ] || [ ! -f "$SANDBOX/$OWNERSHIP_MARKER" ]; then
        if [ -n "${SANDBOX:-}" ]; then
            echo "NOTE: not cleaning $SANDBOX — this run did not create it (no ownership marker); nothing was deleted" >&2
        fi
        return 0
    fi
    if ! stop_our_processes; then
        echo "FAIL: fixture process(es) started by this run survived teardown: $FIXTURE_PIDS" >&2
        TEARDOWN_FAILED=1
        return 1
    fi
    if [ -n "${CUTOVER92_REHEARSAL_KEEP:-}" ]; then
        if ! stop_our_processes with-reaper; then
            echo "FAIL: the sandbox reaper survived teardown" >&2
            TEARDOWN_FAILED=1
            return 1
        fi
        echo "NOTE: CUTOVER92_REHEARSAL_KEEP is set — sandbox kept at $SANDBOX (no process started by this run is left running)"
        return 0
    fi
    # Remove the sandbox this run created (retried: a partially-emptied tree
    # must never be left behind); the reaper notices and exits on its own.
    tries=0
    while [ "$tries" -lt 5 ]; do
        [ -e "$SANDBOX" ] || break
        rm -rf "$SANDBOX" 2>/dev/null || true
        [ -e "$SANDBOX" ] || break
        sleep 0.2
        tries=$((tries + 1))
    done
    # The reaper must be gone before the rehearsal exits (verified).
    if ! wait_for_reaper; then
        echo "FAIL: the sandbox reaper did not exit after the sandbox was removed" >&2
        TEARDOWN_FAILED=1
        return 1
    fi
    if [ -e "$SANDBOX" ]; then
        echo "FAIL: sandbox directory survived removal: $SANDBOX" >&2
        TEARDOWN_FAILED=1
        return 1
    fi
    return 0
}

cleanup() {
    teardown 2>/dev/null || TEARDOWN_FAILED=1
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# --------------------------------------------------------------------------
# Sandbox root: only a fresh mktemp -d path (or an explicitly named one that
# already looks like it) is accepted.
# --------------------------------------------------------------------------
if [ -n "${CUTOVER92_REHEARSAL_ROOT:-}" ]; then
    operator_root="$CUTOVER92_REHEARSAL_ROOT"
    case "$operator_root" in
        "$TMP_BASE"/cutover-92-rehearsal.*) ;;
        *)
            # SANDBOX stays empty: the exit trap must never touch a path this
            # run did not create (#92-R2).
            SANDBOX=""
            refuse "CUTOVER92_REHEARSAL_ROOT must be a cutover-92-rehearsal.* path under $TMP_BASE (got: $operator_root); nothing was created or removed" ;;
    esac
    if [ -e "$operator_root" ]; then
        SANDBOX=""
        refuse "refusing to reuse an existing path: $operator_root; nothing was created or removed"
    fi
    SANDBOX="$operator_root"
    mkdir -p "$SANDBOX" || { SANDBOX=""; refuse "cannot create $operator_root; nothing was removed"; }
else
    SANDBOX="$(mktemp -d "$TMP_BASE/cutover-92-rehearsal.XXXXXX")" || refuse "mktemp failed"
fi
case "$SANDBOX" in
    "$TMP_BASE"/cutover-92-rehearsal.*) ;;
    *) SANDBOX=""; refuse "sandbox is not under the OS temp dir; nothing was removed" ;;
esac

# Ownership proof (#92-R2): this run writes the marker immediately after it
# creates the sandbox, and *every* removal (this script's teardown and the
# reaper) requires it. A pre-existing directory can never carry the marker, so
# a refused or reused operator path is never deleted.
if ! : >"$SANDBOX/$OWNERSHIP_MARKER" 2>/dev/null; then
    SANDBOX=""
    refuse "cannot write the ownership marker; nothing was removed"
fi
SANDBOX_OWNED=1

if ! command -v python3 >/dev/null 2>&1; then
    refuse "python3 is required (stdlib only)"
fi
if [ ! -f "$DRY_RUN" ]; then
    refuse "dry-run script not found at $DRY_RUN"
fi

# --------------------------------------------------------------------------
# Sandbox reaper (#92-R1). Started as soon as the sandbox root exists, before
# any fixture: if the rehearsal disappears for a reason no trap can catch
# (SIGKILL, host kill), the reaper stops every process still naming the
# sandbox and removes the sandbox root — so the rehearsal can never leave a
# fixture *or* a sandbox behind, on any path.
# --------------------------------------------------------------------------
REAPER="$SANDBOX/reaper.sh"
cat >"$REAPER" <<'EOF'
#!/usr/bin/env bash
# Sandbox reaper: independent of the fixture. On an untrappable death it stops
# the fixture this run started (recorded in its own pid file) and removes the
# sandbox root — but only one carrying this run's ownership marker, so a
# pre-existing operator path is never deleted.
sandbox="${CUTOVER92_REAPER_SANDBOX:-}"
tmp_base="${CUTOVER92_REAPER_TMP_BASE:-/nonexistent}"
marker=".cutover-92-rehearsal-owned"
while :; do
    parent="$(ps -ww -p "${CUTOVER92_REAPER_PARENT_PID:-0}" -o command= 2>/dev/null || true)"
    case "$parent" in
        *"${CUTOVER92_REAPER_PARENT_MATCH:-cutover-92-rehearsal}"*)
            # the rehearsal is alive: keep watching, unless it already removed
            # the sandbox itself (a normal, verified teardown) — then there is
            # nothing left to reap and this process may exit.
            [ -d "$sandbox" ] || exit 0
            sleep 0.3
            continue
            ;;
    esac
    break
done
pid="$(cat "${CUTOVER92_REAPER_PIDFILE:-/nonexistent}" 2>/dev/null || true)"
case "$pid" in
    ''|*[!0-9]*) pid="" ;;
esac
if [ -n "$pid" ]; then
    cmdline="$(ps -ww -p "$pid" -o command= 2>/dev/null || true)"
    case "$cmdline" in
        *"$sandbox"*)
            kill "$pid" 2>/dev/null || true
            sleep 1
            kill -9 "$pid" 2>/dev/null || true
            ;;
    esac
fi
case "$sandbox" in
    "$tmp_base"/cutover-92-*)
        # ownership proof: never remove a path this run did not create
        if [ ! -f "$sandbox/$marker" ]; then
            echo "reaper: refusing to remove $sandbox — no ownership marker (not created by this run)" >&2
            exit 0
        fi
        tries=0
        while [ "$tries" -lt 10 ]; do
            [ -e "$sandbox" ] || break
            rm -rf "$sandbox" 2>/dev/null || true
            [ -e "$sandbox" ] || break
            sleep 0.3
            tries=$((tries + 1))
        done
        ;;
esac
exit 0
EOF
chmod +x "$REAPER"
export CUTOVER92_REAPER_PARENT_PID="$$"
export CUTOVER92_REAPER_PARENT_MATCH="cutover-92-rehearsal"
export CUTOVER92_REAPER_SANDBOX="$SANDBOX"
export CUTOVER92_REAPER_TMP_BASE="$TMP_BASE"
export CUTOVER92_REAPER_PIDFILE="$SANDBOX/host/run/supervisor.pid"  # == "$ROOT/run/supervisor.pid"
"$REAPER" >"$SANDBOX/reaper.log" 2>&1 &
REAPER_PID=$!

ROOT="$SANDBOX/host"
LABEL="com.example.fixture-supervisor"
PATTERN="fixture-supervisor --serve"
UNIT="$ROOT/units/$LABEL.plist"
PROGRAM="$ROOT/bin/fixture-supervisor"
CANTER_BIN="$ROOT/bin/canter"
CONFIG="$ROOT/canter.toml"
IDENTITY="$ROOT/identity.json"

echo "cutover-92 rehearsal — disposable fixture only; sandbox: $SANDBOX"
echo "host class: macOS/launchd; the live supervisor, the live unit and the"
echo "service manager are never referenced, read or executed."

step "plant the fixture host (unit, fixture supervisor, canter stub, descriptor)"
contain "$ROOT" "$UNIT" "$PROGRAM" "$CANTER_BIN" "$CONFIG" "$IDENTITY"
mkdir -p "$ROOT/units" "$ROOT/bin" "$ROOT/backup" "$ROOT/run" "$ROOT/log"

printf '%s\n' \
    '<?xml version="1.0" encoding="UTF-8"?>' \
    '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
    '<plist version="1.0">' \
    '<dict>' \
    "  <key>Label</key><string>$LABEL</string>" \
    '  <key>ProgramArguments</key>' \
    '  <array>' \
    "    <string>$PROGRAM</string>" \
    '    <string>--serve</string>' \
    '  </array>' \
    '</dict>' \
    '</plist>' >"$UNIT"

cat >"$PROGRAM" <<'EOF'
#!/usr/bin/env bash
# Fixture supervisor: keeps its own argv (no exec), records its pid, and
# exits as soon as the rehearsal that started it is gone — a fixture must
# never be able to outlive the rehearsal, not even when the rehearsal is
# SIGKILLed (#92-R1). The parent is identified by its command line (not only
# by pid) so pid reuse cannot keep it alive. The sandbox tree itself is
# removed by exactly one owner at a time: the rehearsal's verified teardown,
# or the separate sandbox reaper on an untrappable death — never both at once.
echo "$$" >"$CUTOVER92_FIXTURE_PIDFILE"
while :; do
    parent="$(ps -ww -p "${CUTOVER92_FIXTURE_PARENT_PID:-0}" -o command= 2>/dev/null || true)"
    case "$parent" in
        *"${CUTOVER92_FIXTURE_PARENT_MATCH:-cutover-92-rehearsal}"*) ;;
        *)
            exit 0
            ;;
    esac
    sleep 1
done
EOF
chmod +x "$PROGRAM"

cat >"$CANTER_BIN" <<'EOF'
#!/usr/bin/env bash
case "${1:-}" in
    --version)
        echo "canter-fixture 0.0.0 (rehearsal stub)"
        exit 0
        ;;
    status)
        echo '{"schema":"hf-output/v1","command":"status","data":{"queue":"fixture"},"exit_code":0,"kind":"ok"}'
        exit 0
        ;;
    *)
        echo "canter-fixture: unsupported probe: ${1:-}" >&2
        exit 2
        ;;
esac
EOF
chmod +x "$CANTER_BIN"
printf '%s\n' '[daemon]' 'enabled = false' >"$CONFIG"

export CUTOVER92_FIXTURE_PIDFILE="$ROOT/run/supervisor.pid"
export CUTOVER92_FIXTURE_PARENT_PID="$$"
export CUTOVER92_FIXTURE_PARENT_MATCH="cutover-92-rehearsal"

start_fixture() {
    contain "$ROOT/run/supervisor.pid"
    rm -f "$ROOT/run/supervisor.pid"
    "$PROGRAM" --serve >>"$ROOT/log/fixture-supervisor.log" 2>&1 &
    FIXTURE_JOB_PID=$!
    FIXTURE_PIDS="$FIXTURE_PIDS $FIXTURE_JOB_PID"
    tries=0
    while [ ! -s "$ROOT/run/supervisor.pid" ] && [ "$tries" -lt 100 ]; do
        sleep 0.1
        tries=$((tries + 1))
    done
    sleep 0.3
    FIXTURE_PID="$(cat "$ROOT/run/supervisor.pid" 2>/dev/null || true)"
}

start_fixture
if ! fixture_pid_is_ours; then
    refuse "fixture supervisor did not start (pid: ${FIXTURE_PID:-none})"
fi
echo "    fixture supervisor pid $FIXTURE_PID running under $PROGRAM"

step "write the fixture descriptor + identity record from the running fixture"
CUTOVER92_PATTERN="$PATTERN" python3 - "$ROOT" "$LABEL" "$UNIT" "$PROGRAM" "$CANTER_BIN" "$CONFIG" "$IDENTITY" <<'PY'
import hashlib, json, os, shutil, subprocess, sys

root, label, unit, program, binary, config, identity = sys.argv[1:8]
pattern = os.environ["CUTOVER92_PATTERN"]
backup_dir = os.path.join(root, "backup")

with open(unit, "rb") as handle:
    unit_bytes = handle.read()
unit_sha = hashlib.sha256(unit_bytes).hexdigest()
backup_unit = os.path.join(backup_dir, os.path.basename(unit))
with open(backup_unit, "wb") as handle:      # the plan's S1 backup copy
    handle.write(unit_bytes)

out = subprocess.run(["ps", "-Aww", "-o", "pid=,command="],
                     capture_output=True, text=True).stdout
matches = []
for line in out.splitlines():
    stripped = line.strip()
    if pattern not in stripped:
        continue
    pid, _sep, command = stripped.partition(" ")
    matches.append((pid, " ".join(command.split())))
assert len(matches) == 1, f"fixture identity is not unique: {matches}"
pid, command = matches[0]
argv_sha = hashlib.sha256(command.encode("utf-8")).hexdigest()

with open(identity, "w", encoding="utf-8") as handle:
    json.dump({
        "schema": "cutover-identity/v1",
        "pid": int(pid),
        "argv_sha256": argv_sha,
        "unit_sha256": unit_sha,
        "count": 1,
    }, handle, indent=2)

descriptor = {
    "schema": "cutover-target/v1",
    "queue_key": "example-queue",
    "host_role": "example-host-role",
    "supervisor": {
        "label": label,
        "unit_path": unit,
        "program": program,
        "process_pattern": pattern,
        "identity": {"argv_sha256": argv_sha},
        "identity_record": identity,
    },
    "rollback": {
        "backup_dir": backup_dir,
        "binaries": ["cp", "cmp"] + (["launchctl"] if shutil.which("launchctl") else []),
        "inputs": [
            {"path": "{backup_dir}/" + os.path.basename(unit), "sha256": unit_sha}
        ],
        "commands": [
            {"id": "R1", "class": "sandbox-runnable",
             "run": "cp -p {backup_dir}/" + os.path.basename(unit) + " {unit_path}"},
            {"id": "R2", "class": "sandbox-runnable",
             "run": "cmp -s {backup_dir}/" + os.path.basename(unit) + " {unit_path}"},
            {"id": "R3", "class": "owner-executed",
             "run": "launchctl bootstrap gui/{uid} {unit_path}"},
            {"id": "R4", "class": "owner-executed",
             "run": "launchctl enable gui/{uid}/{supervisor_label}"},
            {"id": "R5", "class": "owner-executed",
             "run": "launchctl kickstart -k gui/{uid}/{supervisor_label}"},
        ],
    },
    "canter": {
        "binary": binary,
        "config": config,
        "probes": [["--version"], ["status", "--json"]],
    },
}
with open(os.path.join(root, "target.json"), "w", encoding="utf-8") as handle:
    json.dump(descriptor, handle, indent=2)

print(f"    fixture unit sha256={unit_sha[:12]} identity argv sha256={argv_sha[:12]}")
PY
DESCRIPTOR_RC=$?
if [ "$DESCRIPTOR_RC" -ne 0 ]; then
    refuse "fixture descriptor generation failed (exit $DESCRIPTOR_RC)"
fi

TARGET="$ROOT/target.json"
contain "$TARGET"

step "dry-run against the fixture host (preconditions must hold)"
python3 "$DRY_RUN" --target "$TARGET" >"$ROOT/log/dryrun-before.log" 2>&1
BEFORE_RC=$?
DRY_RUN_EXIT="$BEFORE_RC"
if [ -n "$DRY_RUN_ONLY" ]; then
    sed 's/^/    /' "$ROOT/log/dryrun-before.log"
else
    tail -n 4 "$ROOT/log/dryrun-before.log" | sed 's/^/    /'
fi
echo "    RUN: python3 $DRY_RUN --target $TARGET"
echo "    dry-run raw exit: $BEFORE_RC"
if [ -n "$DRY_RUN_ONLY" ]; then
    echo "    (--dry-run-only: the dry-run invocation above is the documented fixture invocation;"
    echo "     the simulated cutover, rollback and post-rollback steps are skipped)"
fi
if [ "$BEFORE_RC" -ne 0 ]; then
    fail "dry-run failed against the intact fixture (exit $BEFORE_RC)"
else
    ok "dry-run exit 0 against the intact fixture"
fi
if grep -q '^SUMMARY result=ok ' "$ROOT/log/dryrun-before.log" && \
   grep -q 'mutations=0' "$ROOT/log/dryrun-before.log"; then
    ok "dry-run summary reports result=ok and mutations=0"
else
    fail "dry-run summary missing or not ok"
fi

if [ -n "$DRY_RUN_ONLY" ]; then
    echo "    mode dry-run-only: simulated cutover, rollback and post-rollback steps skipped"
else

step "record the baseline (unit bytes + running identity)"
BASE_UNIT_SHA="$(shasum -a 256 "$UNIT" | awk '{print $1}')"
BASE_IDENTITY_SHA="$(CUTOVER92_PATTERN="$PATTERN" python3 -c '
import hashlib, os, subprocess
pattern = os.environ["CUTOVER92_PATTERN"]
out = subprocess.run(["ps", "-Aww", "-o", "pid=,command="], capture_output=True, text=True).stdout
matches = [" ".join(line.strip().partition(" ")[2].split()) for line in out.splitlines() if pattern in line]
print(hashlib.sha256(matches[0].encode()).hexdigest() if len(matches) == 1 else "")
')"
echo "    baseline unit sha256=$BASE_UNIT_SHA"
echo "    baseline identity argv sha256=$BASE_IDENTITY_SHA"
if [ -n "$BASE_IDENTITY_SHA" ] && [ "$BASE_IDENTITY_SHA" = "$(python3 -c '
import json, sys
print(json.load(open(sys.argv[1]))["argv_sha256"])
' "$IDENTITY")" ]; then
    ok "running identity matches the recorded identity before any change"
else
    fail "running identity does not match the recorded identity"
fi

step "simulated cutover INSIDE the sandbox (unit replaced, fixture stopped)"
printf '%s\n' \
    '<?xml version="1.0" encoding="UTF-8"?>' \
    '<plist version="1.0">' \
    '<dict>' \
    "  <key>Label</key><string>$LABEL</string>" \
    '  <key>ProgramArguments</key>' \
    '  <array>' \
    "    <string>$ROOT/bin/canter-owned-placeholder</string>" \
    '  </array>' \
    '</dict>' \
    '</plist>' >"$UNIT"
if ! stop_our_processes; then
    fail "could not stop the fixture supervisor this run started"
fi
python3 "$DRY_RUN" --target "$TARGET" >"$ROOT/log/dryrun-during-cutover.log" 2>&1
DURING_RC=$?
if [ "$DURING_RC" -eq 0 ]; then
    fail "dry-run stayed green during the simulated cutover (it must bite)"
else
    ok "dry-run fails while the supervisor is down and the unit is replaced (exit $DURING_RC)"
fi
if grep -q 'FAIL: supervisor.running' "$ROOT/log/dryrun-during-cutover.log"; then
    ok "the failing check is named: supervisor.running"
else
    fail "the simulated cutover did not surface supervisor.running"
fi

step "execute the plan's rollback commands from the dry-run's own substitution"
python3 "$DRY_RUN" --target "$TARGET" --print-rollback >"$ROOT/log/rollback-commands.txt" 2>"$ROOT/log/rollback-print.err"
PRINT_RC=$?
if [ "$PRINT_RC" -ne 0 ]; then
    fail "--print-rollback exited $PRINT_RC"
fi
EXECUTED=0
SIMULATED=0
while IFS="$(printf '\t')" read -r RID RCLASS RCOMMAND; do
    [ -n "${RID:-}" ] || continue
    case "$RCLASS" in
        sandbox-runnable)
            guard_command_paths "$RCOMMAND"
            echo "    executing [$RID] (sandbox): $RCOMMAND"
            if bash -u -c "$RCOMMAND"; then
                EXECUTED=$((EXECUTED + 1))
            else
                fail "[$RID] exited non-zero"
            fi
            ;;
        owner-executed)
            echo "    SIMULATED (owner-executed on host, NOT run here) [$RID]: $RCOMMAND"
            SIMULATED=$((SIMULATED + 1))
            ;;
        *)
            fail "[$RID] unknown command class: $RCLASS"
            ;;
    esac
done <"$ROOT/log/rollback-commands.txt"
echo "    rollback commands: $EXECUTED executed in the sandbox, $SIMULATED simulated"
if [ "$EXECUTED" -ge 2 ] && [ "$SIMULATED" -ge 1 ]; then
    ok "rollback path exercised: $EXECUTED executed, $SIMULATED simulated"
else
    fail "rollback command classes missing (executed=$EXECUTED simulated=$SIMULATED)"
fi

step "verify the restore (bytes) and restart the fixture supervisor (R4/R5 shape)"
RESTORED_SHA="$(shasum -a 256 "$UNIT" | awk '{print $1}')"
if [ "$RESTORED_SHA" = "$BASE_UNIT_SHA" ]; then
    ok "restored unit bytes are byte-identical (sha256 $RESTORED_SHA)"
else
    fail "restored unit differs: $RESTORED_SHA vs $BASE_UNIT_SHA"
fi
start_fixture
if ! fixture_pid_is_ours; then
    fail "restarted fixture supervisor did not start (pid: ${FIXTURE_PID:-none})"
fi
RESTORED_IDENTITY_SHA="$(CUTOVER92_PATTERN="$PATTERN" python3 -c '
import hashlib, os, subprocess
pattern = os.environ["CUTOVER92_PATTERN"]
out = subprocess.run(["ps", "-Aww", "-o", "pid=,command="], capture_output=True, text=True).stdout
matches = [" ".join(line.strip().partition(" ")[2].split()) for line in out.splitlines() if pattern in line]
print(hashlib.sha256(matches[0].encode()).hexdigest() if len(matches) == 1 else "NONE:%d" % len(matches))
')"
if [ "$RESTORED_IDENTITY_SHA" = "$BASE_IDENTITY_SHA" ]; then
    ok "post-restart supervisor identity matches the baseline (argv sha256 $RESTORED_IDENTITY_SHA)"
else
    fail "post-restart identity differs: $RESTORED_IDENTITY_SHA vs $BASE_IDENTITY_SHA"
fi

step "post-rollback dry-run (plan step R6 must pass again)"
python3 "$DRY_RUN" --target "$TARGET" >"$ROOT/log/dryrun-after.log" 2>&1
AFTER_RC=$?
tail -n 2 "$ROOT/log/dryrun-after.log" | sed 's/^/    /'
if [ "$AFTER_RC" -eq 0 ]; then
    ok "dry-run exit 0 after the rollback (preconditions hold again)"
else
    fail "dry-run failed after the rollback (exit $AFTER_RC)"
fi

fi

# --------------------------------------------------------------------------
# Teardown: explicit, verified, and the last thing the summary reports.
# --------------------------------------------------------------------------
step "teardown: stop every fixture process and verify the sandbox is gone"
if teardown; then
    ok "no fixture process references the sandbox and the sandbox is removed"
else
    fail "teardown left fixture process(es) or the sandbox behind"
fi
FIXTURES_LEFT="$(our_pids_alive)"
OTHER_NAMING="$(sandbox_naming_pids | tr '\n' ' ' | sed 's/ *$//')"
if [ -n "$OTHER_NAMING" ]; then
    echo "NOTE: process(es) naming the (removed) sandbox root were not started by this run: $OTHER_NAMING" >&2
fi
if [ -e "$SANDBOX" ]; then
    SANDBOX_REMOVED="no"
else
    SANDBOX_REMOVED="yes"
fi
if [ -n "$DRY_RUN_ONLY" ]; then
    DRY_RUN_MODE="dry-run-only"
    if [ "${DRY_RUN_EXIT:-1}" -ne 0 ]; then
        fail "dry-run exited ${DRY_RUN_EXIT} (mode dry-run-only)"
    fi
else
    DRY_RUN_MODE="full"
fi

FAILED=0
[ "$FAILURES" -eq 0 ] || FAILED=1
[ "$TEARDOWN_FAILED" -eq 0 ] || FAILED=1
[ "$FIXTURES_LEFT" -eq 0 ] || FAILED=1
RESULT="ok"
if [ "$FAILED" -eq 1 ]; then
    RESULT="failed"
fi

echo
echo "REHEARSAL result=$RESULT steps=$STEPS verifications=$VERIFICATIONS failures=$FAILURES sandbox=$SANDBOX live_paths_touched=0 fixtures_left=$FIXTURES_LEFT sandbox_removed=$SANDBOX_REMOVED mode=$DRY_RUN_MODE dry_run_exit=${DRY_RUN_EXIT:-none}"
if [ "$FAILED" -eq 0 ]; then
    exit 0
fi
exit 1
