#!/usr/bin/env python3
"""Bounded cancellation checks on private Bree children; retain failed evidence.

The stability runner holds the shared native-experiment lock around this script. For
standalone experiments the caller must hold that same lock through child reaping.
"""
import argparse
import fcntl
import hashlib
import json
import os
import pty
import select
import signal
import struct
import subprocess
import termios
import time
from pathlib import Path

from check_env import bree_environment, empty_bree_environment


# Darwin FCNTLFLAGS: exclude kernel write-history FWASWRITTEN, which F_SETFL
# cannot clear. PENDIN is transient line-discipline state, not a raw-mode bit.
MUTABLE_FLAGS = os.O_APPEND | os.O_ASYNC | os.O_SYNC | os.O_DSYNC | os.O_NONBLOCK
OUTPUT_LIMIT = 512 * 1024
HOME_MARKER = b"Preview"


def modes(fd):
    value = termios.tcgetattr(fd)
    value[3] &= ~termios.PENDIN
    value[6] = [item if isinstance(item, int) else item[0] for item in value[6]]
    return value


def terminal_state(fd):
    attributes = modes(fd)
    return {
        "modes": attributes,
        "settable_flags": fcntl.fcntl(fd, fcntl.F_GETFL) & MUTABLE_FLAGS,
        "canonical": bool(attributes[3] & termios.ICANON),
        "echo": bool(attributes[3] & termios.ECHO),
        "terminal_signals": bool(attributes[3] & termios.ISIG),
    }


def run_case(binary, case, requested_signal, close_master, args, iteration):
    start = time.monotonic()
    record = {
        "case": case,
        "iteration": iteration,
        "status": "failed",
        "signal": requested_signal.name,
        "exit_code": None,
        "child_reaped": False,
        "readiness_observed": False,
        "usable_result_observed": False,
        "signal_send_attempted": False,
        "signal_delivery": "not_attempted",
        "expected_exit_codes": [1, 130] if case == "eof_during_initialization" else [130],
        "terminal_modes_and_flags_restored": None,
        "show_cursor_observed": None,
        "leave_alternate_screen_observed": None,
        "events": [],
        "errors": [],
    }
    output = bytearray()
    total_output_bytes = 0
    master = slave = child = None

    def mark(event, **details):
        record["events"].append({"event": event,
                                 "elapsed_ms": round((time.monotonic() - start) * 1000, 3),
                                 **details})

    def drain(timeout):
        nonlocal total_output_bytes
        if master is None:
            time.sleep(timeout)
            return
        if select.select([master], [], [], timeout)[0]:
            chunk = os.read(master, 65536)
            if chunk:
                if total_output_bytes == 0:
                    mark("first_pty_output")
                total_output_bytes += len(chunk)
                output.extend(chunk[:max(0, OUTPUT_LIMIT - len(output))])
                if not record["readiness_observed"] and HOME_MARKER in output:
                    record["readiness_observed"] = True
                    mark("first_home_frame_observed", terminal=terminal_state(slave))
                if not record["usable_result_observed"] and b"GiB" in output:
                    record["usable_result_observed"] = True
                    mark("first_usable_result_observed")
                return True
        return False

    try:
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 100, 0, 0))
        before = terminal_state(slave)
        mark("before_spawn", terminal=before)
        child = subprocess.Popen([str(binary)], stdin=slave, stdout=slave, stderr=slave,
                                 env=bree_environment(args.data_dir))
        mark("spawn_returned", pid=child.pid)
        if args.send_after_ms is None:
            deadline = time.monotonic() + args.startup_timeout
            while not record["readiness_observed"] and time.monotonic() < deadline:
                drain(.02)
                if child.poll() is not None:
                    raise RuntimeError("Bree exited before its first Home frame")
            if not record["readiness_observed"]:
                raise RuntimeError("Bree did not draw its first Home frame within the startup bound")
        else:
            # Diagnostic mode preserves the old timer-only send for comparison.
            # A default OS signal exit before readiness does not prove a TUI fault.
            deadline = time.monotonic() + args.send_after_ms / 1000
            while time.monotonic() < deadline:
                drain(min(.02, max(0, deadline - time.monotonic())))
        if close_master and case != "eof_during_initialization" and args.send_after_ms is None:
            # Preserve the established trapped-reader experiment separately from
            # terminal loss while initialization or a first draw is still active.
            deadline = time.monotonic() + args.startup_timeout
            while not record["usable_result_observed"] and time.monotonic() < deadline:
                drain(.02)
                if child.poll() is not None:
                    raise RuntimeError("Bree exited before its first usable result")
            if not record["usable_result_observed"]:
                raise RuntimeError("Bree did not show its first usable result within the startup bound")
            settle_deadline = time.monotonic() + .1
            while time.monotonic() < settle_deadline:
                drain(.01)
        if close_master:
            mark("before_terminal_disconnect", terminal=terminal_state(slave))
            os.close(master)
            master = None
            mark("master_closed")
            # Let the dependency observe EOF first to verify the trapped-read path.
            time.sleep(.1)
        observed_exit = child.poll()
        mark("before_signal", terminal=None if master is None else terminal_state(slave),
             readiness_observed=record["readiness_observed"], child_exit_code=observed_exit)
        if case == "eof_during_initialization" and observed_exit is not None:
            # Runtime terminal loss may complete before the later signal. Retain
            # its real error code, rather than pretending it was cancellation.
            record["signal"] = None
            mark("exited_before_signal", exit_code=observed_exit)
        elif observed_exit is not None:
            raise RuntimeError(f"Bree exited before signal delivery with code {observed_exit}")
        else:
            # Popen.send_signal may observe an exit between our poll and its own
            # poll. A send attempt does not prove that the handler ran.
            record["signal_send_attempted"] = True
            record["signal_delivery"] = "unknown"
            child.send_signal(requested_signal)
        signal_sent = time.monotonic()
        if record["signal_send_attempted"]:
            mark("signal_send_attempted")
        deadline = signal_sent + args.exit_timeout
        while child.poll() is None and time.monotonic() < deadline:
            drain(.02)
        if child.poll() is None:
            raise RuntimeError("Bree did not cancel within the exit bound")
        record["exit_code"] = child.wait(timeout=1)
        record["child_reaped"] = True
        record["cancel_ms"] = (round((time.monotonic() - signal_sent) * 1000, 3)
                               if record["signal_send_attempted"] and child.returncode == 130 else None)
        record["outcome"] = ("cancelled" if child.returncode == 130 else
                             "runtime_error_before_signal" if not record["signal_send_attempted"] else
                             "runtime_error_signal_race" if child.returncode == 1 else "unexpected_exit")
        mark("child_reaped", exit_code=child.returncode)
        if master is not None:
            # The child has exited; consume all remaining restoration bytes.
            while drain(0):
                pass
            after = terminal_state(slave)
            mark("after_exit", terminal=after)
            record["terminal_modes_and_flags_restored"] = (
                after["modes"] == before["modes"]
                and after["settable_flags"] == before["settable_flags"]
            )
            record["show_cursor_observed"] = b"\x1b[?25h" in output
            record["leave_alternate_screen_observed"] = b"\x1b[?1049l" in output
            if not record["terminal_modes_and_flags_restored"]:
                record["errors"].append("terminal modes or settable file flags were not restored")
            if not record["show_cursor_observed"]:
                record["errors"].append("cursor restoration sequence was not observed")
            if not record["leave_alternate_screen_observed"]:
                record["errors"].append("alternate-screen restoration sequence was not observed")
        if child.returncode not in record["expected_exit_codes"]:
            record["errors"].append(
                f"unexpected exit code {child.returncode}; expected one of {record['expected_exit_codes']}"
            )
        if not record["errors"]:
            record["status"] = "passed"
    except Exception as error:
        record["errors"].append(f"{type(error).__name__}: {error}")
    finally:
        # Close endpoints before failure cleanup: an unread master can block macOS
        # terminal teardown. Only this exact Popen child is ever killed or waited on.
        for descriptor in (master, slave):
            if descriptor is not None:
                os.close(descriptor)
        if child is not None:
            if child.poll() is None:
                child.kill()
                mark("failure_cleanup_sigkill")
            try:
                record["exit_code"] = child.wait(timeout=args.exit_timeout)
                record["child_reaped"] = True
            except subprocess.TimeoutExpired:
                record["errors"].append("private test child could not be reaped within the cleanup bound")
                record["status"] = "failed"
        record["pty_output_bytes"] = total_output_bytes
        record["pty_capture_truncated"] = total_output_bytes > OUTPUT_LIMIT
    return record


