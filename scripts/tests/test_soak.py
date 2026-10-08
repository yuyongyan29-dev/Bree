import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).parents[1]
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location("soak", SCRIPTS / "soak.py")
SOAK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SOAK)

HOME = ("Memory pressure: Normal\nUsed 1 GiB / Total 2 GiB\n"
        "UTC 01:02:03 · 10 ms\n3. Memory\n4. History\nEnter Open")
RESOURCES = ("bree Memory\nSample UTC 01:02:03 · 10 ms\n"
             "/ Search names, Bundle ID or exact PID\nEsc Home\nMemory · 2.0s refresh")
FAKE = r'''#!INTERPRETER
import os,select,sys,termios,time,tty
assert os.environ['TERM']=='xterm-256color'
assert os.environ['COLORTERM']=='truecolor'
assert 'NO_COLOR' not in os.environ
assert os.environ['BREE_DATA_DIR']!='/caller/private-store'
before=termios.tcgetattr(0)
tty.setraw(0)
page='resources' if len(sys.argv)>1 else 'home'
number=0
def draw():
    global number
    stamp='01:02:%02d' % (number % 60)
    number+=1
    if page=='home':
        text='Memory pressure: Normal\nUTC '+stamp+' · 10 ms\n3. Memory\n4. History\nEnter Open'
    else:
        text='bree Memory\nSample UTC '+stamp+' · 10 ms\n/ Search names, Bundle ID or exact PID\nEsc Home\nMemory · 2.0s refresh'
    os.write(1,('\x1b[2J\x1b[H'+text.replace('\n','\r\n')).encode())
draw()
next_draw=time.monotonic()+.09
try:
    while True:
        if select.select([0],[],[],.01)[0]:
            key=os.read(0,10)
            if b'q' in key:
                break
            if b'3' in key:
                page='resources'
                draw()
                next_draw=time.monotonic()+.09
        if page=='resources' and time.monotonic()>=next_draw:
            draw()
            next_draw=time.monotonic()+.09
finally:
    termios.tcsetattr(0,termios.TCSANOW,before)
'''


