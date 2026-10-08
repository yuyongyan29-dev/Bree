"""Signal restoration checks using only synthetic, owned PTY children."""
import argparse
import importlib.util
import os
from pathlib import Path
import signal
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
SPEC = importlib.util.spec_from_file_location("signal_check", Path(__file__).parents[1] / "signal-check.py")
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


class SignalRestorationTests(unittest.TestCase):
    def test_connected_terminal_requires_cursor_and_alternate_screen_restoration(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {
                "TERM": "dumb", "COLORTERM": "caller", "NO_COLOR": "1",
                "HOME": "/caller/private-store"}):
            root = Path(directory)
            for sequence in (b"", b"\x1b[?25h", b"\x1b[?1049l", b"\x1b[?25h\x1b[?1049l"):
                with self.subTest(sequence=sequence):
                    binary = root / "fake-signal-child"
                    binary.write_text(
                        "#!" + sys.executable + "\n"
                        "import os, signal, termios, tty\n"
                        "assert os.environ['TERM'] == 'xterm-256color'\n"
                        "assert os.environ['COLORTERM'] == 'truecolor'\n"
                        "assert 'NO_COLOR' not in os.environ\n"
                        f"assert os.environ['HOME'] == {str(root / 'data')!r}\n"
                        "before = termios.tcgetattr(0)\n"
                        "def stop(*_):\n"
                        "    termios.tcsetattr(0, termios.TCSANOW, before)\n"
                        f"    os.write(1, {sequence!r})\n"
                        "    raise SystemExit(130)\n"
                        "signal.signal(signal.SIGINT, stop)\n"
                        "tty.setraw(0)\n"
                        "os.write(1, b'\\x1b[?1049h\\x1b[?25l1. Memory')\n"
                        "while True: signal.pause()\n"
                    )
                    binary.chmod(0o700)
                    args = argparse.Namespace(home_dir=root / "data", send_after_ms=None,
                                              startup_timeout=5, exit_timeout=5)
                    row = CHECK.run_case(binary, "sigint", signal.SIGINT, False, args, 0)
                    self.assertEqual(row["exit_code"], 130)
                    self.assertTrue(row["terminal_modes_and_flags_restored"])
                    self.assertEqual(row["status"], "passed" if sequence == b"\x1b[?25h\x1b[?1049l" else "failed")
                    self.assertTrue(row["child_reaped"])
                    pid = next(event["pid"] for event in row["events"] if event["event"] == "spawn_returned")
                    with self.assertRaises(ProcessLookupError):
                        os.kill(pid, 0)


if __name__ == "__main__":
    unittest.main()
