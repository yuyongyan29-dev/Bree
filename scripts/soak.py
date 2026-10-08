#!/usr/bin/env python3
"""Observe Home, menu-entered Resources, and interactive watch in private PTYs.

For formal results run through stability-check.py, which identifies the binary and holds
the shared absolute experiment lock until all owned children are reaped. Direct
short runs are diagnostic only. Raw screens can include application names: keep
this result and its sibling evidence under ignored .artifacts/stability/.
"""
import argparse
import fcntl
import json
import math
import os
import pty
import re
import select
import signal
import statistics
import struct
import subprocess
import sys
import termios
import time
from pathlib import Path

from pty_screen import Screen
from check_env import empty_bree_environment

MUTABLE_FLAGS = os.O_APPEND | os.O_ASYNC | os.O_SYNC | os.O_DSYNC | os.O_NONBLOCK
SAMPLE = re.compile(r"(?:Sample )?UTC (\d{2}:\d{2}:\d{2}) · (\d+) ms")


def cpu_seconds(value):
    """Parse ps [[days-]hours:]minutes:seconds, including decimal seconds."""
    days, separator, clock = value.partition("-")
    total = 0.0
    for part in (clock if separator else value).split(":"):
        total = total * 60 + float(part)
    return total + (int(days) * 86400 if separator else 0)


def page_state(text):
    home = ("Memory pressure:" in text and "3. Memory" in text
            and "Enter Open" in text)
    resources = ("bree Memory" in text and "/ Search names, Bundle ID or exact PID" in text
                 and "Esc Home" in text and "refresh" in text and not home)
    sample = SAMPLE.search(text)
    return {"page": "home" if home else "resources" if resources else "unknown",
            "sample_utc": sample.group(1) if sample else None,
            "collection_ms": int(sample.group(2)) if sample else None,
            "refreshing": "Refreshing" in text or "Analyzing" in text}


def sample_process(pid, elapsed):
    result = subprocess.run(["ps", "-p", str(pid), "-o", "rss=,time="],
                            capture_output=True, text=True, timeout=5, check=True)
    values = result.stdout.strip().split()
    if len(values) != 2:
        raise RuntimeError(f"ps did not return RSS and cumulative CPU for owned PID {pid}")
    return {"elapsed_seconds": elapsed, "rss_bytes": int(values[0]) * 1024,
            "cpu_seconds": cpu_seconds(values[1])}


