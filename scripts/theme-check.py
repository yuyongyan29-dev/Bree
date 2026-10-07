#!/usr/bin/env python3
"""Check default terminal colors and Bree branding through private PTY children."""
import argparse
import fcntl
import json
import os
import pty
import re
import select
import struct
import subprocess
import tempfile
import termios
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, default=Path("target/release/bree"))
parser.add_argument("--output", type=Path)
args = parser.parse_args()
binary = args.binary.resolve()
CSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
SGR = re.compile(rb"\x1b\[([0-9;]*)m")


def check_background(data):
    for match in SGR.finditer(data):
        values = [int(p) if p else 0 for p in match.group(1).split(b";")]
        index = 0
        while index < len(values):
            value = values[index]
            if value == 48 or 40 <= value <= 47 or 100 <= value <= 107:
                raise RuntimeError(f"explicit background SGR: {match.group(0)!r}")
            if value == 38 and index + 1 < len(values):
                index += 5 if values[index + 1] == 2 else 3
            else:
                index += 1


def session(colorfgbg, columns, rows):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
    before = termios.tcgetattr(slave)
    before[3] &= ~termios.PENDIN
    captured = bytearray()
    with tempfile.TemporaryDirectory(prefix="bree-theme-check-") as temporary:
        env = os.environ.copy()
        env.update(TERM="xterm-256color", COLORTERM="truecolor", COLORFGBG=colorfgbg,
                   BREE_DATA_DIR=str(Path(temporary) / "store"))
        child = subprocess.Popen([str(binary)], stdin=slave, stdout=slave, stderr=slave, env=env)
        stages = [("GiB".encode(), b"s"), ("允许与保护规则".encode(), b"\x1b"),
                  ("内存压力".encode(), b"q")]
        current = 0
        stage_data = bytearray()
        deadline = time.monotonic() + 10
        try:
            while child.poll() is None and time.monotonic() < deadline:
                if select.select([master], [], [], .05)[0]:
                    chunk = os.read(master, 65536)
                    captured.extend(chunk)
                    stage_data.extend(chunk)
                    if current < len(stages) and stages[current][0] in CSI.sub(b"", stage_data):
                        os.write(master, stages[current][1])
                        current += 1
                        stage_data.clear()
            if child.poll() is None:
                raise RuntimeError("Bree theme check did not finish within 10 seconds")
            if current != len(stages) or child.returncode != 0:
                raise RuntimeError(f"missing screen or unexpected exit: stage={current}, code={child.returncode}")
            after = termios.tcgetattr(slave)
            after[3] &= ~termios.PENDIN
            if before != after:
                raise RuntimeError("terminal input modes changed")
            check_background(captured)
            plain = CSI.sub(b"", captured).decode(errors="replace")
            if "bree" not in plain or "-.-" not in plain:
                raise RuntimeError("Bree name or sleeping mascot was not emitted")
            expanded = columns >= 76 and rows >= 22
            if expanded and "█" not in plain:
                raise RuntimeError("expanded wordmark was not emitted")
            return {"colorfgbg": colorfgbg, "columns": columns, "rows": rows,
                    "brand": "expanded" if expanded else "compact",
                    "no_explicit_background": True, "settings_opened": True,
                    "exit_code": child.returncode, "terminal_restored": True}
        finally:
            if child.poll() is None:
                # Only this script's Bree child; a timeout is always a failed check.
                child.terminate()
                child.wait(timeout=5)
            os.close(master)
            os.close(slave)


report = {"cases": [session(theme, width, height)
                    for theme in ("0;15", "15;0")
                    for width, height in ((100, 34), (48, 16))],
          "limits": ["COLORFGBG represents test inputs; a PTY has no visual theme",
                     "checks emitted colors, branding, navigation and modes; not physical font rendering",
                     "popup cell colors are covered by Ratatui buffer unit tests"]}
encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
print(encoded, end="")
if args.output:
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(encoded)
