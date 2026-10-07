#!/usr/bin/env python3
"""Exercise only the Bree child in a private PTY; check mode restoration and timings."""
import argparse
import fcntl
import json
import os
import pty
import re
import select
import struct
import subprocess
import termios
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, default=Path("target/release/bree"))
parser.add_argument("--runs", type=int, default=20)
parser.add_argument("--output", type=Path)
args = parser.parse_args()
binary = args.binary.resolve()
mutable_flags = os.O_APPEND | os.O_ASYNC | os.O_SYNC | os.O_DSYNC | os.O_NONBLOCK
if args.runs < 20:
    parser.error("at least 20 runs are required")

def session(keys, size=(28, 100), marker="分析中".encode(), timeout=10, expected_exit=0):
    master, slave = pty.openpty()
    before = termios.tcgetattr(slave)
    before[3] &= ~termios.PENDIN  # Darwin line-discipline state, not an input-mode setting.
    flags_before = fcntl.fcntl(slave, fcntl.F_GETFL)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", *size, 0, 0))
    env = os.environ.copy()
    env["TERM"] = "xterm-256color"
    start = time.perf_counter()
    child = subprocess.Popen([str(binary)], stdin=slave, stdout=slave, stderr=slave, env=env)
    collected = bytearray()
    first = None
    reached = False
    deadline = start + timeout
    try:
        while time.perf_counter() < deadline:
            if select.select([master], [], [], .05)[0]:
                chunk = os.read(master, 65536)
                collected.extend(chunk)
                plain = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", collected)
                if first is None and marker in plain:
                    first = (time.perf_counter() - start) * 1000
                    reached = True
                    os.write(master, keys)
            if child.poll() is not None:
                break
        if child.poll() is None:
            # This is our PTY child only. A timeout is a test failure.
            child.terminate()
            stop_deadline = time.perf_counter() + 5
            while child.poll() is None and time.perf_counter() < stop_deadline:
                if select.select([master], [], [], .05)[0]:
                    os.read(master, 65536)
            if child.poll() is None:
                child.kill()
                os.close(master)
                master = None
                child.wait(timeout=5)
            raise RuntimeError("Bree PTY did not exit after keys")
        after = termios.tcgetattr(slave)
        after[3] &= ~termios.PENDIN
        if before != after:
            raise RuntimeError("terminal modes were not restored")
        if (fcntl.fcntl(slave, fcntl.F_GETFL) & mutable_flags) != (flags_before & mutable_flags):
            raise RuntimeError(f"terminal file flags changed: {flags_before} -> {fcntl.fcntl(slave, fcntl.F_GETFL)}")
        if not reached:
            raise RuntimeError("expected screen marker was not rendered")
        if child.returncode != expected_exit:
            raise RuntimeError(f"PTY exit {child.returncode}")
        return first, collected.decode(errors="replace")
    finally:
        if master is not None:
            os.close(master)
        os.close(slave)

times = [session(b"q")[0] for _ in range(args.runs)]
result_times = [session(b"q", marker="GiB".encode())[0] for _ in range(args.runs)]
_, ctrl_c = session(b"\x03", expected_exit=130)
_, tiny = session(b"q", size=(5, 20), marker="调整".encode())
report = {
    "runs": args.runs,
    "skeleton_p95_ms": sorted(times)[max(0, int(len(times) * .95) - 1)],
    "first_home_result_p95_ms": sorted(result_times)[max(0, int(len(result_times) * .95) - 1)],
    "terminal_restored_after_q": True,
    "terminal_restored_after_ctrl_c": True,
    "terminal_file_flags_restored": True,
    "tiny_window_notice": True,
    "limits": ["private PTY; no visual font rendering check", "no physical terminal emulator UI inspected", "compares Darwin F_SETFL mutable flags; excludes kernel write-history FWASWRITTEN and transient termios PENDIN state"],
}
print(json.dumps(report, ensure_ascii=False, indent=2))
if args.output:
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
