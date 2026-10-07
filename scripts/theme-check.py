#!/usr/bin/env python3
"""Check Bree's inherited colors and navigation through private PTY children."""
import argparse
import codecs
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
import unicodedata
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, default=Path("target/release/bree"))
parser.add_argument("--output", type=Path)
args = parser.parse_args()
binary = args.binary.resolve()
CSI = re.compile(r"\x1b\[([0-?]*)([ -/]*)([@-~])")
SGR = re.compile(rb"\x1b\[([0-9;]*)m")
MUTABLE_FLAGS = os.O_APPEND | os.O_ASYNC | os.O_SYNC | os.O_DSYNC | os.O_NONBLOCK


class Screen:
    """Small VT text recorder for the cursor/erase sequences emitted by Crossterm."""
    def __init__(self, columns, rows):
        self.columns, self.rows = columns, rows
        self.cells = [[" "] * columns for _ in range(rows)]
        self.x = self.y = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        value = self.pending + self.decoder.decode(data)
        self.pending = ""
        index = 0
        while index < len(value):
            char = value[index]
            if char == "\x1b":
                if index + 1 == len(value):
                    self.pending = value[index:]
                    break
                if value[index + 1] == "[":
                    match = CSI.match(value, index)
                    if not match:
                        self.pending = value[index:]
                        break
                    self.control(match.group(1), match.group(3))
                    index = match.end()
                    continue
                index += 2
                continue
            if char == "\r":
                self.x = 0
            elif char == "\n":
                self.y = min(self.y + 1, self.rows - 1)
            elif ord(char) >= 32:
                width = 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
                if self.x < self.columns:
                    self.cells[self.y][self.x] = char
                    if width == 2 and self.x + 1 < self.columns:
                        self.cells[self.y][self.x + 1] = ""
                self.x = min(self.x + width, self.columns)
            index += 1

    def control(self, parameters, command):
        if parameters.startswith("?"):
            return
        values = [int(v) if v else 0 for v in parameters.split(";")]
        first = values[0] or 1
        if command in ("H", "f"):
            self.y = min(first - 1, self.rows - 1)
            self.x = min((values[1] or 1) - 1 if len(values) > 1 else 0, self.columns - 1)
        elif command == "G":
            self.x = min(first - 1, self.columns - 1)
        elif command == "d":
            self.y = min(first - 1, self.rows - 1)
        elif command == "A":
            self.y = max(0, self.y - first)
        elif command == "B":
            self.y = min(self.rows - 1, self.y + first)
        elif command == "C":
            self.x = min(self.columns - 1, self.x + first)
        elif command == "D":
            self.x = max(0, self.x - first)
        elif command == "J":
            if values[0] == 2:
                self.cells = [[" "] * self.columns for _ in range(self.rows)]
            elif values[0] == 0:
                self.cells[self.y][self.x:] = [" "] * (self.columns - self.x)
                for row in range(self.y + 1, self.rows):
                    self.cells[row] = [" "] * self.columns
        elif command == "K":
            if values[0] == 2:
                self.cells[self.y] = [" "] * self.columns
            elif values[0] == 0:
                self.cells[self.y][self.x:] = [" "] * (self.columns - self.x)

    def lines(self):
        return ["".join(row).rstrip() for row in self.cells]

    def text(self):
        return "\n".join(self.lines())


def check_colors(data):
    for match in SGR.finditer(data):
        values = [int(p) if p else 0 for p in match.group(1).split(b";")]
        index = 0
        while index < len(values):
            value = values[index]
            if value == 48 or 40 <= value <= 47 or 100 <= value <= 107:
                raise RuntimeError(f"explicit background SGR: {match.group(0)!r}")
            if value == 7:
                raise RuntimeError("reverse-video selection was emitted")
            if 30 <= value <= 37 or 90 <= value <= 97:
                raise RuntimeError(f"non-default text foreground: {match.group(0)!r}")
            if value == 38 and index + 1 < len(values):
                index += 5 if values[index + 1] == 2 else 3
            else:
                index += 1


