#!/usr/bin/env python3
"""Build and run Bree's existing acceptance tools against one identified build.

Python is a development dependency only. Results and private live-process output
belong in an ignored, new output directory. The caller supplies the shared
absolute experiment lock; it is never replaced or unlinked.
"""
import argparse
import contextlib
import datetime
import fcntl
import hashlib
import json
import os
import platform
import signal
import subprocess
import sys
import time
from pathlib import Path

from check_env import bree_environment

ROOT = Path(__file__).resolve().parent.parent
CHECKS = ("fmt", "clippy", "rust-tests", "script-tests", "build", "doctor",
          "benchmark", "terminal", "theme", "signal", "soak", "visual")


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def digest(path):
    with path.open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def capture(command, env=None):
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, env=env)
    return {"command": command, "exit_code": result.returncode,
            "stdout": result.stdout.strip(), "stderr": result.stderr.strip()}


def source_manifest():
    files = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=ROOT
    ).decode().split("\0")
    hashes = {name: digest(ROOT / name) if (ROOT / name).is_file() else None
              for name in sorted(set(files)) if name}
    encoded = json.dumps(hashes, sort_keys=True).encode()
    return {"sha256": hashlib.sha256(encoded).hexdigest(), "files": hashes}


@contextlib.contextmanager
def experiment_lock(path, events):
    if not path.is_absolute():
        raise ValueError("The shared experiment lock must be an absolute path")
    # Creation is permitted for the lock only. Never truncate or replace it.
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    acquired = False
    waiting = time.monotonic()
    next_message = waiting
    events.append({"event": "lock_wait", "time": utc_now(), "path": str(path)})
    try:
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                acquired = True
                break
            except BlockingIOError:
                if time.monotonic() >= next_message:
                    print("Waiting for the shared experiment lock...", flush=True)
                    next_message = time.monotonic() + 30
                time.sleep(.2)
        events.append({"event": "lock_acquired", "time": utc_now(),
                       "wait_seconds": time.monotonic() - waiting})
        yield fd
    finally:
        events.append({"event": "lock_released" if acquired else "lock_wait_cancelled", "time": utc_now()})
        os.close(fd)


def group_alive(pgid):
    try:
        os.killpg(pgid, 0)
        return True
    except ProcessLookupError:
        return False


def run_command(name, command, output, env, timeout, lock_fd=None):
    """Reap the owned process group before returning to the lock owner."""
    log_path = output / (name + ".log")
    started = time.monotonic()
    record = {"status": "unknown", "command": command, "started_utc": utc_now(),
              "exit_code": None, "log": str(log_path)}
    old_handlers = {}
    child = None
    interrupted = []

    def cancel_owned():
        if child is not None and child.poll() is None:
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass

    def request_cancel(signum, _frame):
        interrupted.append(signum)
        cancel_owned()

    try:
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            old_handlers[signum] = signal.signal(signum, request_cancel)
        with log_path.open("w") as log:
            if interrupted:
                record.update(status="failed", error="cancelled before command spawn",
                              interrupted_by=interrupted)
                return record
            child = subprocess.Popen(command, cwd=ROOT, env=env, stdout=log,
                                     stderr=subprocess.STDOUT, start_new_session=True,
                                     pass_fds=() if lock_fd is None else (lock_fd,))
            if interrupted:
                # A signal can arrive while Popen is returning, before child is
                # assigned. Honor that latched cancellation without waiting for
                # the full measurement timeout.
                cancel_owned()
            try:
                child.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                record["error"] = "command timed out"
            finally:
                # Only the new process group created for this check is affected.
                # A script's timeout/error can leave its own Bree child running.
                if group_alive(child.pid):
                    os.killpg(child.pid, signal.SIGTERM)
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        pass
                    deadline = time.monotonic() + 5
                    while group_alive(child.pid) and time.monotonic() < deadline:
                        time.sleep(.05)
                    if group_alive(child.pid):
                        record["group_cleanup_required"] = True
                        os.killpg(child.pid, signal.SIGKILL)
                child.wait()
                # Do not release the experiment lock while surviving owned
                # descendants remain. Zombies are reaped by their parent/OS.
                while group_alive(child.pid):
                    print(f"{name}: waiting for owned child group to be reaped", flush=True)
                    time.sleep(1)
        record["exit_code"] = child.returncode
        record["owned_group_reaped"] = True
        record["status"] = ("passed" if child.returncode == 0 and not interrupted
                            and "error" not in record and not record.get("group_cleanup_required")
                            else "failed")
        if interrupted:
            record["interrupted_by"] = interrupted
    except (OSError, ValueError) as error:
        record.update(status="failed", error=str(error))
    finally:
        for signum, handler in old_handlers.items():
            signal.signal(signum, handler)
        record["duration_seconds"] = time.monotonic() - started
        record["finished_utc"] = utc_now()
    return record


