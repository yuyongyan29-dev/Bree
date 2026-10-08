#!/usr/bin/env python3
"""R1 native fault probe: self-built fixture only, shared flock, no signals.

The controller exits 73 after direct spawn. The fixture does not inherit the
flock descriptor. An independent observer checks that the wrapper still holds
the lock, then asks only this fixture to self-exit via its dedicated reap file.
Executable-path failure is injected in the read-only recovery adapter.
"""
import datetime
import fcntl
import hashlib
import json
import os
import pathlib
import plistlib
import shutil
import subprocess
import sys
import time

from p2_recovery import proc, run_owned

REPO = pathlib.Path(__file__).resolve().parents[2]
LOCK = REPO / ".artifacts/native-experiment.lock"


def read_json(path):
    return json.loads(path.read_text())


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def exists(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def wait_for(check, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.05)
    raise RuntimeError("Native recovery probe condition was not observed")


def validate_root(value):
    root = pathlib.Path(value).resolve()
    parent = REPO / ".artifacts/p2"
    if not root.is_relative_to(parent) or not root.parent.name.startswith("r1-native-"):
        raise ValueError("Probe only accepts its own dedicated run directories")
    return root


def controller(root):
    executable = root / "build/BreeQuitFixture.app/Contents/MacOS/BreeQuitFixture"
    case = root / "fixture"
    # No inherited flock descriptor: the observer can attribute lock custody
    # after controller exit to the wrapper itself, not to this fixture.
    child = subprocess.Popen([executable, "immediate", case / "fixture.jsonl", case],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, close_fds=True)
    log = case / "fixture.jsonl"

    def launched():
        if child.poll() is not None:
            raise RuntimeError("Fixture exited before launched")
        if log.exists():
            return next((row for row in map(json.loads, log.read_text().splitlines())
                         if row["event"] == "launched"), None)

    row = wait_for(launched)
    write_json(root / "fault-controller.json", {
        "controller_pid": os.getpid(), "controller_pgid": os.getpgrp(),
        "fixture_pid": child.pid, "fixture_pgid": os.getpgid(child.pid),
        "fixture_launch_pgid": row["process_group"], "fixture_parent_pid": row["parent_pid"],
        "fixture_inherits_flock_fd": False, "intentional_controller_exit": 73,
    })
    # Simulates the controller dying before its missing/truncated/omitted PID
    # manifest becomes usable. No normal quit or fixture reap is sent here.
    os._exit(73)


def observer(root):
    case = root / "fixture"
    checks = []
    try:
        metadata = wait_for(lambda: read_json(root / "fault-controller.json")
                            if (root / "fault-controller.json").exists() else None)
        pid, pgid = metadata["fixture_pid"], metadata["controller_pgid"]
        wait_for(lambda: not exists(metadata["controller_pid"]))
        for _ in range(3):
            alive = exists(pid)
            group = os.getpgid(pid)
            group_exists = False
            try:
                os.killpg(pgid, 0)
                group_exists = True
            except ProcessLookupError:
                pass
            blocked = False
            with LOCK.open("a+") as attempt:
                try:
                    fcntl.flock(attempt, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    fcntl.flock(attempt, fcntl.LOCK_UN)
                except BlockingIOError:
                    blocked = True
            checks.append({"fixture_alive": alive, "fixture_pgid": group,
                           "owned_group_exists": group_exists, "competing_flock_blocked": blocked,
                           "timestamp": time.time()})
            if not alive or group != pgid or not group_exists or not blocked:
                raise RuntimeError("Owned live fixture or wrapper lock custody was not confirmed")
            time.sleep(0.2)
        (case / "reap").write_text("Self-exit after R1 recovery and lock observations.\n")
        wait_for(lambda: not exists(pid))

        def lock_released():
            with LOCK.open("a+") as attempt:
                try:
                    fcntl.flock(attempt, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    fcntl.flock(attempt, fcntl.LOCK_UN)
                    return True
                except BlockingIOError:
                    return False

        wait_for(lock_released)
        write_json(root / "observer.json", {"passed": True, "live_lock_checks": checks,
                                           "fixture_absent_then_lock_released": True})
    except Exception as error:
        write_json(root / "observer.json", {"passed": False, "live_lock_checks": checks, "error": str(error)})
        raise
    finally:
        # This only writes inside the current disposable fixture's directory.
        (case / "reap").write_text("Self-exit after probe observation.\n")


class UnreadableFixturePath:
    def __init__(self, root):
        self.root = root
        self.failures = 0

    def proc_listallpids(self, *args):
        return proc.proc_listallpids(*args)

    def proc_pidpath(self, pid, *args):
        metadata = self.root / "fault-controller.json"
        if metadata.exists() and pid == read_json(metadata)["fixture_pid"]:
            self.failures += 1
            return 0
        return proc.proc_pidpath(pid, *args)


def main():
    import p2_recovery as recovery

    root = REPO / ".artifacts/p2" / (
        "r1-native-" + datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        + "-" + str(os.getpid()))
    root.mkdir()
    source = REPO / "tests/fixtures/QuitFixture.swift"
    sources = [source, pathlib.Path(__file__).resolve(), REPO / "tests/fixtures/p2_recovery.py"]
    digests = {str(path.relative_to(REPO)): hashlib.sha256(path.read_bytes()).hexdigest() for path in sources}
    template = root / "compiled-fixture"
    with (root / "build.log").open("w") as log:
        subprocess.run(["xcrun", "swiftc", source, "-o", template, "-framework", "AppKit"],
                       stdout=log, stderr=log, check=True)
    if digests != {str(path.relative_to(REPO)): hashlib.sha256(path.read_bytes()).hexdigest() for path in sources}:
        raise RuntimeError("Probe source changed during build; native action not started")
    write_json(root / "source-sha256.json", digests)
    summary = []
    for variant in ("missing", "truncated", "empty", "omitted"):
        run = root / variant
        case = run / "fixture"
        case.mkdir(parents=True)
        app = run / "build/BreeQuitFixture.app"
        executable = app / "Contents/MacOS/BreeQuitFixture"
        executable.parent.mkdir(parents=True)
        shutil.copy2(template, executable)
        with (app / "Contents/Info.plist").open("wb") as output:
            plistlib.dump({"CFBundleIdentifier": "local.bree.p2.r1.t" + root.name + "." + variant,
                          "CFBundleExecutable": "BreeQuitFixture", "CFBundleName": "Bree R1 Fixture",
                          "CFBundlePackageType": "APPL", "CFBundleVersion": "1", "LSUIElement": False}, output)
        with (root / "build.log").open("a") as log:
            subprocess.run(["codesign", "--force", "--sign", "-", app], stdout=log, stderr=log, check=True)
            subprocess.run(["codesign", "--verify", "--deep", "--strict", app], stdout=log, stderr=log, check=True)
        manifest = run / "owned-children.jsonl"
        if variant == "truncated":
            manifest.write_text('{"pid":')
        elif variant == "empty":
            manifest.write_text("")
        elif variant == "omitted":
            manifest.write_text(json.dumps({"pid": os.getpid(), "executable": "/usr/bin/unrelated"}) + "\n")
        adapter = UnreadableFixturePath(run)
        recovery.proc = adapter
        print("R1 native case: " + str(run), flush=True)
        with LOCK.open("a+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            monitor = subprocess.Popen([sys.executable, __file__, "--observer", run], close_fds=True)
            started = time.monotonic()
            result = run_owned([sys.executable, __file__, "--controller", run], run, lock.fileno())
            returned = time.monotonic() - started
        monitor_exit = monitor.wait()
        metadata = read_json(run / "fault-controller.json")
        observed = read_json(run / "observer.json")
        events = list(map(json.loads, (case / "fixture.jsonl").read_text().splitlines()))
        passed = (result.controller_exit_code == result.wrapper_exit_code == 73
                  and monitor_exit == 0 and observed["passed"] and adapter.failures > 0
                  and metadata["controller_pgid"] == metadata["controller_pid"]
                  and metadata["fixture_pgid"] == metadata["fixture_launch_pgid"] == metadata["controller_pgid"]
                  and metadata["fixture_parent_pid"] == metadata["controller_pid"]
                  and any(row["event"] == "p2_cleanup_exit" for row in events)
                  and not any(row["event"] in ("safety_timer_natural_exit", "application_should_terminate") for row in events))
        summary.append({"variant": variant, "passed": passed,
                        "controller_exit_code": result.controller_exit_code,
                        "wrapper_exit_code": result.wrapper_exit_code,
                        "observer_exit": monitor_exit, "injected_path_failures": adapter.failures,
                        "recovery_seconds": returned, "run_directory": str(run), "scope": metadata})
        write_json(root / "summary.json", summary)
        if not passed:
            raise RuntimeError("R1 native probe failed; see " + str(run))
    print(root, flush=True)


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] in ("--controller", "--observer"):
        dedicated = validate_root(sys.argv[2])
        (controller if sys.argv[1] == "--controller" else observer)(dedicated)
    elif len(sys.argv) == 1:
        main()
    else:
        raise SystemExit("usage: p2_recovery_probe.py (self-built fixture only)")
