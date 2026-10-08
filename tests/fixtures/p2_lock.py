#!/usr/bin/env python3
"""Hold the shared, persistent flock through a native controller and recovery.

The descriptor is inherited by the controller; no lock file is deleted/replaced.
An isolated process group covers children missing from the manifest.
"""
import fcntl
import json
import pathlib
import sys
from p2_recovery import run_owned

lock_path = pathlib.Path(sys.argv[1])
lock_path.parent.mkdir(parents=True, exist_ok=True)
with lock_path.open("a+") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    # quit_probe's output directory is its second argument.
    result = run_owned(sys.argv[2:], sys.argv[4], lock.fileno())
    # The legacy shell may remove its own build only after this positive
    # confirmation, never merely because a fixed safety-timer delay elapsed.
    (pathlib.Path(sys.argv[4]) / "native-recovery-confirmed.json").write_text(json.dumps({
        "recovery_confirmed": True, "controller_exit_code": result.controller_exit_code,
        "wrapper_exit_code": result.wrapper_exit_code,
    }, indent=2) + "\n")
    sys.exit(result.wrapper_exit_code)