def load_result(record, path):
    if not path.exists():
        record["result_status"] = "unknown"
        record["result_error"] = "tool did not produce a JSON result"
        if record["status"] == "passed":
            record["status"] = "failed"
        return
    try:
        record["result"] = json.loads(path.read_text())
        record["result_status"] = "available"
        record["result_file"] = str(path)
    except (ValueError, OSError) as error:
        record["result_status"] = "unknown"
        record["result_error"] = str(error)
        record["status"] = "failed"


def automated_success(report, skipped=()):
    return (all(row["status"] == "passed" for name, row in report["checks"].items()
                if name != "visual" and name not in skipped)
            and report.get("source_unchanged_after_checks") is True
            and report.get("binary_unchanged_after_checks") is True
            and report.get("build_consistency") == "passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lock", type=Path, required=True,
                        help="absolute shared native/performance flock file")
    parser.add_argument("--output", type=Path, required=True,
                        help="new directory below this checkout's .artifacts or target")
    parser.add_argument("--runs", type=int, default=20)
    parser.add_argument("--skip", action="append", default=[], choices=CHECKS)
    args = parser.parse_args()
    output = args.output.resolve()
    if not args.lock.is_absolute():
        parser.error("--lock must be absolute")
    if not any(output.is_relative_to(ROOT / name) for name in (".artifacts", "target")):
        parser.error("--output must be inside this checkout's .artifacts or target")
    if output.exists():
        parser.error("--output must be new; existing evidence is never overwritten")
    if args.runs < 20:
        parser.error("--runs must be at least 20")
    if "build" in args.skip:
        parser.error("a current release build is required")
    output.mkdir(parents=True, mode=0o700)
    output.chmod(0o700)
    temporary = output / "temporary"
    temporary.mkdir(mode=0o700)
    # Theme checks and Rust tests create their own private homes under TMPDIR.
    # Keep those homes inside this run as well as the explicit HOME.
    env = bree_environment(output / "empty-home")
    env["TMPDIR"] = str(temporary)
    binary = ROOT / "target/release/bree"
    manifest = source_manifest()
    (output / "source-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    diff = subprocess.check_output(["git", "diff", "HEAD", "--"], cwd=ROOT)
    (output / "worktree.patch").write_bytes(diff)
    report = {
        "schema_version": 1, "started_utc": utc_now(), "workspace": str(ROOT),
        "home_dir": env["HOME"], "temporary_dir": env["TMPDIR"],
        "commit": capture(["git", "rev-parse", "HEAD"]),
        "branch": capture(["git", "branch", "--show-current"]),
        "worktree_status": capture(["git", "status", "--short"]),
        "diff_summary": capture(["git", "diff", "--stat", "HEAD"]),
        "diff_sha256": hashlib.sha256(diff).hexdigest(),
        "source_manifest_sha256": manifest["sha256"],
        "toolchain": {"rustc": capture(["rustc", "-Vv"]),
                      "cargo": capture(["cargo", "-V"]),
                      "python": sys.version, "rust_toolchain_sha256": digest(ROOT / "rust-toolchain.toml")},
        "system": {"platform": platform.platform(), "architecture": platform.machine(),
                   "macos": capture(["sw_vers"]),
                   "hardware": capture(["sysctl", "hw.model", "hw.memsize", "hw.ncpu"])},
        "cargo_lock_sha256": digest(ROOT / "Cargo.lock"),
        "checks": {name: {"status": "not-run", "reason": "not reached"} for name in CHECKS},
        "lock_events": [],
        "limits": ["one live Mac and OS; workload/cache uncontrolled",
                   "PTY results do not establish physical terminal visual quality",
                   "local build does not verify released Homebrew/curl/signing combinations",
                   "Bree only inspects applications; no quit requests are supported"],
    }
    summary_path = output / "summary.json"

    def save():
        temporary = output / "summary.tmp"
        temporary.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        temporary.replace(summary_path)

    def check(name, command, timeout, lock_fd=None, result_path=None):
        if name in args.skip:
            report["checks"][name] = {"status": "not-run", "reason": "explicit --skip"}
            save()
            return False
        print(f"{name}: started", flush=True)
        # Cargo needs the caller's toolchain/configuration; only Bree gets an isolated HOME.
        command_env = dict(env)
        if command[0] == "cargo":
            if "HOME" in os.environ:
                command_env["HOME"] = os.environ["HOME"]
            else:
                command_env.pop("HOME", None)
        record = run_command(name, command, output, command_env, timeout, lock_fd)
        record["source_manifest_sha256"] = manifest["sha256"]
        if "binary" in report:
            record["binary_sha256"] = report["binary"]["sha256"]
        if result_path is not None:
            load_result(record, result_path)
        report["checks"][name] = record
        save()
        print(f"{name}: {record['status']} (exit {record['exit_code']})", flush=True)
        if record.get("interrupted_by"):
            raise KeyboardInterrupt("check cancelled; owned group reaped")
        return record["status"] == "passed"

    try:
        save()
        check("fmt", ["cargo", "fmt", "--check"], 300)
        check("clippy", ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"], 900)
        check("script-tests", [sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests", "-v"], 300)
        if not check("build", ["cargo", "build", "--locked", "--release"], 900):
            return 1
        report["binary"] = {"path": str(binary), "sha256": digest(binary),
                            "bytes": binary.stat().st_size, "version": capture([str(binary), "--version"], env=env)}
        if source_manifest()["sha256"] != manifest["sha256"]:
            report["build_consistency"] = "failed: source changed during checks/build"
            return 1
        report["build_consistency"] = "passed"
        save()
        with experiment_lock(args.lock, report["lock_events"]) as lock_fd:
            save()
            check("rust-tests", ["cargo", "test", "--locked"], 900, lock_fd)
            doctor_path = output / "doctor.json"
            check("doctor", [str(binary), "doctor", "--json"], 30, lock_fd)
            if (output / "doctor.log").exists():
                doctor_path.write_text((output / "doctor.log").read_text())
                load_result(report["checks"]["doctor"], doctor_path)
            for name, script, extras, timeout in (
                ("benchmark", "benchmark.py", ["--runs", str(args.runs)], 180),
                ("terminal", "terminal-check.py", ["--runs", str(args.runs)], 300),
                ("theme", "theme-check.py", [], 300),
                ("signal", "signal-check.py", ["--repeat", "20"], 180),
                ("soak", "soak.py", [], 780),
            ):
                if digest(binary) != report["binary"]["sha256"] or source_manifest()["sha256"] != manifest["sha256"]:
                    report["build_consistency"] = "failed: source or binary changed during measurement"
                    break
                result_path = output / (name + ".json")
                check(name, [sys.executable, str(ROOT / "scripts" / script),
                             "--binary", str(binary), "--output", str(result_path)] + extras,
                      timeout, lock_fd, result_path)
        report["checks"]["visual"] = {"status": "not-run", "reason": "requires separate physical terminal inspection"}
        report["source_unchanged_after_checks"] = source_manifest()["sha256"] == manifest["sha256"]
        report["binary_unchanged_after_checks"] = digest(binary) == report["binary"]["sha256"]
        return 0 if automated_success(report, args.skip) else 1
    except KeyboardInterrupt:
        report["orchestration_error"] = "cancelled after owned subprocess recovery"
        return 130
    except (OSError, ValueError) as error:
        report["orchestration_error"] = str(error)
        return 1
    finally:
        report["finished_utc"] = utc_now()
        required = [row["status"] for name, row in report["checks"].items() if name != "visual"]
        report["overall_status"] = ("failed" if "failed" in required or report.get("build_consistency", "").startswith("failed")
                                    or "orchestration_error" in report or report.get("source_unchanged_after_checks") is False
                                    or report.get("binary_unchanged_after_checks") is False
                                    else "incomplete" if "not-run" in required or "unknown" in required
                                    else "passed")
        report["overall_status_scope"] = "automated command exits, result availability and build consistency; performance budgets, visual quality and support claims are assessed separately"
        save()
        print(f"Evidence: {summary_path}", flush=True)


if __name__ == "__main__":
    sys.exit(main())
