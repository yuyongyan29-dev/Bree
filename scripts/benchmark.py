#!/usr/bin/env python3
"""Measure this release binary; results are observations, not compatibility claims."""
import argparse
import hashlib
import json
import os
import platform
import re
import statistics
import subprocess
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, default=Path("target/release/bree"))
parser.add_argument("--runs", type=int, default=20)
parser.add_argument("--output", type=Path)
args = parser.parse_args()
binary = args.binary.resolve()
if args.runs < 20:
    parser.error("at least 20 runs are required")
elapsed = []
sampling = []
first = None
observations = []
for _ in range(args.runs):
    start = time.perf_counter()
    result = subprocess.run([str(binary), "status", "--json"], capture_output=True, check=True)
    elapsed.append((time.perf_counter() - start) * 1000)
    row = json.loads(result.stdout)
    first = first or row
    sampling.append(row["collected_in_ms"])
    observations.append({"command": [str(binary), "status", "--json"],
                         "exit_code": result.returncode,
                         "elapsed_ms": elapsed[-1],
                         "collected_in_ms": row["collected_in_ms"],
                         "processes": row["coverage"]["enumerated_processes"]})
timed = subprocess.run(["/usr/bin/time", "-l", str(binary), "watch", "--json", "--count", "5", "--interval", "2"], capture_output=True, check=True)
match = re.search(rb"(\d+)\s+maximum resident set size", timed.stderr)
report = {
    "system": platform.platform(), "architecture": platform.machine(),
    "binary": str(binary), "binary_bytes": binary.stat().st_size,
    "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
    "runs": args.runs, "processes": first["coverage"]["enumerated_processes"],
    "first_launch_ms": elapsed[0],
    "repeated_launch_p95_ms": sorted(elapsed)[int(len(elapsed) * .95) - 1],
    "repeated_launch_median_ms": statistics.median(elapsed),
    "collection_p95_ms": sorted(sampling)[int(len(sampling) * .95) - 1],
    "short_watch_peak_rss_bytes": int(match.group(1)) if match else None,
    "observations": observations,
    "short_watch": {"command": ["/usr/bin/time", "-l", str(binary), "watch", "--json", "--count", "5", "--interval", "2"],
                    "exit_code": timed.returncode,
                    "timing_stderr": timed.stderr.decode(errors="replace")},
    "limits": ["first launch is not a controlled cold-cache measurement", "short watch is not a 10-minute growth test", "PTY skeleton and idle sampling measured separately", "only this device and OS tested"],
}
print(json.dumps(report, ensure_ascii=False, indent=2))
if args.output:
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
