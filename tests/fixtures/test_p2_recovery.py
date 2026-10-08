"""Pure recovery checks: fake libproc only, no native app or process control."""
import ctypes
import json
import pathlib
import runpy
import tempfile
import unittest
from unittest import mock

import p2_recovery as recovery


ROOT = pathlib.Path("/bree/.artifacts/p2/unit-test")
EXECUTABLE = (ROOT / "build/BreeQuitFixture.app/Contents/MacOS/BreeQuitFixture").resolve()
MANIFEST = json.dumps({"pid": 101, "executable": str(EXECUTABLE), "case": "test"}) + "\n"
PGID = 70101


class FakeProc:
    def __init__(self, paths=None, enumeration_failures=0, groups=None):
        self.paths = {998: "/usr/bin/unrelated", **(paths or {})}
        self.pids = set(self.paths)
        self.enumeration_failures = enumeration_failures
        self.enumerations = 0
        self.groups = {101: PGID, **(groups or {})}
        self.group_checks = []

    def proc_listallpids(self, buffer, size):
        if buffer is None:
            self.enumerations += 1
            if self.enumeration_failures:
                self.enumeration_failures -= 1
                return -1
            return len(self.pids)
        for index, pid in enumerate(sorted(self.pids)):
            buffer[index] = pid
        return len(self.pids)

    def proc_pidpath(self, pid, buffer, size):
        path = self.paths.get(pid) if pid in self.pids else None
        if path is None:
            return 0
        data = path.encode() + b"\0"
        ctypes.memmove(buffer, data, len(data))
        return len(data) - 1

    def exists(self, pid, signal):
        assert signal == 0
        if pid not in self.pids:
            raise ProcessLookupError()

    def group_exists(self, pgid, signal):
        assert pgid == PGID and signal == 0
        self.group_checks.append(pgid)
        if not any(self.groups.get(pid) == pgid for pid in self.pids):
            raise ProcessLookupError()