class SoakTests(unittest.TestCase):
    def test_cpu_time_units_and_days(self):
        self.assertEqual(SOAK.cpu_seconds("00:01.25"), 1.25)
        self.assertEqual(SOAK.cpu_seconds("02:01:03"), 7263)
        self.assertEqual(SOAK.cpu_seconds("1-02:01:03"), 93663)

    def test_page_markers_require_page_only_content(self):
        self.assertEqual(SOAK.page_state(HOME)["page"], "home")
        self.assertEqual(SOAK.page_state(RESOURCES)["page"], "resources")
        self.assertEqual(SOAK.page_state("Memory All Apps AI/Dev System Review")["page"], "unknown")
        self.assertEqual(SOAK.page_state(HOME + "\nbree Memory")["page"], "home")
        self.assertIsNone(SOAK.page_state("Memory pressure: Unknown")["sample_utc"])

    def test_cell_recorder_erases_stale_titles_and_decodes_split_chunks(self):
        screen = SOAK.Screen(100, 28)
        data = ("\x1b[H" + HOME.replace("\n", "\r\n")).encode()
        for byte in data:
            screen.feed(bytes([byte]))
        self.assertEqual(SOAK.page_state(screen.text())["page"], "home")
        data = ("\x1b[2J\x1b[H" + RESOURCES.replace("\n", "\r\n")).encode()
        for position in range(0, len(data), 3):
            screen.feed(data[position:position + 3])
        self.assertEqual(SOAK.page_state(screen.text())["page"], "resources")
        self.assertNotIn("4. History", screen.text())

    def test_nearest_rank_p95_and_bounded_growth_evidence(self):
        rows = [{"elapsed_seconds": n * 10, "rss_bytes": n + 1, "cpu_seconds": n * .1}
                for n in range(21)]
        summary = SOAK.summarize_samples(rows)
        self.assertEqual(summary["rss_p95_bytes"], 20)
        self.assertAlmostEqual(summary["average_cpu_one_core_percent"], 1)
        self.assertTrue(summary["rss_trend"]["all_window_medians_strictly_increasing"])
        self.assertEqual(SOAK.summarize_samples([])["rss_p95_bytes"], None)
        self.assertEqual(SOAK.summarize_samples(rows[:2])["rss_trend"]
                         ["all_window_medians_strictly_increasing"], None)

    def test_ready_duration_endpoint_page_refresh_and_restoration(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            binary = folder / "fake-bree"
            binary.write_text(FAKE.replace("INTERPRETER", sys.executable))
            binary.chmod(0o700)
            args = argparse.Namespace(binary=binary, output=folder / "soak.json",
                                      home_seconds=.3, resources_seconds=.4,
                                      watch_seconds=.4, sample_interval=.08)
            with patch.dict(os.environ, {"BREE_DATA_DIR": "/caller/private-store", "TERM": "dumb",
                                         "COLORTERM": "caller", "NO_COLOR": "1"}):
                report = SOAK.run(args)
            self.assertEqual(report["status"], "passed", report.get("error"))
            for name in ("home", "resources", "watch"):
                row = report[name]
                self.assertGreaterEqual(row["actual_observation_seconds"], row["requested_duration_seconds"])
                self.assertGreaterEqual(row["samples"][-1]["elapsed_seconds"], row["requested_duration_seconds"])
                self.assertTrue(row["child_reaped"])
                self.assertTrue(row["terminal_restored"])
                self.assertTrue(row["terminal_file_flags_restored"])
                self.assertEqual(row["exit_code"], 0)
                self.assertEqual(os.stat(row["raw_output"]).st_mode & 0o777, 0o600)
                with self.assertRaises(ProcessLookupError):
                    os.kill(row["pid"], 0)
            self.assertTrue(report["home"]["home_static_verified"])
            self.assertTrue(report["resources"]["refresh_verified"])
            self.assertTrue(report["watch"]["refresh_verified"])
            events = [json.loads(line) for line in Path(report["resources"]["events_file"]).read_text().splitlines()]
            names = [row["event"] for row in events]
            self.assertIn("home_verified_before_navigation", names)
            self.assertLess(names.index("home_verified_before_navigation"), names.index("observation_started"))

    def test_early_exit_is_failed_not_valid_zero_metrics_and_all_children_reaped(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            binary = folder / "early-exit"
            binary.write_text("#!" + sys.executable + "\nraise SystemExit(7)\n")
            binary.chmod(0o700)
            args = argparse.Namespace(binary=binary, output=folder / "soak.json",
                                      home_seconds=.3, resources_seconds=.4,
                                      watch_seconds=.4, sample_interval=.08)
            report = SOAK.run(args)
            self.assertEqual(report["status"], "failed")
            self.assertIn("exited before", report["error"])
            for name in ("home", "resources", "watch"):
                self.assertTrue(report[name]["child_reaped"])
                self.assertEqual(report[name]["status"], "failed")
                self.assertIsNone(report[name]["average_cpu_one_core_percent"])
                self.assertIsNone(report[name]["rss_p95_bytes"])

    def test_relative_caller_data_dir_is_ignored(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {"BREE_DATA_DIR": "relative"}):
            args = argparse.Namespace(binary=Path("unused"), output=Path(directory) / "soak.json")
            with patch.object(SOAK, "observe") as observe:
                SOAK.run(args)
            store = Path(observe.call_args.args[2]["BREE_DATA_DIR"])
            self.assertTrue(store.is_absolute())
            self.assertEqual(store.parent.parent, Path(directory).resolve())
            self.assertFalse(store.parent.exists())

    def test_screen_failure_still_reaps_owned_children(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            binary = folder / "fake-bree"
            binary.write_text(FAKE.replace("INTERPRETER", sys.executable))
            binary.chmod(0o700)
            args = argparse.Namespace(binary=binary, output=folder / "soak.json",
                                      home_seconds=.3, resources_seconds=.4,
                                      watch_seconds=.4, sample_interval=.08)
            with patch.object(SOAK.Screen, "feed", side_effect=RuntimeError("screen error")):
                report = SOAK.run(args)
            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["error"], "screen error")
            for name in ("home", "resources", "watch"):
                self.assertTrue(report[name]["child_reaped"])
                self.assertEqual(report[name]["cleanup_actions"], ["q"])
                with self.assertRaises(ProcessLookupError):
                    os.kill(report[name]["pid"], 0)

    def test_unstarted_scenarios_remain_not_run_after_spawn_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            args = argparse.Namespace(binary=folder / "missing", output=folder / "soak.json",
                                      home_seconds=.3, resources_seconds=.4,
                                      watch_seconds=.4, sample_interval=.08)
            report = SOAK.run(args)
            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["home"]["status"], "failed")
            self.assertEqual(report["resources"]["status"], "not-run")
            self.assertEqual(report["watch"]["status"], "not-run")


if __name__ == "__main__":
    unittest.main()
