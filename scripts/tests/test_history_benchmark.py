"""Fixture, output-contract and terminal-frame regressions; no formal timing runs."""
import importlib.util
import json
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location("history_benchmark", SCRIPTS / "history-benchmark.py")
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


class FixtureTests(unittest.TestCase):
    def test_near_capacity_fixture_has_valid_complete_private_recent_records(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "data"
            fixture = benchmark.create_fixture(root)
            self.assertGreater(fixture["journal_bytes"], 9.99 * 1024 * 1024)
            self.assertLessEqual(fixture["journal_bytes"], benchmark.MAX_JOURNAL_BYTES)
            self.assertEqual(stat.S_IMODE(root.stat().st_mode), 0o700)
            self.assertEqual(stat.S_IMODE((root / "journal.jsonl").stat().st_mode), 0o600)
            data = (root / "journal.jsonl").read_bytes()
            self.assertTrue(data.endswith(b"\n"))
            records = [json.loads(line) for line in data.splitlines()]
            self.assertEqual(len(records), fixture["record_count"])
            now = time.time_ns() // 1_000_000
            for row in records:
                self.assertEqual(set(row), {"schema_version", "timestamp_unix_ms", "event", "data"})
                self.assertEqual(row["schema_version"], 1)
                self.assertGreater(row["timestamp_unix_ms"], now - 7 * 24 * 3600 * 1000)
                self.assertLessEqual(row["timestamp_unix_ms"], now)
                self.assertTrue(row["data"]["run_id"].startswith("run:benchmark"))
                if row["event"] == "cleanup_finished":
                    result = row["data"]
                    self.assertEqual(result["schema_version"], 1)
                    self.assertIsNone(result["resource_after"])
                    self.assertIsNone(result["targets"][0]["identity"])
                    self.assertEqual(result["targets"][0]["outcome"], "unknown")
                    for key, metric in result["resource_before"].items():
                        if key != "used_definition":
                            self.assertIsNone(metric["value"])
                            self.assertEqual(metric["status"], "unknown")
            self.assertEqual(records[-1]["event"], "target_request_sent")
            self.assertEqual(records[-1]["data"]["run_id"], benchmark.UNFINISHED_ID)
            self.assertEqual(records[-3]["event"], "cleanup_finished")
            self.assertEqual(records[-3]["data"]["run_id"], benchmark.FINISHED_ID)

    def test_existing_directory_and_files_are_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "data"
            root.mkdir()
            old = root / "journal.jsonl"
            old.write_bytes(b"existing data\n")
            with self.assertRaises(FileExistsError):
                benchmark.create_fixture(root)
            self.assertEqual(old.read_bytes(), b"existing data\n")

    def test_relative_directory_and_out_of_range_sizes_rejected_before_write(self):
        with self.assertRaises(ValueError):
            benchmark.create_fixture(Path("relative"))
        with tempfile.TemporaryDirectory() as temporary:
            for size in (1, benchmark.MAX_JOURNAL_BYTES + 1):
                root = Path(temporary) / str(size)
                with self.assertRaises(ValueError):
                    benchmark.create_fixture(root, size)
                self.assertFalse(root.exists())

    def test_integrity_check_detects_lock_files_and_permission_changes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "data"
            benchmark.create_fixture(root, 4096)
            before = benchmark.digest_tree(root)
            lock = root / "journal.lock"
            lock.touch(mode=0o600)
            self.assertNotEqual(before, benchmark.digest_tree(root))
            lock.unlink()
            self.assertEqual(before, benchmark.digest_tree(root))
            os.chmod(root / "journal.jsonl", 0o644)
            self.assertNotEqual(before, benchmark.digest_tree(root))


class MeasurementTests(unittest.TestCase):
    def cli_output(self, replay=False, unfinished_status="unfinished"):
        records = [{"run_id": benchmark.UNFINISHED_ID, "status": unfinished_status, "result": None},
                   {"run_id": benchmark.FINISHED_ID, "status": "finished", "result": {"schema_version": 1}}]
        records.extend({"run_id": f"run:other-{index}"} for index in range(48))
        return json.dumps({"schema_version": 1, "replay_enabled": replay, "records": records}).encode()

    def test_actual_cli_command_data_scope_and_finished_unfinished_contract(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with patch.object(benchmark.subprocess, "run", return_value=subprocess.CompletedProcess(
                    [], 0, self.cli_output(), b"")) as call:
                row = benchmark.cli_read(Path("/bree"), root / "data", root, 0)
            self.assertEqual(row["status"], "passed")
            self.assertEqual(call.call_args.args[0], ["/bree", "history", "--json", "--limit", "50"])
            self.assertEqual(call.call_args.kwargs["env"]["BREE_DATA_DIR"], str(root / "data"))
            self.assertFalse(row["replay_enabled"])
            self.assertTrue(row["unfinished_without_result"])
            self.assertTrue(Path(row["stdout"]).is_file())

    def test_failure_is_retained_with_command_exit_and_raw_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with patch.object(benchmark.subprocess, "run", return_value=subprocess.CompletedProcess(
                    [], 23, b"", b"journal corrupt")):
                row = benchmark.cli_read(Path("/bree"), root / "data", root, 0)
            self.assertEqual(row["status"], "failed")
            self.assertEqual(row["exit_code"], 23)
            self.assertEqual(Path(row["stderr"]).read_bytes(), b"journal corrupt")
            self.assertEqual(row["command"][1], "history")

    def test_timeout_keeps_partial_output_and_unknown_exit_status(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            error = subprocess.TimeoutExpired(["/bree"], 15, output=b"partial", stderr=b"diagnostic")
            with patch.object(benchmark.subprocess, "run", side_effect=error):
                row = benchmark.cli_read(Path("/bree"), root / "data", root, 0)
            self.assertEqual(row["status"], "failed")
            self.assertIsNone(row["exit_code"])
            self.assertEqual(row["exit_status"], "unknown")
            self.assertEqual(Path(row["stdout"]).read_bytes(), b"partial")
            self.assertEqual(Path(row["stderr"]).read_bytes(), b"diagnostic")

    def test_replay_or_lost_unfinished_semantics_fail(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for output in (self.cli_output(replay=True), self.cli_output(unfinished_status="finished")):
                with patch.object(benchmark.subprocess, "run", return_value=subprocess.CompletedProcess(
                        [], 0, output, b"")):
                    row = benchmark.cli_read(Path("/bree"), root / "data", root, 0)
                self.assertEqual(row["status"], "failed")

    def test_percentile_uses_nearest_rank_and_does_not_forge_missing_samples(self):
        self.assertEqual(benchmark.summarize([], "elapsed_ms")["status"], "not-run")
        rows = [{"status": "passed", "elapsed_ms": value} for value in range(1, 21)]
        rows.append({"status": "failed"})
        summary = benchmark.summarize(rows, "elapsed_ms")
        self.assertEqual(summary["samples"], 20)
        self.assertEqual(summary["p95_ms"], 19)
        self.assertEqual(summary["median_ms"], 10.5)


class TerminalFrameTests(unittest.TestCase):
    def test_cleanup_reaps_child_despite_screen_or_read_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "fake-history-child"
            binary.write_text(
                "#!" + sys.executable + "\n"
                "import os, signal, time\n"
                "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                "while True:\n"
                "    os.write(1, b'ready\\r\\n')\n"
                "    time.sleep(.01)\n"
            )
            binary.chmod(0o700)
            for failure in ("screen", "read"):
                with self.subTest(failure=failure):
                    session = benchmark.PtySession(binary, root / "data", root / (failure + ".pty"))
                    try:
                        session.wait_screen(["ready"])
                        target, name, error = (
                            (benchmark.Screen, "feed", RuntimeError("screen failure"))
                            if failure == "screen" else
                            (benchmark.os, "read", OSError("read failure")))
                        with patch.object(target, name, side_effect=error):
                            session.close()
                        self.assertIsNotNone(session.child.returncode)
                        with self.assertRaises(ProcessLookupError):
                            os.kill(session.child.pid, 0)
                    finally:
                        if session.child.poll() is None:
                            session.child.kill()
                        session.child.wait(timeout=5)

    def test_differential_cursor_output_does_not_reuse_previous_screen_markers(self):
        screen = benchmark.Screen(140, 40)
        screen.feed(b"\x1b[1;1HHistory\x1b[2;1HOld detail")
        self.assertIn("History", screen.text())
        screen.feed(b"\x1b[1;1HRun details\x1b[K\x1b[2;1HNew detail\x1b[K")
        self.assertNotIn("History", screen.text())
        self.assertNotIn("Old detail", screen.text())
        self.assertIn("Run details", screen.text())

    def test_partial_utf8_and_escape_sequences_form_one_rendered_marker(self):
        screen = benchmark.Screen(140, 40)
        value = "\x1b[2;4HHistory · Recent runs".encode()
        for byte in value:
            screen.feed(bytes([byte]))
        self.assertIn("History · Recent runs", screen.text())


if __name__ == "__main__":
    unittest.main()