class RecoveryTests(unittest.TestCase):
    def wait(self, proc, manifest=MANIFEST, on_sleep=None, print_error=None):
        read = mock.patch.object(pathlib.Path, "read_text", return_value=manifest)
        if isinstance(manifest, Exception):
            read = mock.patch.object(pathlib.Path, "read_text", side_effect=manifest)
        with read, mock.patch.object(recovery, "proc", proc), \
                mock.patch.object(recovery.os, "kill", side_effect=proc.exists) as existence, \
                mock.patch.object(recovery.os, "killpg", side_effect=proc.group_exists), \
                mock.patch.object(recovery.time, "sleep", side_effect=on_sleep) as sleep, \
                mock.patch("builtins.print", side_effect=print_error) as output:
            recovery.wait_owned(ROOT, PGID)
        return sleep, existence, output

    def test_missing_manifest_still_finds_exact_binary(self):
        proc = FakeProc({101: str(EXECUTABLE)})
        sleep, _, _ = self.wait(proc, FileNotFoundError(), lambda _: proc.pids.remove(101))
        self.assertEqual(sleep.call_count, 1)
        self.assertEqual(proc.enumerations, 2)

    def test_truncated_manifest_still_finds_exact_binary(self):
        proc = FakeProc({101: str(EXECUTABLE)})
        sleep, _, output = self.wait(proc, '{"pid": 101', lambda _: proc.pids.remove(101))
        self.assertEqual(sleep.call_count, 1)
        self.assertIn("manifest missing or incomplete", output.call_args_list[0].args[0])

    def test_missing_manifest_without_exact_binary_still_enumerates(self):
        proc = FakeProc()
        sleep, _, _ = self.wait(proc, FileNotFoundError())
        self.assertEqual(proc.enumerations, 1)
        sleep.assert_not_called()

    def test_missing_manifest_and_unreadable_live_fixture_does_not_finish(self):
        proc = FakeProc({101: None})
        sleep, _, _ = self.wait(proc, FileNotFoundError(), lambda _: proc.pids.remove(101))
        self.assertEqual(sleep.call_count, 1, "live fixture was incorrectly confirmed as recovered")

    def test_truncated_manifest_and_unreadable_live_fixture_does_not_finish(self):
        proc = FakeProc({101: None})
        sleep, _, _ = self.wait(proc, '{"pid": 101', lambda _: proc.pids.remove(101))
        self.assertEqual(sleep.call_count, 1, "live fixture was incorrectly confirmed as recovered")

    def test_empty_manifest_and_unreadable_live_fixture_does_not_finish(self):
        proc = FakeProc({101: None})
        sleep, _, _ = self.wait(proc, "", lambda _: proc.pids.remove(101))
        self.assertEqual(sleep.call_count, 1)

    def test_parseable_manifest_omitting_live_fixture_does_not_finish(self):
        proc = FakeProc({101: None})
        omitted = json.dumps({"pid": 99, "executable": str(EXECUTABLE)}) + "\n"
        sleep, _, _ = self.wait(proc, omitted, lambda _: proc.pids.remove(101))
        self.assertEqual(sleep.call_count, 1)

    def test_unreadable_process_outside_owned_group_does_not_wait(self):
        proc = FakeProc({202: None}, groups={202: PGID + 1})
        sleep, existence, _ = self.wait(proc, FileNotFoundError())
        sleep.assert_not_called()
        existence.assert_not_called()

    def test_unreadable_reused_manifest_pid_outside_group_does_not_wait(self):
        proc = FakeProc({101: None}, groups={101: PGID + 1})
        sleep, existence, _ = self.wait(proc)
        sleep.assert_not_called()
        existence.assert_not_called()

    def test_missing_group_scope_is_explicitly_unconfirmed(self):
        with self.assertRaisesRegex(recovery.RecoveryUnconfirmed, "isolated process group"):
            recovery.wait_owned(ROOT)

    def test_current_group_is_not_an_owned_scope(self):
        with self.assertRaises(recovery.RecoveryUnconfirmed):
            recovery.wait_owned(ROOT, recovery.os.getpgrp())

    def test_group_errors_retry_instead_of_proving_exit(self):
        for error in (PermissionError(), OSError("unavailable")):
            with self.subTest(error=error):
                proc = FakeProc()
                with mock.patch.object(proc, "group_exists", side_effect=[error, ProcessLookupError()]):
                    sleep, _, output = self.wait(proc, FileNotFoundError())
                self.assertEqual(sleep.call_count, 1)
                self.assertTrue(any("retaining native lock" in call.args[0] for call in output.call_args_list))

    def test_exact_fixture_outside_group_is_still_pending(self):
        proc = FakeProc({101: str(EXECUTABLE)}, groups={101: PGID + 1})
        sleep, _, _ = self.wait(proc, FileNotFoundError(), lambda _: proc.pids.remove(101))
        self.assertEqual(sleep.call_count, 1)

    def test_broken_diagnostic_pipe_does_not_break_owned_recovery(self):
        proc = FakeProc({101: None})
        sleep, _, _ = self.wait(proc, FileNotFoundError(), lambda _: proc.pids.remove(101),
                                print_error=BrokenPipeError())
        self.assertEqual(sleep.call_count, 1)

    def test_live_owned_binary_waits_until_absent(self):
        proc = FakeProc({101: str(EXECUTABLE)})
        sleep, existence, _ = self.wait(proc, on_sleep=lambda _: proc.pids.remove(101))
        self.assertEqual(sleep.call_count, 1)
        existence.assert_not_called()

    def test_reused_pid_with_different_executable_is_not_pending(self):
        proc = FakeProc({101: "/usr/bin/unrelated"}, groups={101: PGID + 1})
        sleep, existence, _ = self.wait(proc)
        sleep.assert_not_called()
        existence.assert_not_called()

    def test_unreadable_manifest_pid_remains_pending_while_alive(self):
        proc = FakeProc({101: None})
        sleep, existence, _ = self.wait(proc, on_sleep=lambda _: proc.pids.remove(101))
        self.assertEqual(sleep.call_count, 1)
        self.assertEqual(proc.group_checks, [PGID, PGID])
        existence.assert_not_called()

    def test_permission_error_is_not_exit_evidence(self):
        proc = FakeProc()
        with mock.patch.object(proc, "group_exists", side_effect=[PermissionError(), ProcessLookupError()]):
            sleep, existence, _ = self.wait(proc)
        self.assertEqual(sleep.call_count, 1)
        existence.assert_not_called()

    def test_enumeration_failure_retries_before_proving_absence(self):
        proc = FakeProc(enumeration_failures=1)
        sleep, _, output = self.wait(proc, FileNotFoundError())
        self.assertEqual(sleep.call_count, 1)
        self.assertEqual(proc.enumerations, 2)
        self.assertTrue(any("retaining native lock" in call.args[0] for call in output.call_args_list))

    def test_full_process_buffer_does_not_prove_absence(self):
        proc = FakeProc()
        original = proc.proc_listallpids

        def full_buffer(buffer, size):
            return original(buffer, size) if buffer is None else len(buffer)

        with mock.patch.object(proc, "proc_listallpids", side_effect=full_buffer), \
                mock.patch.object(recovery, "proc", proc):
            with self.assertRaisesRegex(RuntimeError, "complete process list"):
                recovery._all_pids()