def summarize_samples(samples):
    if len(samples) < 2:
        return {"sample_count": len(samples), "average_cpu_one_core_percent": None,
                "rss_p95_bytes": None, "rss_trend": None}
    elapsed = samples[-1]["elapsed_seconds"] - samples[0]["elapsed_seconds"]
    rss = sorted(row["rss_bytes"] for row in samples)
    times = [row["elapsed_seconds"] for row in samples]
    values = [row["rss_bytes"] for row in samples]
    x_mean, y_mean = statistics.mean(times), statistics.mean(values)
    denominator = sum((value - x_mean) ** 2 for value in times)
    slope = (sum((x - x_mean) * (y - y_mean) for x, y in zip(times, values))
             / denominator if denominator else None)
    windows = {}
    for row in samples:
        windows.setdefault(int(row["elapsed_seconds"] // 60), []).append(row["rss_bytes"])
    medians = [{"start_seconds": number * 60, "sample_count": len(rows),
                "median_bytes": statistics.median(rows)}
               for number, rows in sorted(windows.items()) if len(rows) >= 3]
    growing = (all(after["median_bytes"] > before["median_bytes"]
                   for before, after in zip(medians, medians[1:]))
               if len(medians) >= 3 else None)
    return {"sample_count": len(samples), "sampled_duration_seconds": elapsed,
            "average_cpu_one_core_percent": ((samples[-1]["cpu_seconds"] - samples[0]["cpu_seconds"])
                                              / elapsed * 100 if elapsed > 0 else None),
            "rss_p95_bytes": rss[math.ceil(len(rss) * .95) - 1],
            "rss_max_bytes": max(rss),
            "first_five_rss_median_bytes": statistics.median(values[:5]),
            "last_five_rss_median_bytes": statistics.median(values[-5:]),
            "rss_trend": {"last_minus_first_bytes": values[-1] - values[0],
                          "linear_slope_bytes_per_second": slope,
                          "one_minute_window_medians": medians,
                          "all_window_medians_strictly_increasing": growing,
                          "interpretation": "this sampled interval only; does not prove absence of leaks"}}


def private_file(path, mode):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    return os.fdopen(fd, mode)


class PtyRun:
    def __init__(self, name, command, duration, output, env):
        self.name, self.duration = name, duration
        self.master = self.slave = self.child = self.raw = self.events = None
        self.screen = Screen(100, 28)
        self.start = time.monotonic()
        self.last_bytes = self.start
        self.ready = None
        self.next_sample = 0.0
        self.stopping = None
        self.finished = False
        self.menu_sent = False
        self.last_state = None
        self.result = {"status": "unknown", "command": command, "requested_duration_seconds": duration,
                       "mode": "Home menu -> 3 Enter -> Resources" if name == "resources"
                               else "interactive watch -> Resources" if name == "watch" else "Home idle",
                       "pid": None, "samples": [], "sample_timestamps_utc": [],
                       "terminal_output_bytes": 0, "idle_output_bytes": 0,
                       "exit_code": None, "child_reaped": False,
                       "terminal_restored": None, "terminal_file_flags_restored": None,
                       "cleanup_actions": []}
        try:
            self.master, self.slave = pty.openpty()
            fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 100, 0, 0))
            self.before = termios.tcgetattr(self.slave)
            self.before[3] &= ~termios.PENDIN
            self.flags_before = fcntl.fcntl(self.slave, fcntl.F_GETFL) & MUTABLE_FLAGS
            raw_path = output.with_name(output.stem + "-" + name + ".raw")
            events_path = output.with_name(output.stem + "-" + name + "-events.jsonl")
            self.raw = private_file(raw_path, "wb")
            self.events = private_file(events_path, "w")
            self.result.update(raw_output=str(raw_path), events_file=str(events_path))
            self.child = subprocess.Popen(command, stdin=self.slave, stdout=self.slave,
                                          stderr=self.slave, env=env)
            self.result["pid"] = self.child.pid
        except BaseException:
            self.close()
            raise

    def event(self, event, **details):
        self.events.write(json.dumps({"elapsed_since_launch_seconds": time.monotonic() - self.start,
                                      "event": event, **details}) + "\n")
        self.events.flush()

    def drain(self):
        try:
            chunk = os.read(self.master, 65536)
        except OSError as error:
            if self.child.poll() is None:
                raise RuntimeError(f"{self.name}: PTY read failed: {error}") from error
            return
        if not chunk:
            return
        self.raw.write(chunk)
        self.screen.feed(chunk)
        self.result["terminal_output_bytes"] += len(chunk)
        if self.ready is not None and self.stopping is None and self.name == "home":
            self.result["idle_output_bytes"] += len(chunk)
        self.last_bytes = time.monotonic()

    def tick(self, output, interval):
        now = time.monotonic()
        if self.child.poll() is not None:
            if self.stopping is None:
                raise RuntimeError(f"{self.name}: Bree exited before bounded observation (code {self.child.returncode})")
            self.finish()
            return
        if self.stopping is not None:
            if now - self.stopping > 10:
                raise RuntimeError(f"{self.name}: Q did not exit within 10 seconds")
            return
        stable = now - self.last_bytes >= .03
        state = page_state(self.screen.text())
        usable = stable and state["sample_utc"] is not None and not state["refreshing"]
        if stable and state != self.last_state:
            self.event("rendered_state", **state)
            self.last_state = state
        if self.name == "resources" and not self.menu_sent and usable and state["page"] == "home":
            self.event("home_verified_before_navigation", **state)
            os.write(self.master, b"3\r")
            self.menu_sent = True
            self.event("keys_sent", keys="3 Enter")
            return
        expected = "home" if self.name == "home" else "resources"
        if self.ready is None:
            if now - self.start > 15:
                raise RuntimeError(f"{self.name}: usable {expected} screen not rendered within 15 seconds")
            if usable and state["page"] == expected:
                self.ready = now
                self.result["ready_after_launch_ms"] = (now - self.start) * 1000
                self.result["page_verified"] = expected
                proof = output.with_name(output.stem + "-" + self.name + "-ready.txt")
                with private_file(proof, "w") as handle:
                    handle.write(self.screen.text() + "\n")
                self.result["ready_screen_file"] = str(proof)
                self.event("observation_started", **state)
        if self.ready is None:
            return
        if stable and state["page"] != expected:
            raise RuntimeError(f"{self.name}: observed page changed to {state['page']}")
        if usable and state["page"] == expected:
            stamps = self.result["sample_timestamps_utc"]
            if not stamps or stamps[-1] != state["sample_utc"]:
                stamps.append(state["sample_utc"])
                self.event("sample_timestamp_observed", **state)
        elapsed = now - self.ready
        if elapsed >= self.next_sample or elapsed >= self.duration:
            self.result["samples"].append(sample_process(self.child.pid, elapsed))
            self.next_sample = elapsed + interval
        if elapsed >= self.duration:
            self.result["actual_observation_seconds"] = elapsed
            proof = output.with_name(output.stem + "-" + self.name + "-end.txt")
            with private_file(proof, "w") as handle:
                handle.write(self.screen.text() + "\n")
            self.result["end_screen_file"] = str(proof)
            self.event("keys_sent", keys="Q", actual_observation_seconds=elapsed)
            os.write(self.master, b"q")
            self.stopping = time.monotonic()

    def finish(self):
        self.child.wait(timeout=1)
        self.result.update(exit_code=self.child.returncode, child_reaped=True,
                           exit_after_q_seconds=time.monotonic() - self.stopping if self.stopping else None)
        after = termios.tcgetattr(self.slave)
        after[3] &= ~termios.PENDIN
        self.result["terminal_restored"] = after == self.before
        self.result["terminal_file_flags_restored"] = (
            fcntl.fcntl(self.slave, fcntl.F_GETFL) & MUTABLE_FLAGS) == self.flags_before
        stamps = self.result["sample_timestamps_utc"]
        self.result["refresh_verified"] = len(stamps) > 1 if self.name != "home" else None
        self.result["home_static_verified"] = (len(stamps) == 1 and self.result["idle_output_bytes"] == 0
                                               if self.name == "home" else None)
        valid = (self.child.returncode == 0 and self.result["terminal_restored"]
                 and self.result["terminal_file_flags_restored"]
                 and self.result.get("actual_observation_seconds", 0) >= self.duration
                 and (self.result["home_static_verified"] if self.name == "home"
                      else self.result["refresh_verified"]))
        if self.result["status"] != "failed":
            self.result["status"] = "passed" if valid else "failed"
            if not valid:
                self.result["error"] = "duration, page refresh/idle, Q exit, or terminal restoration check failed"
        self.finished = True
        self.event("child_reaped", exit_code=self.child.returncode,
                   terminal_restored=self.result["terminal_restored"],
                   terminal_file_flags_restored=self.result["terminal_file_flags_restored"])
        print(f"{self.name}: {self.result['status']}, observed {self.result.get('actual_observation_seconds', 0):.2f}s", flush=True)

    def close(self):
        if self.child is not None:
            if self.child.poll() is None:
                for action, timeout in (("q", 2), ("terminate", 3), ("kill", 3)):
                    self.result["cleanup_actions"].append(action)
                    if action == "q":
                        try:
                            os.write(self.master, b"q")
                        except OSError:
                            pass
                    elif action == "terminate":
                        self.child.terminate()
                    else:
                        self.child.kill()
                    deadline = time.monotonic() + timeout
                    while self.child.poll() is None and time.monotonic() < deadline:
                        if select.select([self.master], [], [], .05)[0]:
                            # A screen/recording error is a test failure, but must
                            # not interrupt reaping. Drain without parsing/writing.
                            try:
                                os.read(self.master, 65536)
                            except OSError:
                                break
                    if self.child.poll() is not None:
                        break
            self.child.wait(timeout=3)
            self.result.update(child_reaped=True, exit_code=self.child.returncode)
            if not self.finished:
                after = termios.tcgetattr(self.slave)
                after[3] &= ~termios.PENDIN
                self.result["terminal_restored"] = after == self.before
                self.result["terminal_file_flags_restored"] = (
                    fcntl.fcntl(self.slave, fcntl.F_GETFL) & MUTABLE_FLAGS) == self.flags_before
        for handle in (self.raw, self.events):
            if handle is not None:
                handle.close()
        for descriptor in (self.master, self.slave):
            if descriptor is not None:
                os.close(descriptor)
        self.master = self.slave = None


