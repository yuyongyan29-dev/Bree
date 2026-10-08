import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
SPEC = importlib.util.spec_from_file_location("stability_check", Path(__file__).parents[1] / "stability-check.py")
STABILITY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(STABILITY)


class EvidenceTests(unittest.TestCase):
    def test_cancel_during_spawn_is_forwarded_without_full_timeout(self):
        handlers = {}
        child = MagicMock(pid=12345, returncode=-15)
        child.poll.return_value = None
        def register(signum, handler):
            handlers[signum] = handler
            return STABILITY.signal.SIG_DFL
        def spawn(*_args, **_kwargs):
            handlers[STABILITY.signal.SIGTERM](STABILITY.signal.SIGTERM, None)
            return child
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(STABILITY.signal, "signal", side_effect=register), \
                patch.object(STABILITY.subprocess, "Popen", side_effect=spawn), \
                patch.object(STABILITY.os, "killpg") as send, \
                patch.object(STABILITY, "group_alive", return_value=False):
            row = STABILITY.run_command("cancel", ["unused"], Path(directory), {}, 780)
        send.assert_called_once_with(child.pid, STABILITY.signal.SIGTERM)
        self.assertEqual(row["status"], "failed")
        self.assertEqual(row["interrupted_by"], [STABILITY.signal.SIGTERM])
        self.assertTrue(row["owned_group_reaped"])

    def test_final_binary_replacement_cannot_return_success(self):
        report = {"checks": {"soak": {"status": "passed"}, "visual": {"status": "not-run"}},
                  "source_unchanged_after_checks": True,
                  "binary_unchanged_after_checks": True, "build_consistency": "passed"}
        self.assertTrue(STABILITY.automated_success(report))
        report["binary_unchanged_after_checks"] = False
        self.assertFalse(STABILITY.automated_success(report))
        report.pop("binary_unchanged_after_checks")
        self.assertFalse(STABILITY.automated_success(report))

    def test_missing_and_malformed_results_cannot_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "result.json"
            row = {"status": "passed"}
            STABILITY.load_result(row, path)
            self.assertEqual(row["status"], "failed")
            self.assertEqual(row["result_status"], "unknown")
            path.write_text("{")
            row = {"status": "passed"}
            STABILITY.load_result(row, path)
            self.assertEqual(row["status"], "failed")
            path.write_text(json.dumps({"some_metric": None}))
            row = {"status": "passed"}
            STABILITY.load_result(row, path)
            self.assertIsNone(row["result"]["some_metric"])

    def test_exit_failure_and_timeout_are_retained(self):
        with tempfile.TemporaryDirectory() as directory:
            row = STABILITY.run_command("failed", [sys.executable, "-c", "print('evidence'); raise SystemExit(7)"],
                                 Path(directory), os.environ.copy(), 5)
            self.assertEqual(row["status"], "failed")
            self.assertEqual(row["exit_code"], 7)
            self.assertIn("evidence", Path(row["log"]).read_text())
            self.assertTrue(row["owned_group_reaped"])
            row = STABILITY.run_command("timeout", [sys.executable, "-c", "import time; time.sleep(5)"],
                                 Path(directory), os.environ.copy(), .05)
            self.assertEqual(row["status"], "failed")
            self.assertEqual(row["error"], "command timed out")
            self.assertTrue(row["owned_group_reaped"])

    def test_shared_lock_is_exclusive_preserves_file_and_releases_after_reap(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "lock"
            path.write_text("preserved")
            inode = path.stat().st_ino
            probe = [sys.executable, "-c", (
                "import fcntl,sys; f=open(sys.argv[1], 'r+'); "
                "fcntl.flock(f, fcntl.LOCK_EX|fcntl.LOCK_NB)"), str(path)]
            events = []
            with STABILITY.experiment_lock(path, events) as fd:
                result = subprocess.run(probe, capture_output=True)
                self.assertNotEqual(result.returncode, 0)
                row = STABILITY.run_command("owned", [sys.executable, "-c", "print('done')"],
                                     Path(directory), os.environ.copy(), 5, fd)
                self.assertTrue(row["owned_group_reaped"])
                self.assertNotEqual(subprocess.run(probe, capture_output=True).returncode, 0)
            self.assertEqual(subprocess.run(probe, capture_output=True).returncode, 0)
            self.assertEqual(path.stat().st_ino, inode)
            self.assertEqual(path.read_text(), "preserved")
            self.assertEqual(events[-1]["event"], "lock_released")

    def test_relative_lock_is_rejected(self):
        with self.assertRaises(ValueError):
            with STABILITY.experiment_lock(Path("relative-lock"), []):
                self.fail("relative lock was accepted")


if __name__ == "__main__":
    unittest.main()