class WrapperTests(unittest.TestCase):
    def run_controller(self, wait_result=0, wait_error=None, metadata_error=None):
        child = mock.Mock(pid=PGID)
        child.wait.return_value = wait_result
        child.wait.side_effect = wait_error
        write = mock.patch.object(pathlib.Path, "write_text", side_effect=metadata_error)
        with mock.patch.object(recovery.subprocess, "Popen", return_value=child) as spawn, \
                mock.patch.object(recovery, "wait_owned") as confirm, write:
            try:
                result = recovery.run_owned(["owned-controller"], ROOT, 7)
            finally:
                spawn.assert_called_once_with(["owned-controller"], start_new_session=True, pass_fds=(7,))
                confirm.assert_called_once_with(ROOT, PGID, controller=child)
        return result

    def test_abnormal_controller_exit_runs_recovery(self):
        result = self.run_controller(wait_result=73)
        self.assertEqual(result.controller_exit_code, 73)
        self.assertEqual(result.wrapper_exit_code, 73)
        self.assertEqual(result.received_signals, ())

    def test_wait_exception_runs_recovery_and_does_not_return_success(self):
        for error in (KeyboardInterrupt(), OSError("wait failed")):
            with self.subTest(error=error), self.assertRaises(type(error)):
                self.run_controller(wait_error=error)

    def test_group_record_write_failure_still_runs_recovery(self):
        with self.assertRaises(OSError):
            self.run_controller(metadata_error=OSError("write failed"))

    def test_deferred_signal_is_not_mislabeled_as_controller_exit(self):
        handlers = {}

        def install(number, handler):
            handlers[number] = handler

        child = mock.Mock(pid=PGID)

        def wait():
            handlers[recovery.signal.SIGTERM](recovery.signal.SIGTERM, None)
            return 0

        child.wait.side_effect = wait
        with mock.patch.object(recovery.signal, "signal", side_effect=install), \
                mock.patch.object(recovery.subprocess, "Popen", return_value=child), \
                mock.patch.object(recovery, "wait_owned") as confirm, \
                mock.patch.object(pathlib.Path, "write_text"), \
                mock.patch("builtins.print", side_effect=BrokenPipeError()):
            result = recovery.run_owned(["owned-controller"], ROOT, 7)
        self.assertEqual(result.controller_exit_code, 0)
        self.assertEqual(result.wrapper_exit_code, 143)
        self.assertEqual(result.received_signals, (recovery.signal.SIGTERM,))
        confirm.assert_called_once_with(ROOT, PGID, controller=child)

    def test_legacy_wrapper_certifies_only_confirmed_recovery(self):
        artifacts = pathlib.Path(__file__).resolve().parents[2] / ".artifacts"
        artifacts.mkdir(exist_ok=True)
        for failed in (False, True):
            with self.subTest(failed=failed), tempfile.TemporaryDirectory(prefix="bree-p2-lock-", dir=artifacts) as directory:
                root = pathlib.Path(directory)
                lock = root / "native-experiment.lock"
                argv = ["p2_lock.py", str(lock), "owned-controller", str(EXECUTABLE), str(root)]
                result = recovery.NativeRunResult(73, 73, ())
                with mock.patch("sys.argv", argv), \
                        mock.patch.object(recovery, "run_owned", return_value=result,
                                          side_effect=recovery.RecoveryUnconfirmed("unknown") if failed else None):
                    if failed:
                        with self.assertRaises(recovery.RecoveryUnconfirmed):
                            runpy.run_path(str(pathlib.Path(__file__).with_name("p2_lock.py")))
                    else:
                        with self.assertRaises(SystemExit) as stopped:
                            runpy.run_path(str(pathlib.Path(__file__).with_name("p2_lock.py")))
                        self.assertEqual(stopped.exception.code, 73)
                certificate = root / "native-recovery-confirmed.json"
                self.assertEqual(certificate.exists(), not failed)
                if certificate.exists():
                    self.assertEqual(json.loads(certificate.read_text())["recovery_confirmed"], True)


if __name__ == "__main__":
    unittest.main()
