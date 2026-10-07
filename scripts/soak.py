#!/usr/bin/env python3
"""Bounded 5-minute home / 10-minute resource observation in private PTYs."""
import argparse
import fcntl
import json
import os
import pty
import select
import statistics
import struct
import subprocess
import termios
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, default=Path("target/release/bree"))
parser.add_argument("--output", type=Path, default=Path(".artifacts/soak.json"))
args = parser.parse_args()
binary = args.binary.resolve()
mutable_flags = os.O_APPEND | os.O_ASYNC | os.O_SYNC | os.O_DSYNC | os.O_NONBLOCK
children = {}
for name, command, duration in [("home", [str(binary)], 300), ("resources", [str(binary), "watch"], 600)]:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 100, 0, 0))
    before = termios.tcgetattr(slave)
    before[3] &= ~termios.PENDIN
    flags_before = fcntl.fcntl(slave, fcntl.F_GETFL)
    env = os.environ.copy()
    env["TERM"] = "xterm-256color"
    child = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave, env=env)
    children[name] = {"child": child, "master": master, "slave": slave, "before": before, "flags_before": flags_before,
                      "duration": duration, "samples": [], "closed": False, "bytes_received": 0}

def cpu_seconds(text):
    text = text.replace("-", ":")
    total = 0.0
    for part in text.split(":"):
        total = total * 60 + float(part)
    return total

def stop_and_drain(item):
    os.write(item["master"], b"q")
    deadline = time.monotonic() + 10
    while item["child"].poll() is None and time.monotonic() < deadline:
        if select.select([item["master"]], [], [], .05)[0]:
            item["bytes_received"] += len(os.read(item["master"], 65536))
    if item["child"].poll() is None:
        raise RuntimeError("Bree child failed to exit while PTY output was drained")

start = time.monotonic()
next_sample = 0
try:
    while time.monotonic() - start < 605:
        elapsed = time.monotonic() - start
        active = [item for item in children.values() if not item["closed"]]
        if not active:
            break
        readable, _, _ = select.select([item["master"] for item in active], [], [], .5)
        for descriptor in readable:
            chunk = os.read(descriptor, 65536)
            for item in active:
                if item["master"] == descriptor:
                    item["bytes_received"] += len(chunk)
        if elapsed >= next_sample:
            for name, item in children.items():
                if item["closed"]:
                    continue
                if item["child"].poll() is not None:
                    raise RuntimeError(f"{name} exited before bounded observation")
                measured = subprocess.run(["ps", "-p", str(item["child"].pid), "-o", "rss=,time="], capture_output=True, text=True, check=True).stdout.strip().split()
                item["samples"].append({"elapsed_seconds": round(elapsed, 2), "rss_bytes": int(measured[0]) * 1024, "cpu_seconds": cpu_seconds(measured[1])})
            next_sample += 10
        for name, item in children.items():
            if not item["closed"] and elapsed >= item["duration"]:
                stop_and_drain(item)
                item["closed"] = True
                after = termios.tcgetattr(item["slave"])
                after[3] &= ~termios.PENDIN
                if (item["child"].returncode != 0 or after != item["before"]
                        or (fcntl.fcntl(item["slave"], fcntl.F_GETFL) & mutable_flags) != (item["flags_before"] & mutable_flags)):
                    raise RuntimeError(f"{name} exit or terminal restoration failed")
                print(f"{name}: bounded {item['duration']}s observation complete", flush=True)
    report = {"limits": ["one macOS device, live workload uncontrolled", "RSS sampled every 10 seconds", "trend evidence describes this interval only"]}
    for name, item in children.items():
        samples = item["samples"]
        duration = samples[-1]["elapsed_seconds"] - samples[0]["elapsed_seconds"]
        rss = sorted(row["rss_bytes"] for row in samples)
        report[name] = {"duration_seconds": item["duration"], "sample_count": len(samples),
            "average_cpu_one_core_percent": (samples[-1]["cpu_seconds"] - samples[0]["cpu_seconds"]) / duration * 100,
            "rss_p95_bytes": rss[int(len(rss) * .95) - 1],
            "first_five_rss_median_bytes": statistics.median(row["rss_bytes"] for row in samples[:5]),
            "last_five_rss_median_bytes": statistics.median(row["rss_bytes"] for row in samples[-5:]),
            "terminal_restored": True, "terminal_file_flags_restored": True, "exit_code": item["child"].returncode,
            "terminal_output_bytes": item["bytes_received"], "samples": samples}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({name: {key: value for key, value in result.items() if key != "samples"} for name, result in report.items() if isinstance(result, dict)}, ensure_ascii=False, indent=2))
finally:
    for item in children.values():
        if item["child"].poll() is None:
            try:
                stop_and_drain(item)
            except RuntimeError:
                item["child"].terminate()
                try:
                    stop_and_drain(item)
                except RuntimeError:
                    # Bounded failure cleanup of this exact spawned test child only.
                    item["child"].kill()
                    os.close(item["master"])
                    item["master"] = None
                    item["child"].wait(timeout=5)
        if item["master"] is not None:
            os.close(item["master"])
        os.close(item["slave"])
