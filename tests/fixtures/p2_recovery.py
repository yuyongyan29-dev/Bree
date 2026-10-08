"""Read-only owned-process recovery checks for the native experiment wrappers."""
import ctypes
from dataclasses import dataclass
import errno
import json
import os
import pathlib
import signal
import subprocess
import time

proc = ctypes.CDLL("/usr/lib/libproc.dylib")
proc.proc_listallpids.argtypes = [ctypes.c_void_p, ctypes.c_int]
proc.proc_listallpids.restype = ctypes.c_int
proc.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
proc.proc_pidpath.restype = ctypes.c_int


def _all_pids():
    count = proc.proc_listallpids(None, 0)
    if count <= 0:
        raise RuntimeError("Cannot determine the process-list size")
    capacity = count + 64
    for _ in range(3):
        buffer = (ctypes.c_int * capacity)()
        count = proc.proc_listallpids(buffer, ctypes.sizeof(buffer))
        if count <= 0:
            raise RuntimeError("Cannot read the process list")
        if count < capacity:
            return {pid for pid in buffer[:count] if pid > 0}
        capacity *= 2
    raise RuntimeError("Cannot confirm a complete process list")


def _manifest_pids(manifest, executable):
    try:
        lines = manifest.read_text().splitlines()
    except (OSError, UnicodeError):
        return set(), False
    pids = set()
    complete = True
    for line in lines:
        try:
            child = json.loads(line)
            pid = child["pid"]
            if not isinstance(pid, int) or isinstance(pid, bool) or pid <= 0:
                raise ValueError("Invalid owned PID")
            if pathlib.Path(child["executable"]).resolve() == executable:
                pids.add(pid)
        except (ValueError, TypeError, KeyError, OSError):
            complete = False
    return pids, complete


class RecoveryUnconfirmed(RuntimeError):
    """The caller has no positive scope with which to confirm child recovery."""


@dataclass(frozen=True)
class NativeRunResult:
    controller_exit_code: int
    wrapper_exit_code: int
    received_signals: tuple


def _note(message):
    try:
        print(message, flush=True)
    except OSError:
        # Diagnostics must not break recovery or release the caller's lock.
        pass


def _group_exists(pgid):
    try:
        os.killpg(pgid, 0)  # read-only; never signals or terminates the group
        return True
    except ProcessLookupError:
        return False
    except OSError as error:
        if error.errno == errno.ESRCH:
            return False
        # Permission and other errors are unknown, never evidence of absence.
        raise RecoveryUnconfirmed("Cannot confirm owned process group: " + str(error)) from error


def wait_owned(run_root, pgid=None, controller=None):
    # Only run_owned supplies this scope, from a controller started in a new
    # session. A parseable manifest is not a certificate of spawn coverage.
    if not isinstance(pgid, int) or isinstance(pgid, bool) or pgid <= 0 or pgid == os.getpgrp():
        raise RecoveryUnconfirmed("Recovery requires the controller's isolated process group")
    while True:
        try:
            return _wait_owned(run_root, pgid, controller)
        except KeyboardInterrupt:
            _note("Owned fixture recovery interrupted; retaining native lock")


def _wait_owned(run_root, pgid, controller):
    run_root = pathlib.Path(run_root).resolve()
    manifest = run_root / "owned-children.jsonl"
    executable = (run_root / "build/BreeQuitFixture.app/Contents/MacOS/BreeQuitFixture").resolve()
    started = time.monotonic()
    warned = False
    enumeration_warned = False
    manifest_warned = False
    while True:
        owned_pids, complete_manifest = _manifest_pids(manifest, executable)
        if not complete_manifest and not manifest_warned:
            _note("Owned fixture manifest missing or incomplete; confirming isolated group and exact binary")
            manifest_warned = True
        try:
            if controller is not None:
                controller.poll()  # reap the controller even if its wait raised
            group_pending = _group_exists(pgid)
            pids = _all_pids()
        except (OSError, RuntimeError, OverflowError) as error:
            if not enumeration_warned:
                _note("Owned fixture recovery cannot be confirmed; retaining native lock: " + str(error))
                enumeration_warned = True
            time.sleep(0.2)
            continue
        pending = []
        for pid in pids | owned_pids:
            buffer = ctypes.create_string_buffer(4096)
            size = proc.proc_pidpath(pid, buffer, len(buffer))
            try:
                path = pathlib.Path(os.fsdecode(buffer.value)).resolve() if size > 0 else None
            except (OSError, ValueError):
                path = None
            # A bare manifest PID can be reused by an unrelated process. Only
            # the dedicated group covers unreadable direct children; group
            # absence closes that scope without tracking a reused bare PID.
            if path == executable:
                pending.append(pid)
        if not group_pending and not pending:
            return
        if time.monotonic() - started >= 65 and not warned:
            # Keep the lock held. An unconfirmed recovery is never permission to
            # kill a child or start a second foreground/native experiment.
            _note("Owned fixture recovery still pending; retaining native lock: "
                  + str({"pgid": pgid, "group_exists": group_pending, "pids": pending}))
            warned = True
        time.sleep(0.2)


def run_owned(command, run_root, lock_fd, **kwargs):
    """Caller holds flock; keep it through wait failures and child recovery.

    The fixture and Rust controller directly spawn children and never detach
    them from this new session. Group absence therefore covers the spawn-to-
    manifest gap even when executable paths cannot be read. Exact-path scans
    additionally catch readable fixtures outside that group.
    """
    interrupted = []
    previous = {}

    def defer_signal(number, _frame):
        interrupted.append(number)
        _note("Native wrapper received signal; retaining lock through owned recovery: " + str(number))

    try:
        for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            previous[number] = signal.signal(number, defer_signal)
        child = subprocess.Popen(command, start_new_session=True, pass_fds=(lock_fd,), **kwargs)
        try:
            (pathlib.Path(run_root) / "native-process-group.json").write_text(json.dumps({
                "controller_pid": child.pid, "pgid": child.pid,
                "start_new_session": True, "direct_children_must_not_detach": True,
            }, indent=2) + "\n")
            exit_code = child.wait()
        finally:
            # Includes exceptions in wait and metadata writes. No success result
            # is returned until both the group and fixture evidence are absent.
            wait_owned(run_root, child.pid, controller=child)
        return NativeRunResult(exit_code, 128 + interrupted[0] if interrupted else exit_code,
                               tuple(interrupted))
    finally:
        for number, handler in previous.items():
            signal.signal(number, handler)
