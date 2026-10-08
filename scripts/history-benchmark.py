#!/usr/bin/env python3
"""Measure real read-only CLI/TUI history paths with a private near-10 MiB journal.

Use scripts/p5-check.py for formal measurements: it holds the absolute shared
native-experiment lock until children are reaped. Direct callers must hold that
same lock. This script does not acquire a nested lock. Python is a verification
requirement only; it is not a Bree runtime dependency.
"""
import argparse
import fcntl
import hashlib
import json
import math
import os
import pty
import select
import stat
import statistics
import struct
import subprocess
import termios
import time
from pathlib import Path

from pty_screen import Screen
from check_env import bree_environment

MAX_JOURNAL_BYTES = 10 * 1024 * 1024
MUTABLE_FLAGS = os.O_APPEND | os.O_ASYNC | os.O_SYNC | os.O_DSYNC | os.O_NONBLOCK
UNFINISHED_ID = "run:benchmark-unfinished-latest"
FINISHED_ID = "run:benchmark-finished-latest"


def encode_record(event, data, timestamp):
    return (json.dumps({"schema_version": 1, "timestamp_unix_ms": timestamp,
                        "event": event, "data": data}, separators=(",", ":")) + "\n").encode()


def synthetic_result(run_id, timestamp):
    """Valid historical data with no live process identity or resource claims."""
    missing = {"value": None, "status": "unknown", "source": "p5 synthetic history fixture",
               "reason": "Synthetic fixture; no live resource observation was made"}
    system = {key: dict(missing) for key in ("total_bytes", "used_bytes", "compressed_bytes",
                                           "swap_used_bytes", "cached_bytes", "pressure")}
    system["used_definition"] = "Synthetic fixture; not a measurement of system memory"
    return {"schema_version": 1, "run_id": run_id, "plan_id": "plan:synthetic-history",
            "rule_revision": 0, "started_at_unix_ms": timestamp - 1,
            "finished_at_unix_ms": timestamp, "cancelled": False,
            "targets": [{"group_id": "synthetic:not-a-live-instance", "name": "Synthetic history target",
                         "identity": None, "outcome": "unknown", "request_sent": True,
                         "observed_ms": 0, "reason": "Synthetic prior request; no real application was targeted"}],
            "resource_before": system, "resource_after": None,
            "resource_observation": "Synthetic result; request acceptance and actual exit remain distinct",
            "errors": []}


def run_records(run_id, timestamp, finished=True):
    records = [encode_record("cleanup_started", {"run_id": run_id, "target_count": 1}, timestamp - 2),
               encode_record("target_request_sent", {"run_id": run_id,
                                                     "group_id": "synthetic:not-a-live-instance"}, timestamp - 1)]
    if finished:
        records.append(encode_record("cleanup_finished", synthetic_result(run_id, timestamp), timestamp))
    return b"".join(records)


def create_fixture(root, target_bytes=MAX_JOURNAL_BYTES):
    """Create only a new private directory; never replace an existing fixture."""
    root = Path(root)
    if not root.is_absolute():
        raise ValueError("fixture directory must be absolute")
    timestamp = time.time_ns() // 1_000_000
    tail = (run_records(FINISHED_ID, timestamp - 5)
            + run_records(UNFINISHED_ID, timestamp, finished=False))
    if target_bytes < len(tail) or target_bytes > MAX_JOURNAL_BYTES:
        raise ValueError("fixture size must fit complete records within the 10 MiB limit")
    root.mkdir(mode=0o700)
    journal = root / "journal.jsonl"
    record_count = 0
    run_count = 0
    with journal.open("xb") as stream:
        os.fchmod(stream.fileno(), 0o600)
        while True:
            finished = run_count % 5 != 0
            group = run_records(f"run:benchmark-{run_count:06}", timestamp - 60_000 + run_count,
                                finished=finished)
            if stream.tell() + len(group) + len(tail) > target_bytes:
                break
            stream.write(group)
            record_count += 3 if finished else 2
            run_count += 1
        stream.write(tail)
        stream.flush()
        os.fsync(stream.fileno())
    return {"data_dir": str(root), "journal_bytes": journal.stat().st_size,
            "journal_mib": journal.stat().st_size / 1024 / 1024,
            "record_count": record_count + 5, "run_count": run_count + 2,
            "finished_latest": FINISHED_ID, "unfinished_latest": UNFINISHED_ID,
            "synthetic": True, "contains_live_process_identities": False,
            "maximum_journal_bytes": MAX_JOURNAL_BYTES}


