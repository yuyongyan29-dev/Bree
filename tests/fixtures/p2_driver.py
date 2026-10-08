#!/usr/bin/env python3
"""Build an isolated cfg(test) library, then run only owned P2 native fixtures.

Usage: zsh scripts/probe-quit.zsh --p2
No release gate, environment switch in Bree, user-app PID, or force quit exists.
Logs and dedicated test documents are retained under .artifacts/p2.
"""
import datetime
import fcntl
import hashlib
import json
import os
import pathlib
import sys
import plistlib
import subprocess
from p2_recovery import run_owned
if len(sys.argv) > 2:
    raise SystemExit("usage: p2_driver.py [case]")
selected_case = sys.argv[1] if len(sys.argv) == 2 else None

repo = pathlib.Path(__file__).resolve().parents[2]
root = repo / ".artifacts/p2"
root.mkdir(parents=True, exist_ok=True)
run_root = root / (datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + str(os.getpid()))
run_root.mkdir()
app = run_root / "build/BreeQuitFixture.app"
executable = app / "Contents/MacOS/BreeQuitFixture"
executable.parent.mkdir(parents=True)
with (app / "Contents/Info.plist").open("wb") as output:
    plistlib.dump({
        "CFBundleIdentifier": "local.bree.p2.quit-fixture.t" + run_root.name,
        "CFBundleExecutable": "BreeQuitFixture",
        "CFBundleName": "Bree P2 Quit Fixture",
        "CFBundleVersion": "1",
        "CFBundlePackageType": "APPL",
        "LSUIElement": False,
        "NSHighResolutionCapable": True,
        "CFBundleDocumentTypes": [{
            "CFBundleTypeName": "Bree P2 Text", "CFBundleTypeRole": "Editor",
            "LSItemContentTypes": ["public.plain-text"], "NSDocumentClass": "FileDocument",
            "CFBundleTypeExtensions": ["txt"],
        }],
    }, output)
env = dict(os.environ, CARGO_TARGET_DIR=str(root / "test-build"),
           BREE_DATA_DIR=str(run_root / "isolated-data"))
commands = run_root / "commands.log"

def run(args):
    with commands.open("a") as log:
        log.write("command: " + json.dumps([str(arg) for arg in args]) + "\n")
        log.flush()
        result = subprocess.run(args, cwd=repo, env=env, stdout=log, stderr=log, check=False)
        log.write("exit_code: " + str(result.returncode) + "\n")
    if result.returncode:
        raise SystemExit("Build failed; see " + str(commands))

sources = ["src/cleanup.rs", "src/platform/actions_macos.rs", "tests/fixtures/QuitFixture.swift",
           "tests/fixtures/p2_native_harness.rs", "tests/fixtures/p2_session_access.rs",
           "tests/fixtures/p2_driver.py", "tests/fixtures/p2_recovery.py", "Cargo.toml", "Cargo.lock"]

def source_hashes():
    return {name: hashlib.sha256((repo / name).read_bytes()).hexdigest() for name in sources}

before_build = source_hashes()
for args in [["sw_vers"], ["uname", "-m"], ["xcrun", "swiftc", "--version"],
             ["cargo", "--version"], ["git", "rev-parse", "HEAD"], ["git", "status", "--short"]]:
    run(args)
run(["xcrun", "swiftc", repo / "tests/fixtures/QuitFixture.swift", "-o", executable, "-framework", "AppKit"])
run(["codesign", "--force", "--sign", "-", app])
run(["codesign", "--verify", "--deep", "--strict", app])
# cargo's normal/release library never gains cfg(test); keep this separate cache.
run(["cargo", "rustc", "--locked", "--lib", "--", "--cfg", "test"])
harness = run_root / "p2_native_harness"
run(["rustc", "--edition=2024", repo / "tests/fixtures/p2_native_harness.rs",
     "--extern", "bree_cli=" + str(root / "test-build/debug/libbree_cli.rlib"),
     "-L", "dependency=" + str(root / "test-build/debug/deps"), "-o", harness])
after_build = source_hashes()
(run_root / "source-sha256.json").write_text(json.dumps(after_build, indent=2) + "\n")
if before_build != after_build:
    raise SystemExit("Source changed during P2 build; native experiment not started: " + str(run_root))
(run_root / "binary-sha256.json").write_text(json.dumps({
    "fixture": hashlib.sha256(executable.read_bytes()).hexdigest(),
    "harness": hashlib.sha256(harness.read_bytes()).hexdigest(),
    "source_unchanged_during_build": True,
}, indent=2) + "\n")
print("P2 build complete; waiting for native-experiment.lock: " + str(run_root), flush=True)
with (repo / ".artifacts/native-experiment.lock").open("a+") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    print("P2 native lock acquired", flush=True)
    with (run_root / "controller.jsonl").open("w") as output, (run_root / "controller.stderr").open("w") as errors:
        invocation = [harness, executable, run_root] + ([selected_case] if selected_case else [])
        result = run_owned(invocation, run_root, lock.fileno(), cwd=repo, env=env,
                           stdout=output, stderr=errors)
    (run_root / "driver-result.json").write_text(json.dumps({
        "controller_exit_code": result.controller_exit_code,
        "wrapper_exit_code": result.wrapper_exit_code, "received_signals": result.received_signals,
        "shared_flock": True,
        "release_gate_enabled": False, "run_directory": str(run_root),
    }, indent=2) + "\n")
print(run_root, flush=True)
raise SystemExit(result.wrapper_exit_code)
