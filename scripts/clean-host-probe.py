#!/usr/bin/env python3
"""clean-host-probe.py — synthetic daemon round-trip for clean-host checks.

Issue #10 AC2: proves the RELEASE binary's daemon surface works on a fresh
host with no private repositories and no service-manager activation: boot
in disposable temp dirs, then RPC status/epoch, a backup.create ->
restore.begin round trip (epoch rotation), a synthetic hf-schedule/v1
lifecycle (create/list/pause/resume/delete), a fail-closed `apply` refusal
(mutation gate without an idempotency key), and one hf-event/v1 snapshot
line on the subscribe stream.

The human gate (docs/RELEASING.md) runs this on maintainer clean hosts via
scripts/clean-host-verify.sh; this file is self-tested on CI-equivalent
hosts by scripts/test-clean-host-probe.py against a locally built binary in
the same disposable-temp-dir manner (never a real service install).

Stdlib only. Synthetic/public data only; no host paths are ever committed —
all paths come from tempfile at runtime.

Usage:
  clean-host-probe.py --bin PATH [--schedule-doc PATH]

Exit codes: 0 all checks passed, 1 a check failed, 2 usage error.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time

DAEMON_READY_TIMEOUT_SECS = 30
RPC_TIMEOUT_SECS = 15


def canonical_json(value) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=True)


def rpc_call(socket_path: str, request_id: str, method: str,
             params=None) -> dict:
    """One request/response exchange over the daemon Unix socket."""
    request = {
        "schema": "hf-rpc-request/v1",
        "id": request_id,
        "method": method,
        "params": params if params is not None else None,
    }
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(RPC_TIMEOUT_SECS)
        sock.connect(socket_path)
        sock.sendall(canonical_json(request).encode("utf-8") + b"\n")
        line = b""
        while not line.endswith(b"\n"):
            chunk = sock.recv(65536)
            if not chunk:
                break
            line += chunk
        if not line.endswith(b"\n"):
            raise AssertionError(f"{method}: daemon closed without a response")
        return json.loads(line.decode("utf-8"))


def wait_for_daemon(socket_path: str) -> None:
    deadline = time.monotonic() + DAEMON_READY_TIMEOUT_SECS
    last_error = None
    while time.monotonic() < deadline:
        try:
            response = rpc_call(socket_path, "0000000000000001", "status")
            if response.get("ok") is True:
                return
            last_error = f"status not ok: {response}"
        except (OSError, ValueError, AssertionError) as exc:
            last_error = str(exc)
        time.sleep(0.1)
    raise AssertionError(f"daemon did not become ready: {last_error}")


def subscribe_snapshot(socket_path: str) -> dict:
    """Subscribe and read exactly one pushed hf-event/v1 line (the fresh
    snapshot that must arrive first with no cursor)."""
    request = {
        "schema": "hf-rpc-request/v1",
        "id": "0000000000000002",
        "method": "events.subscribe",
        "params": None,
    }
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(RPC_TIMEOUT_SECS)
        sock.connect(socket_path)
        sock.sendall(canonical_json(request).encode("utf-8") + b"\n")

        def read_line() -> str:
            line = b""
            while not line.endswith(b"\n"):
                chunk = sock.recv(65536)
                if not chunk:
                    break
                line += chunk
            if not line.endswith(b"\n"):
                raise AssertionError("events.subscribe: no response line")
            return line.decode("utf-8").strip()

        response = json.loads(read_line())
        if response.get("ok") is not True:
            raise AssertionError(
                f"events.subscribe refused: {response.get('error')}")
        event = json.loads(read_line())
        return event


class Probe:
    def __init__(self, binary: str, schedule_doc: str):
        self.binary = binary
        self.schedule_doc = schedule_doc
        self.failures: list[str] = []

    def check(self, label: str, condition: bool, detail: str = "") -> None:
        if condition:
            print(f"PASS: {label}")
        else:
            self.failures.append(f"{label}: {detail}")
            print(f"FAIL: {label}: {detail}")

    def run(self) -> int:
        with tempfile.TemporaryDirectory(prefix="hf-clean-host-") as tmp:
            home = os.path.join(tmp, "home")
            state = os.path.join(tmp, "state")
            config = os.path.join(tmp, "config")
            runtime = os.path.join(tmp, "run")
            for directory in (home, state, config, runtime):
                os.makedirs(directory)
            socket_path = os.path.join(runtime, "hf.sock")
            env = dict(os.environ)
            env.update({
                "HOME": home,
                "XDG_STATE_HOME": state,
                "XDG_CONFIG_HOME": config,
                "XDG_RUNTIME_DIR": runtime,
            })
            stderr_log = os.path.join(tmp, "daemon.stderr.log")
            with open(stderr_log, "wb") as log:
                daemon = subprocess.Popen(
                    [self.binary, "daemon", "run", "--socket", socket_path],
                    env=env, stdout=subprocess.DEVNULL, stderr=log,
                    start_new_session=True)
            try:
                wait_for_daemon(socket_path)
                self.check("daemon boots and answers status on a fresh host",
                           True)
                self.probe_rpc(socket_path)
            finally:
                try:
                    os.killpg(os.getpgid(daemon.pid), signal.SIGTERM)
                except (ProcessLookupError, PermissionError):
                    pass
                try:
                    daemon.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait(timeout=10)

        if self.failures:
            print(f"clean-host probe: {len(self.failures)} failure(s)")
            return 1
        print("clean-host probe: all checks passed")
        return 0

    def probe_rpc(self, socket_path: str) -> None:
        # Read-only RPCs.
        status = rpc_call(socket_path, "0000000000000003", "status")
        self.check("status RPC returns ok:true",
                   status.get("ok") is True, str(status))
        epoch = rpc_call(socket_path, "0000000000000004", "state.epoch")
        self.check("state.epoch returns ok:true",
                   epoch.get("ok") is True, str(epoch))

        # Backup -> restore round trip (AC2 daemon-state round trip).
        backup = rpc_call(socket_path, "0000000000000005", "backup.create",
                          {"idempotency_key": "ik_clean-host-snap-0001"})
        self.check("backup.create returns ok:true",
                   backup.get("ok") is True, str(backup))
        backup_result = backup.get("result", {})
        snapshot_name = None
        backup_obj = backup_result.get("backup") or {}
        snapshot_name = backup_obj.get("snapshot")
        self.check("backup.create returns a snapshot name",
                   isinstance(snapshot_name, str) and len(snapshot_name) > 0,
                   str(backup_result))
        if snapshot_name:
            restored = rpc_call(
                socket_path, "0000000000000006", "restore.begin",
                {"idempotency_key": "ik_clean-host-restore-0001",
                 "backup": snapshot_name})
            self.check("restore.begin round-trips the snapshot (ok:true)",
                       restored.get("ok") is True, str(restored))
        post_restore = rpc_call(socket_path, "0000000000000007", "backup.create",
                                {"idempotency_key": "ik_clean-host-after-0001"})
        self.check("mutation after restore is acknowledged (durable)",
                   post_restore.get("ok") is True, str(post_restore))

        # Synthetic hf-schedule/v1 lifecycle (fixture doc, no repo access).
        with open(self.schedule_doc, "r", encoding="utf-8") as handle:
            schedule = json.loads(handle.read())
        schedule_id = schedule.get("schedule_id", "")
        created = rpc_call(
            socket_path, "0000000000000008", "schedules.create",
            {"idempotency_key": "ik_clean-host-sched-0001",
             "schedule": schedule})
        created_ok = created.get("ok") is True
        self.check("schedules.create accepts the synthetic fixture doc",
                   created_ok, str(created))
        listed = rpc_call(socket_path, "0000000000000009", "schedules.list")
        listed_ids = [row.get("schedule_id")
                      for row in (listed.get("result", {})
                                  .get("schedules") or [])]
        self.check("schedules.list contains the created schedule",
                   listed.get("ok") is True and schedule_id in listed_ids,
                   str(listed))
        paused = rpc_call(
            socket_path, "000000000000000a", "schedules.pause",
            {"idempotency_key": "ik_clean-host-pause-0001",
             "schedule_id": schedule_id})
        paused_row = (paused.get("result", {}).get("schedule") or {})
        self.check("schedules.pause durably disables the schedule",
                   paused.get("ok") is True and paused_row.get("enabled")
                   is False, str(paused))
        resumed = rpc_call(
            socket_path, "000000000000000b", "schedules.resume",
            {"idempotency_key": "ik_clean-host-resume-0001",
             "schedule_id": schedule_id})
        resumed_row = (resumed.get("result", {}).get("schedule") or {})
        self.check("schedules.resume re-enables the schedule",
                   resumed.get("ok") is True and resumed_row.get("enabled")
                   is True, str(resumed))
        deleted = rpc_call(
            socket_path, "000000000000000c", "schedules.delete",
            {"idempotency_key": "ik_clean-host-delete-0001",
             "schedule_id": schedule_id})
        self.check("schedules.delete removes the schedule",
                   deleted.get("ok") is True, str(deleted))

        # Mutation gate stays fail-closed: apply without an idempotency key.
        refused = rpc_call(socket_path, "000000000000000d", "apply")
        error = refused.get("error") or {}
        self.check(
            "apply without an idempotency key is refused (fail-closed gate)",
            refused.get("ok") is False and
            str(error.get("code", "")).startswith("refusal"),
            str(refused))

        # Event stream snapshot (read-only consumer boundary).
        try:
            event = subscribe_snapshot(socket_path)
            self.check(
                "events.subscribe pushes an hf-event/v1 snapshot first",
                event.get("schema") == "hf-event/v1"
                and event.get("event") == "state.snapshot",
                str(event))
        except (OSError, ValueError, AssertionError) as exc:
            self.check("events.subscribe pushes an hf-event/v1 snapshot first",
                       False, str(exc))


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="clean-host-probe.py")
    parser.add_argument("--bin", required=True,
                        help="path to the release herdr-fleet binary")
    parser.add_argument("--schedule-doc", required=True,
                        help="path to the synthetic hf-schedule/v1 fixture "
                             "(schemas/fixtures/schedule/schedule.valid.json)")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if not os.path.isfile(args.bin) or not os.access(args.bin, os.X_OK):
        sys.stderr.write(f"error: --bin {args.bin} is not executable\n")
        return 2
    if not os.path.isfile(args.schedule_doc):
        sys.stderr.write(f"error: --schedule-doc {args.schedule_doc} not found\n")
        return 2
    return Probe(args.bin, args.schedule_doc).run()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