def session(colorfgbg, columns, rows, cancel_in_editor=False):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
    before = termios.tcgetattr(slave)
    before[3] &= ~termios.PENDIN
    flags_before = fcntl.fcntl(slave, fcntl.F_GETFL) & MUTABLE_FLAGS
    captured = bytearray()
    screen = Screen(columns, rows)
    with tempfile.TemporaryDirectory(prefix="bree-theme-check-") as temporary:
        env = os.environ.copy()
        env.update(TERM="xterm-256color", COLORTERM="truecolor", COLORFGBG=colorfgbg,
                   BREE_DATA_DIR=str(Path(temporary) / "store"))
        child = subprocess.Popen([str(binary)], stdin=slave, stdout=slave, stderr=slave, env=env)
        # These keys only inspect pages. They never confirm a rule or request application quit.
        stages = [("Memory pressure:", b"s"), ("Settings · Allow / Protect", b"\x1b"),
                  ("Memory pressure:", b"3\r"), ("bree Memory", b"/"),
                  ("Enter Apply", b"qrsa"), ("Search: qrsa", b"\x15bree-private-pty-no-match\r"),
                  ("No matches in this filter.", b"/"), ("Enter Apply", b"\x15draft\x1b"),
                  ("Search: bree-private-pty-no-match", b"\x1b"), ("/ Search names", b"\x1b"),
                  ("Memory pressure:", b"p"), ("Preview · No quit requests", b"\x1b"),
                  ("Memory pressure:", b"4\r"), ("History · Recent runs", b"q")]
        if cancel_in_editor:
            stages = [("Memory pressure:", b"3\r"), ("bree Memory", b"/"),
                      ("Enter Apply", b"\x03")]
        expected_exit = 130 if cancel_in_editor else 0
        current = 0
        updated = time.monotonic()
        pending_action = False
        home = None
        deadline = time.monotonic() + 15
        try:
            while child.poll() is None and time.monotonic() < deadline:
                if select.select([master], [], [], .025)[0]:
                    chunk = os.read(master, 65536)
                    captured.extend(chunk)
                    screen.feed(chunk)
                    updated = time.monotonic()
                    pending_action = current < len(stages) and stages[current][0] in screen.text()
                if pending_action and time.monotonic() - updated >= .025:
                    if current == 0:
                        home = screen.lines()
                        for label in ("1. Clean", "2. Needs review", "> 3. Memory", "4. History", "S Settings", "Q Quit"):
                            if label not in screen.text():
                                raise RuntimeError(f"home entry clipped: {label}\n{screen.text()}")
                        if "-.-" in screen.text() or "Rules and preview" in screen.text():
                            raise RuntimeError("removed mascot or subtitle remains")
                    else:
                        # Both narrow and wide pages must keep the exit key visible.
                        exit_key = "Ctrl+C Quit" if "Enter Apply" in screen.text() else "Q Quit"
                        if exit_key not in screen.text():
                            raise RuntimeError(f"page exit key clipped: stage {current}\n{screen.text()}")
                    os.write(master, stages[current][1])
                    current += 1
                    pending_action = False
            if child.poll() is None:
                raise RuntimeError(f"Bree theme check timed out at stage {current}\n{screen.text()}")
            if current != len(stages) or child.returncode != expected_exit:
                raise RuntimeError(f"missing screen or unexpected exit: stage={current}, code={child.returncode}")
            after = termios.tcgetattr(slave)
            after[3] &= ~termios.PENDIN
            if before != after:
                raise RuntimeError("terminal input modes changed")
            if (fcntl.fcntl(slave, fcntl.F_GETFL) & MUTABLE_FLAGS) != flags_before:
                raise RuntimeError("mutable terminal file flags changed")
            check_colors(captured)
            expanded = columns >= 80 and rows >= 24
            if expanded != any("█▄▄▄" in line for line in home):
                raise RuntimeError("wordmark did not adapt to available space")
            result = {"colorfgbg": colorfgbg, "columns": columns, "rows": rows,
                    "brand": "wordmark" if expanded else "compact",
                    "default_background_and_text": True, "no_reverse_video": True,
                    "pages_opened": ["Home", "Memory"] if cancel_in_editor else ["Home", "Settings", "Memory", "Preview", "History"],
                    "critical_entries_visible": True, "home_lines": home,
                    "exit_code": child.returncode, "terminal_restored": True,
                    "mutable_file_flags_restored": True}
            if cancel_in_editor:
                result["ctrl_c_in_search_editor"] = True
            else:
                result["search_edit_submit_cancel_clear"] = True
                result["shortcut_letters_are_search_text"] = True
            return result
        finally:
            if child.poll() is None:
                # Only this script's Bree child; a timeout is always a failed check.
                child.terminate()
                stop = time.monotonic() + 5
                while child.poll() is None and time.monotonic() < stop:
                    if select.select([master], [], [], .05)[0]:
                        os.read(master, 65536)
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=5)
            os.close(master)
            os.close(slave)


report = {"cases": [session(theme, width, height)
                    for theme in ("0;15", "15;0")
                    for width, height in ((100, 28), (140, 40), (48, 16))],
          "editor_ctrl_c": session("15;0", 48, 16, cancel_in_editor=True),
          "limits": ["COLORFGBG represents test inputs; a PTY has no visual theme",
                     "checks emitted colors, rendered cell text, navigation and modes; not physical font rendering",
                     "popup cell colors are covered by Ratatui buffer unit tests"]}
encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
print(encoded, end="")
if args.output:
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(encoded)
