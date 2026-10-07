#!/usr/bin/env python3
"""Send cancellation only to private Bree children and check terminal restoration."""
import argparse
import fcntl
import json
import os
import pty
import select
import signal
import struct
import subprocess
import termios
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, default=Path("target/release/bree"))
parser.add_argument("--output", type=Path, default=Path(".artifacts/signal-check.json"))
args = parser.parse_args()
binary = args.binary.resolve()
# Darwin FCNTLFLAGS; FWASWRITTEN records any write and cannot be cleared by F_SETFL.
mutable_flags = os.O_APPEND | os.O_ASYNC | os.O_SYNC | os.O_DSYNC | os.O_NONBLOCK
results = []
for case, requested_signal, close_master in [
    ("sigint", signal.SIGINT, False), ("sigterm", signal.SIGTERM, False),
    ("sighup", signal.SIGHUP, False), ("sighup_after_eof", signal.SIGHUP, True),
]:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 100, 0, 0))
    before = termios.tcgetattr(slave)
    before[3] &= ~termios.PENDIN
    flags_before = fcntl.fcntl(slave, fcntl.F_GETFL)
    child = subprocess.Popen([str(binary)], stdin=slave, stdout=slave, stderr=slave,
                             env=dict(os.environ, TERM="xterm-256color"))
    output = bytearray()
    deadline = time.monotonic() + .4
    while time.monotonic() < deadline:
        if select.select([master], [], [], .02)[0]:
            output.extend(os.read(master, 65536))
    if close_master:
        os.close(master)
        master = None
        # Let the dependency observe EOF first, so this verifies the trapped-read path.
        time.sleep(.1)
    child.send_signal(requested_signal)
    start = time.monotonic()
    try:
        deadline = start + 5
        while child.poll() is None and time.monotonic() < deadline:
            if master is not None and select.select([master], [], [], .02)[0]:
                output.extend(os.read(master, 65536))
            else:
                time.sleep(.02)
        if child.poll() is None:
            raise RuntimeError(f"{case}: Bree did not cancel")
        if child.returncode != 130:
            raise RuntimeError(f"{case}: unexpected code {child.returncode}")
        if master is not None:
            after = termios.tcgetattr(slave)
            after[3] &= ~termios.PENDIN
            if after != before:
                raise RuntimeError(f"{case}: raw mode not restored")
            if (fcntl.fcntl(slave, fcntl.F_GETFL) & mutable_flags) != (flags_before & mutable_flags):
                raise RuntimeError(f"{case}: file flags changed: {flags_before} -> {fcntl.fcntl(slave, fcntl.F_GETFL)}")
        results.append({"case": case, "exit_code": 130,
                        "cancel_ms": round((time.monotonic() - start) * 1000, 3),
                        "terminal_modes_and_flags_restored": True if master is not None else None})
    finally:
        if master is not None:
            os.close(master)
        os.close(slave)
        if child.poll() is None:
            child.kill()  # bounded cleanup of this exact test child only
            child.wait(timeout=5)
report = {"cases": results, "limits": ["restoration cannot be observed after the terminal itself is gone", "compares Darwin F_SETFL mutable flags; excludes kernel write-history FWASWRITTEN and transient termios PENDIN state"]}
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
print(json.dumps(report, ensure_ascii=False, indent=2))