def run(args):
    output = args.output.resolve()
    with empty_bree_environment(output) as env:
        return observe(args, output, env)


def observe(args, output, env):
    binary = args.binary.resolve()
    sessions = []
    report = {"status": "unknown", "sample_interval_seconds": args.sample_interval,
              "data_dir": env["BREE_DATA_DIR"],
              "limits": ["one live Mac; workload and caches uncontrolled",
                         "three owned Bree PTYs observed concurrently; per-PID CPU/RSS",
                         "CPU is cumulative ps CPU / actual sampled wall time, as percent of one core",
                         "RSS P95 is nearest-rank over periodic samples; not a continuous peak",
                         "terminal cells and bytes are evidence, not physical terminal visual acceptance",
                         "process reaping proves this Bree process and its threads exited",
                         "RSS trends describe only this interval; cannot prove absence of leaks",
                         "formal use requires stability-check.py shared-lock/build wrapper"]}
    report.update({name: {"status": "not-run", "reason": "scenario not started"}
                   for name in ("home", "resources", "watch")})
    error = None
    stopped_at = None
    try:
        for name, command, duration in (("home", [str(binary)], args.home_seconds),
                                        ("resources", [str(binary)], args.resources_seconds),
                                        ("watch", [str(binary), "watch"], args.watch_seconds)):
            try:
                sessions.append(PtyRun(name, command, duration, output, env))
            except Exception as failure:
                report[name] = {"status": "failed", "error": str(failure),
                                "command": command, "requested_duration_seconds": duration}
                raise
        while any(not item.finished for item in sessions):
            active = [item for item in sessions if not item.finished]
            readable = select.select([item.master for item in active], [], [], .05)[0]
            for item in active:
                if item.master in readable:
                    item.drain()
            for item in active:
                item.tick(output, args.sample_interval)
    except (Exception, KeyboardInterrupt) as failure:
        stopped_at = time.monotonic()
        error = str(failure) or type(failure).__name__
        report["error"] = error
    finally:
        cleanup_errors = []
        for item in sessions:
            if not item.finished:
                item.result.update(status="failed", error=error or "observation incomplete")
                if item.ready is not None and "actual_observation_seconds" not in item.result:
                    item.result["actual_observation_seconds"] = (stopped_at or time.monotonic()) - item.ready
            try:
                item.close()
            except Exception as failure:
                item.result.update(status="failed", cleanup_error=str(failure))
                cleanup_errors.append(str(failure))
            item.result.update(summarize_samples(item.result["samples"]))
            report[item.name] = item.result
        if cleanup_errors:
            report["cleanup_errors"] = cleanup_errors
        report["status"] = ("passed" if not error and not cleanup_errors and len(sessions) == 3
                            and all(item.result["status"] == "passed" for item in sessions) else "failed")
        output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    return report


def cancelled(*_):
    raise KeyboardInterrupt("SIGTERM")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/bree"))
    parser.add_argument("--output", type=Path, default=Path(".artifacts/stability/soak.json"))
    parser.add_argument("--home-seconds", type=float, default=300)
    parser.add_argument("--resources-seconds", type=float, default=600)
    parser.add_argument("--watch-seconds", type=float, default=600)
    parser.add_argument("--sample-interval", type=float, default=10)
    args = parser.parse_args()
    if any(not math.isfinite(value) or value <= 0 for value in
           (args.home_seconds, args.resources_seconds, args.watch_seconds, args.sample_interval)):
        parser.error("durations and sample interval must be finite and positive")
    signal.signal(signal.SIGTERM, cancelled)
    report = run(args)
    print(json.dumps({name: {key: value for key, value in row.items() if key != "samples"}
                      for name, row in report.items() if isinstance(row, dict)}, ensure_ascii=False, indent=2))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