def digest_tree(root):
    """Include names, private modes and bytes; unexpected lock/state files are changes."""
    rows = {}
    for path in sorted(Path(root).rglob("*")):
        mode = path.lstat().st_mode
        row = {"mode": stat.S_IMODE(mode), "kind": "file" if stat.S_ISREG(mode) else "other"}
        if stat.S_ISREG(mode):
            row.update(bytes=path.stat().st_size, sha256=hashlib.sha256(path.read_bytes()).hexdigest())
        rows[str(path.relative_to(root))] = row
    return {"root_mode": stat.S_IMODE(Path(root).lstat().st_mode), "entries": rows}


def terminal_modes(descriptor):
    modes = termios.tcgetattr(descriptor)
    modes[3] &= ~getattr(termios, "PENDIN", 0)
    return modes


class PtySession:
    def __init__(self, binary, data_dir, raw_path):
        self.master, self.slave = pty.openpty()
        self.before = terminal_modes(self.slave)
        self.flags_before = fcntl.fcntl(self.slave, fcntl.F_GETFL) & MUTABLE_FLAGS
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
        self.screen = Screen(140, 40)
        self.raw_path = raw_path
        self.captured = bytearray()
        env = bree_environment(data_dir)
        try:
            self.child = subprocess.Popen([str(binary)], stdin=self.slave, stdout=self.slave,
                                          stderr=self.slave, env=env)
        except Exception:
            os.close(self.master)
            os.close(self.slave)
            raise
        self.commands = []

    def read(self, timeout=.01):
        if select.select([self.master], [], [], timeout)[0]:
            chunk = os.read(self.master, 65536)
            self.captured.extend(chunk)
            self.screen.feed(chunk)
            return bool(chunk)
        return False

    def wait_screen(self, markers, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read()
            if all(marker in self.screen.text() for marker in markers):
                return
            if self.child.poll() is not None:
                raise RuntimeError(f"Bree exited {self.child.returncode} before screen markers {markers}")
        raise RuntimeError(f"Screen timeout waiting for {markers}; current screen:\n{self.screen.text()}")

    def transition(self, keys, markers):
        start = time.perf_counter()
        os.write(self.master, keys)
        self.wait_screen(markers)
        elapsed = (time.perf_counter() - start) * 1000
        self.commands.append({"keys_hex": keys.hex(), "markers": markers, "elapsed_ms": elapsed})
        return elapsed

    def quit(self):
        start = time.perf_counter()
        os.write(self.master, b"q")
        deadline = time.monotonic() + 10
        while self.child.poll() is None and time.monotonic() < deadline:
            self.read()
        if self.child.poll() is None:
            raise RuntimeError("Bree failed to exit on Q")
        self.child.wait(timeout=5)
        result = {"exit_code": self.child.returncode,
                  "exit_after_q_ms": (time.perf_counter() - start) * 1000,
                  "terminal_restored": self.before == terminal_modes(self.slave),
                  "terminal_file_flags_restored": self.flags_before == (fcntl.fcntl(self.slave, fcntl.F_GETFL) & MUTABLE_FLAGS)}
        if result["exit_code"] != 0 or not result["terminal_restored"] or not result["terminal_file_flags_restored"]:
            raise RuntimeError(f"Bree exit or terminal restoration failed: {result}")
        return result

    def close(self):
        try:
            if self.child.poll() is None:
                # Bounded cleanup targets only this script's exact spawned child.
                self.child.terminate()
                deadline = time.monotonic() + 5
                while self.child.poll() is None and time.monotonic() < deadline:
                    # A recorder failure must not interrupt child recovery.
                    # Drain raw bytes only; even an unreadable PTY still reaches
                    # the bounded kill/wait below.
                    try:
                        if select.select([self.master], [], [], .01)[0]:
                            self.captured.extend(os.read(self.master, 65536))
                    except OSError:
                        break
                if self.child.poll() is None:
                    self.child.kill()
                self.child.wait(timeout=5)
        finally:
            try:
                self.raw_path.write_bytes(self.captured)
            finally:
                os.close(self.master)
                os.close(self.slave)


def cli_read(binary, data_dir, evidence, index, limit=50):
    command = [str(binary), "history", "--json", "--limit", str(limit)]
    env = bree_environment(data_dir)
    stdout = evidence / f"cli-{index}.stdout.json"
    stderr = evidence / f"cli-{index}.stderr.txt"
    row = {"command": command, "elapsed_ms": None, "exit_code": None,
           "stdout": str(stdout), "stderr": str(stderr)}
    start = time.perf_counter()
    try:
        child = subprocess.run(command, env=env, capture_output=True, timeout=15)
    except subprocess.TimeoutExpired as error:
        stdout.write_bytes(error.stdout or b"")
        stderr.write_bytes(error.stderr or b"")
        row.update(status="failed", elapsed_ms=(time.perf_counter() - start) * 1000,
                   error="CLI history timed out after 15 seconds", exit_status="unknown")
        return row
    elapsed = (time.perf_counter() - start) * 1000
    stdout.write_bytes(child.stdout)
    stderr.write_bytes(child.stderr)
    row.update(elapsed_ms=elapsed, exit_code=child.returncode)
    if child.returncode != 0:
        row.update(status="failed", error=child.stderr.decode(errors="replace"))
        return row
    try:
        result = json.loads(child.stdout)
    except json.JSONDecodeError as error:
        row.update(status="failed", error=f"Invalid CLI JSON: {error}")
        return row
    records = result.get("records", [])
    if (result.get("schema_version") != 1 or result.get("replay_enabled") is not False
            or len(records) != limit or records[0].get("run_id") != UNFINISHED_ID
            or records[0].get("status") != "unfinished" or records[0].get("result") is not None
            or records[1].get("run_id") != FINISHED_ID or records[1].get("status") != "finished"
            or records[1].get("result") is None):
        row.update(status="failed", error="CLI history did not preserve expected finished/unfinished/no-replay contract")
        return row
    row.update(status="passed", records_returned=len(records), replay_enabled=False, unfinished_without_result=True)
    return row


def tui_read(binary, data_dir, evidence, index):
    session = PtySession(binary, data_dir, evidence / f"tui-{index}.pty")
    try:
        session.wait_screen(["Memory pressure:", "4. History", "Q Quit"])
        load_ms = session.transition(b"4\r", ["History · Recent runs", "50 runs", UNFINISHED_ID, "No replay"])
        open_ms = session.transition(b"\r", ["Run details", UNFINISHED_ID, "has no final result.", "will not replay"])
        back_ms = session.transition(b"\x1b", ["History · Recent runs", "50 runs", "No replay"])
        finished_ms = session.transition(b"j\r", ["Run details", "Application quit outcomes", "System resource observations"])
        second_back_ms = session.transition(b"\x1b", ["History · Recent runs", "50 runs", "No replay"])
        reload_ms = session.transition(b"r\r", ["Run details", "Application quit outcomes", "System resource observations"])
        (evidence / f"tui-{index}.screen.txt").write_text(session.screen.text() + "\n")
        return {"status": "passed", "command": [str(binary)], "entry_keys": "4 Enter", "load_to_render_ms": load_ms,
                "open_unfinished_ms": open_ms, "return_to_list_ms": back_ms,
                "select_and_open_finished_ms": finished_ms, "second_return_to_list_ms": second_back_ms,
                "reload_and_open_finished_ms": reload_ms, "transitions": session.commands,
                "raw_pty": str(session.raw_path), **session.quit()}
    except Exception as error:
        return {"status": "failed", "command": [str(binary)], "error": str(error),
                "transitions": session.commands, "raw_pty": str(session.raw_path)}
    finally:
        session.close()


def corrupted_history(binary, root, evidence):
    root.mkdir(mode=0o700)
    journal = root / "journal.jsonl"
    with journal.open("xb") as stream:
        os.fchmod(stream.fileno(), 0o600)
        # A corrupt oldest line must not be hidden by --limit 1 and a valid newest run.
        stream.write(b'{"schema_version":1,broken}\n')
        stream.write(run_records(UNFINISHED_ID, time.time_ns() // 1_000_000, finished=False))
    before = digest_tree(root)
    command = [str(binary), "history", "--json", "--limit", "1"]
    env = bree_environment(root)
    child = subprocess.run(command, env=env, capture_output=True, timeout=15)
    (evidence / "corrupt-cli.stdout.txt").write_bytes(child.stdout)
    (evidence / "corrupt-cli.stderr.txt").write_bytes(child.stderr)
    if child.returncode == 0 or b"corrupt" not in child.stderr.lower():
        raise RuntimeError(f"CLI failed to reject corrupt history: {child.returncode}, {child.stderr!r}")
    session = PtySession(binary, root, evidence / "corrupt-tui.pty")
    try:
        session.wait_screen(["Memory pressure:", "4. History", "Q Quit"])
        session.transition(b"4\r", ["History · Recent runs", "History read failed:", "actions are not replayed"])
        (evidence / "corrupt-tui.screen.txt").write_text(session.screen.text() + "\n")
        terminal = session.quit()
    finally:
        session.close()
    after = digest_tree(root)
    if before != after:
        raise RuntimeError("Corrupt history was changed or additional data files were created")
    return {"status": "passed", "command": command, "cli_exit_code": child.returncode,
            "cli_rejected_corruption_with_limit_one": True, "tui_read_error_visible": True,
            "files_unchanged": True, "before": before, "after": after, **terminal}


def summarize(rows, field):
    values = [row[field] for row in rows if row.get("status") == "passed" and field in row]
    if not values:
        return {"status": "not-run", "samples": 0, "p95_ms": None, "median_ms": None, "maximum_ms": None}
    return {"status": "measured", "samples": len(values),
            "p95_ms": sorted(values)[math.ceil(len(values) * .95) - 1],
            "median_ms": statistics.median(values), "maximum_ms": max(values)}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/bree"))
    parser.add_argument("--output", type=Path, default=Path(".artifacts/p5/history/history.json"))
    parser.add_argument("--runs", type=int, default=20)
    args = parser.parse_args(argv)
    if args.runs < 20:
        parser.error("at least 20 runs are required for reported P95")
    binary, output = args.binary.resolve(), args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    evidence = output.parent / "history-evidence"
    report = {"status": "failed", "binary": str(binary), "binary_sha256": None, "runs_requested": args.runs,
              "cli": [], "tui": [], "corrupt_history": {"status": "not-run"},
              "errors": [], "response_budget_status": "unknown",
              "limits": ["Synthetic private journal; no real cleanup requests or live identities",
                         "Timings include actual CLI parsing or TUI rendering and PTY transport",
                         "Reload timing includes opening detail because unchanged reload emits no visible frame",
                         "Warm filesystem cache is uncontrolled; this is not a cold-cache claim",
                         "No history response budget is specified by P5; measurements do not invent one",
                         "Private PTY cannot verify physical terminal rendering or user perception",
                         "Read-only evidence is unchanged files plus replay_enabled=false and synthetic identity-free targets; no syscall audit"]}
    try:
        report["binary_sha256"] = hashlib.sha256(binary.read_bytes()).hexdigest()
        evidence.mkdir(mode=0o700)
        root = output.parent / "history-data"
        report["fixture"] = create_fixture(root)
        before = digest_tree(root)
        report["files_before"] = before
        for index in range(args.runs):
            report["cli"].append(cli_read(binary, root, evidence, index))
            if report["cli"][-1]["status"] != "passed":
                raise RuntimeError(report["cli"][-1]["error"])
            report["tui"].append(tui_read(binary, root, evidence, index))
            if report["tui"][-1]["status"] != "passed":
                raise RuntimeError(report["tui"][-1]["error"])
        after = digest_tree(root)
        report["files_after"] = after
        report["files_unchanged"] = before == after
        if before != after:
            raise RuntimeError("History reads changed fixture files or created additional state/lock files")
        report["corrupt_history"] = {"status": "failed", "error": "Corruption check did not complete"}
        report["corrupt_history"] = corrupted_history(binary, output.parent / "history-corrupt-data", evidence)
        report["status"] = "passed"
    except Exception as error:
        report["errors"].append(f"{type(error).__name__}: {error}")
    if "files_before" in report and "files_after" not in report:
        try:
            report["files_after"] = digest_tree(Path(report["fixture"]["data_dir"]))
            report["files_unchanged"] = report["files_before"] == report["files_after"]
        except Exception as error:
            report["files_unchanged"] = None
            report["errors"].append(f"Integrity check is unknown: {error}")
    report["cli_runs_not_run"] = args.runs - len(report["cli"])
    report["tui_runs_not_run"] = args.runs - len(report["tui"])
    report["cli_wall_time"] = summarize(report["cli"], "elapsed_ms")
    report["tui_timings"] = {field: summarize(report["tui"], field) for field in
                             ("load_to_render_ms", "open_unfinished_ms", "return_to_list_ms",
                              "select_and_open_finished_ms", "reload_and_open_finished_ms", "exit_after_q_ms")}
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key not in ("cli", "tui")},
                     ensure_ascii=False, indent=2))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