def positive_int(value):
    result = int(value)
    if not 1 <= result <= 100:
        raise argparse.ArgumentTypeError("must be between 1 and 100")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/bree"))
    parser.add_argument("--output", type=Path, default=Path(".artifacts/signal-check.json"))
    parser.add_argument("--data-dir", type=Path,
                        help="explicit isolated BREE_DATA_DIR; default is a fresh private store beside the report")
    parser.add_argument("--repeat", type=positive_int, default=1)
    parser.add_argument("--startup-timeout", type=positive_int, default=5)
    parser.add_argument("--exit-timeout", type=positive_int, default=5)
    parser.add_argument("--send-after-ms", type=float,
                        help="diagnostic timer-only send; default waits for the first Home frame")
    args = parser.parse_args()
    if args.send_after_ms is not None and not 0 <= args.send_after_ms <= 10000:
        parser.error("--send-after-ms must be between 0 and 10000")
    binary = args.binary.resolve()
    with empty_bree_environment(args.output) as env:
        args.data_dir = (args.data_dir.resolve() if args.data_dir is not None else
                         Path(env["BREE_DATA_DIR"]))
        results = [
            run_case(binary, case, requested_signal, close_master, args, iteration)
            for iteration in range(args.repeat)
            for case, requested_signal, close_master in [
                ("sigint", signal.SIGINT, False), ("sigterm", signal.SIGTERM, False),
                ("sighup", signal.SIGHUP, False), ("sighup_after_eof", signal.SIGHUP, True),
                ("eof_during_initialization", signal.SIGHUP, True),
            ]
        ]
    report = {
        "status": "passed" if all(case["status"] == "passed" for case in results) else "failed",
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "data_dir": str(args.data_dir),
        "send_policy": "first_home_frame" if args.send_after_ms is None else "diagnostic_timer",
        "send_after_ms": args.send_after_ms,
        "cases": results,
        "handler_timing": {
            "observation": "upper_bound_from_first_home_frame_and_source_order",
            "source": "src/tui.rs run_internal installs ctrlc before TerminalGuard::enter and first draw",
            "exact_handler_install_timestamp": None,
        },
        "limits": [
            "A first Home frame proves the handler is installed by source order; its exact install time is not observed",
            "restoration cannot be observed after the terminal itself is gone",
            "compares Darwin F_SETFL mutable flags; excludes FWASWRITTEN and transient PENDIN",
            "startup timer diagnostics do not explain a historical failure without corresponding historical timing evidence",
            "PTY checks do not verify real-terminal visual appearance",
        ],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
