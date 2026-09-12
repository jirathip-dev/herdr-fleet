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
# Host class: the macOS/launchd queue host (the fixture mirrors a launchd
# user agent). Requires python3 (stdlib only) and shasum.
#
# Usage:
#   bash scripts/cutover-92-rehearsal.sh
#   CUTOVER92_REHEARSAL_ROOT=<temp-dir path> bash scripts/cutover-92-rehearsal.sh
#
# Output: one PASS/FAIL line per verification, the per-step evidence, and a
# final `REHEARSAL result=... steps=... verifications=... failures=...
# sandbox=... live_paths_touched=0` line.
# Exit codes: 0 all verifications passed · 1 a verification failed ·
# 2 usage error or containment refusal.

set -u

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DRY_RUN="$REPO/scripts/cutover-92-dryrun.py"
TMP_BASE="${TMPDIR:-/tmp}"
TMP_BASE="${TMP_BASE%/}"

SANDBOX=""
FIXTURE_PID=""
STEPS=0
VERIFICATIONS=0
FAILURES=0

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

stop_fixture() {
    if fixture_pid_is_ours; then
        kill "$FIXTURE_PID" 2>/dev/null || true
        wait "$FIXTURE_PID" 2>/dev/null || true
        tries=0
        while [ "$tries" -lt 50 ]; do
            fixture_pid_is_ours || return 0
            sleep 0.1
            tries=$((tries + 1))
        done
        return 1
    fi
    return 0
}

cleanup() {
    if [ -n "${SANDBOX:-}" ] && [ -d "$SANDBOX" ]; then
        if [ -n "${CUTOVER92_REHEARSAL_KEEP:-}" ]; then
            echo "NOTE: CUTOVER92_REHEARSAL_KEEP is set — sandbox kept at $SANDBOX"
            echo "      fixture supervisor left running (pid ${FIXTURE_PID:-none}); to remove:"
            echo "      kill ${FIXTURE_PID:-<pid>} 2>/dev/null; rm -rf $SANDBOX"
        else
            stop_fixture 2>/dev/null || true
            rm -rf "$SANDBOX"
        fi
    fi
}
trap cleanup EXIT

# --------------------------------------------------------------------------
# Sandbox root: only a fresh mktemp -d path (or an explicitly named one that
# already looks like it) is accepted.
# --------------------------------------------------------------------------
if [ -n "${CUTOVER92_REHEARSAL_ROOT:-}" ]; then
    SANDBOX="$CUTOVER92_REHEARSAL_ROOT"
    case "$SANDBOX" in
        "$TMP_BASE"/cutover-92-rehearsal.*) ;;
        *) refuse "CUTOVER92_REHEARSAL_ROOT must be a cutover-92-rehearsal.* path under $TMP_BASE (got: $SANDBOX)" ;;
    esac
    if [ -e "$SANDBOX" ]; then
        refuse "refusing to reuse an existing path: $SANDBOX"
    fi
    mkdir -p "$SANDBOX" || refuse "cannot create $SANDBOX"
else
    SANDBOX="$(mktemp -d "$TMP_BASE/cutover-92-rehearsal.XXXXXX")" || refuse "mktemp failed"
fi
case "$SANDBOX" in
    "$TMP_BASE"/cutover-92-rehearsal.*) ;;
    *) refuse "sandbox is not under the OS temp dir: $SANDBOX" ;;
esac

if ! command -v python3 >/dev/null 2>&1; then
    refuse "python3 is required (stdlib only)"
fi
if [ ! -f "$DRY_RUN" ]; then
    refuse "dry-run script not found at $DRY_RUN"
fi

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
# Fixture supervisor: keeps its own argv (no exec) and records its pid.
echo "$$" >"$CUTOVER92_FIXTURE_PIDFILE"
while :; do sleep 1; done
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
"$PROGRAM" --serve >"$ROOT/log/fixture-supervisor.log" 2>&1 &
tries=0
while [ ! -s "$ROOT/run/supervisor.pid" ] && [ "$tries" -lt 100 ]; do
    sleep 0.1
    tries=$((tries + 1))
done
FIXTURE_PID="$(cat "$ROOT/run/supervisor.pid" 2>/dev/null || true)"
contain "$ROOT/run/supervisor.pid"
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
tail -n 4 "$ROOT/log/dryrun-before.log" | sed 's/^/    /'
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
if ! stop_fixture; then
    fail "could not stop the fixture supervisor"
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
contain "$ROOT/run/supervisor.pid"
rm -f "$ROOT/run/supervisor.pid"
"$PROGRAM" --serve >>"$ROOT/log/fixture-supervisor.log" 2>&1 &
tries=0
while [ ! -s "$ROOT/run/supervisor.pid" ] && [ "$tries" -lt 100 ]; do
    sleep 0.1
    tries=$((tries + 1))
done
sleep 0.5
FIXTURE_PID="$(cat "$ROOT/run/supervisor.pid" 2>/dev/null || true)"
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

echo
if [ "$FAILURES" -ne 0 ]; then
    echo "REHEARSAL result=failed steps=$STEPS verifications=$VERIFICATIONS failures=$FAILURES sandbox=$SANDBOX live_paths_touched=0 sandbox_kept=${CUTOVER92_REHEARSAL_KEEP:+yes}"
    exit 1
fi
echo "REHEARSAL result=ok steps=$STEPS verifications=$VERIFICATIONS failures=0 sandbox=$SANDBOX live_paths_touched=0 sandbox_kept=${CUTOVER92_REHEARSAL_KEEP:+yes}"
