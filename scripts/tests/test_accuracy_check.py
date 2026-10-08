import copy
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch, MagicMock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
SPEC = importlib.util.spec_from_file_location('accuracy_check', Path(__file__).parents[1] / 'accuracy-check.py')
ACCURACY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ACCURACY)

VM = '''Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages wired down: 200.
Pages purgeable: 50.
Anonymous pages: 1000.
Pages occupied by compressor: 300.
Pages stored in compressor: 800.
File-backed pages: 500.
'''
SWAP = 'total = 2048.00M  used = 1.5M  free = 2046.50M (encrypted)'


def metric(value, status='ok', reason=None):
    return dict(value=value, status=status, reason=reason)


def process(pid, value, status='ok'):
    return dict(identity={'pid': pid}, name='fixture', category='unknown',
                memory_bytes=metric(value, status, 'fixture denial' if status != 'ok' else None), metric_kind='rss')


def system():
    return dict(used_bytes=metric(1450 * 16384), total_bytes=metric(16 * 1024**3),
                compressed_bytes=metric(300 * 16384), swap_used_bytes=metric(1572864))


def commands():
    texts = dict(status=json.dumps({'schema_version': 2, 'system': system()}),
                 list=json.dumps({'schema_version': 2, 'processes': [process(42, 1024)]}),
                 vm_stat=VM, total=str(16 * 1024**3), swap=SWAP, pagesize='16384', ps='42 1')
    return {name: dict(exit_code=0, stdout=text, stderr='', started_unix_ns=1050000000,
                       finished_unix_ns=1150000000, started_monotonic_ns=50000000,
                       finished_monotonic_ns=150000000) for name, text in texts.items()}


class ParserTests(unittest.TestCase):
    def test_vm_mapping_uses_occupied_not_stored_compressor_pages(self):
        page, counts = ACCURACY.parse_vm_stat(VM)
        self.assertEqual(page, 16384)
        self.assertEqual(counts, dict(wire_count=200, purgeable_count=50,
                                     internal_page_count=1000, compressor_page_count=300))
        result = ACCURACY.system_comparison(system(), VM, str(16 * 1024**3), SWAP, '16384')
        self.assertTrue(all(row['within_tolerance'] for row in result['comparisons'].values()))
        self.assertEqual(result['comparisons']['used_bytes']['reference_bytes'], 1450 * 16384)

    def test_missing_counter_remains_unavailable_not_zero(self):
        result = ACCURACY.system_comparison(system(), VM.replace('Anonymous pages: 1000.\n', ''),
                                             str(16 * 1024**3), SWAP, '16384')
        self.assertEqual(result['missing_counters'], ['internal_page_count'])
        used = result['comparisons']['used_bytes']
        self.assertIsNone(used['reference_bytes'])
        self.assertIsNone(used['within_tolerance'])
        self.assertEqual(used['status'], 'unavailable')
        self.assertTrue(result['comparisons']['compressed_bytes']['within_tolerance'])

    def test_unavailable_bree_metric_keeps_its_actual_reason(self):
        source = system()
        source['swap_used_bytes'] = metric(None, 'denied', 'OS permission denied')
        result = ACCURACY.system_comparison(source, VM, str(16 * 1024**3), SWAP, '16384')
        self.assertEqual(result['comparisons']['swap_used_bytes']['reason'], 'OS permission denied')

    def test_vm_duplicate_malformed_header_and_page_mismatch_fail(self):
        for text in [VM + 'Anonymous pages: 1.\n', VM.replace('16384', '0'), 'invalid']:
            with self.assertRaises(ValueError):
                ACCURACY.parse_vm_stat(text)
        with self.assertRaisesRegex(ValueError, 'disagree'):
            ACCURACY.system_comparison(system(), VM, '1024', SWAP, '4096')

    def test_saturating_internal_subtraction_matches_rust_formula(self):
        result = ACCURACY.system_comparison(system(), VM.replace('Anonymous pages: 1000.', 'Anonymous pages: 1.'), '1024', SWAP, '16384')
        self.assertEqual(result['comparisons']['used_bytes']['reference_bytes'], 500 * 16384)

    def test_swap_units_rounding_and_zero(self):
        for source, expected in [('0.0M', 0), ('1.5M', 1572864), ('2G', 2 * 1024**3), ('16K', 16384), ('42B', 42)]:
            result = ACCURACY.parse_swap('total = 4G used = ' + source + ' free = 2G')
            self.assertEqual(result['bytes'], expected)
        self.assertAlmostEqual(ACCURACY.parse_swap(SWAP)['rounding_half_step_bytes'], .05 * 1024**2)
        with self.assertRaises(ValueError):
            ACCURACY.parse_swap('total = 2G free = 2G')

    def test_ps_kib_conversion_zero_and_invalid_rows(self):
        self.assertEqual(ACCURACY.parse_ps(' 1 0\n42 16\n'), {1: 0, 42: 16384})
        for text in ['', '1 2\n1 2', 'PID RSS', '1 -1', '1 2 command']:
            with self.assertRaises(ValueError):
                ACCURACY.parse_ps(text)


class ComparisonTests(unittest.TestCase):
    def test_inclusive_budget_and_real_zero(self):
        for reference in [0, 100 * 1024**3]:
            tolerance = max(64 * 1024**2, reference / 100)
            self.assertTrue(ACCURACY.compare_metric(metric(reference + int(tolerance)), reference, tolerance)['within_tolerance'])
            self.assertFalse(ACCURACY.compare_metric(metric(reference + int(tolerance) + 1), reference, tolerance)['within_tolerance'])
        self.assertTrue(ACCURACY.compare_metric(metric(0), 0, 0)['within_tolerance'])
        for missing in [metric(None, 'denied', 'denied'), metric(0, 'stale'), metric(None)]:
            self.assertIsNone(ACCURACY.compare_metric(missing, 0, 1)['within_tolerance'])

    def test_bracket_accepts_values_between_reads_and_measures_distance_outside(self):
        tolerance = 64 * 1024**2
        low, high = 10 * 1024**3, 10 * 1024**3 + 400 * 1024**2
        inside = ACCURACY.compare_metric(metric(low + 200 * 1024**2), low, tolerance, bracket=(low, high))
        self.assertTrue(inside['within_tolerance'])
        self.assertEqual(inside['bracket_distance_bytes'], 0)
        self.assertEqual(inside['difference_bytes'], 200 * 1024**2)
        outside = ACCURACY.compare_metric(metric(high + tolerance + 1), low, tolerance, bracket=(low, high))
        self.assertFalse(outside['within_tolerance'])
        self.assertEqual(outside['bracket_distance_bytes'], tolerance + 1)

    def test_used_memory_change_between_reads_stays_within_the_bracket(self):
        # 400 MiB of anonymous pages appear between the concurrent and the after read.
        after = VM.replace('Anonymous pages: 1000.', 'Anonymous pages: 26600.')
        source = system()
        source['used_bytes'] = metric(27050 * 16384)
        alone = ACCURACY.system_comparison(source, VM, str(16 * 1024**3), SWAP, '16384')
        self.assertFalse(alone['comparisons']['used_bytes']['within_tolerance'])
        bracketed = ACCURACY.system_comparison(source, VM, str(16 * 1024**3), SWAP, '16384', [VM, after])
        used = bracketed['comparisons']['used_bytes']
        self.assertTrue(used['within_tolerance'])
        self.assertEqual(used['reference_bracket_bytes'], [1450 * 16384, 27050 * 16384])
        with self.assertRaises(ValueError):
            ACCURACY.system_comparison(source, VM, str(16 * 1024**3), SWAP, '16384',
                                       [after.replace('16384 bytes', '4096 bytes')])

    def test_rss_buckets_are_disjoint_exclude_non_ok_and_preserve_reasons(self):
        rows = [process(1, 1024), process(2, 1024+16384), process(3, 1024+16385),
                process(4, None, 'denied'), process(5, 0, 'stale'), process(6, None), process(7, 0), process(8, 0)]
        result = ACCURACY.rss_comparison(rows, {1: 1024, 2: 1024, 3: 1024, 4: 0, 5: 0, 6: 0, 8: 0, 9: 0}, 16384)
        self.assertEqual(result['counts'], dict(exact=2, within_one_page_nonzero=1, larger=1, non_ok=2, ok_null=1, absent_from_ps=1))
        self.assertEqual(result['compared'], 4)
        self.assertEqual(result['ratios'], dict(exact=.5, within_one_page_nonzero=.25, larger=.25))
        self.assertEqual(result['ps_only'], [9])
        self.assertEqual(result['non_ok_statuses'], {'denied': 1, 'stale': 1})
        self.assertEqual(result['details'][0]['pid'], 3)
        self.assertEqual(result['details'][1]['reason'], 'fixture denial')

    def test_no_comparable_processes_has_no_success_ratio(self):
        result = ACCURACY.rss_comparison([process(42, None, 'exited')], {1: 0}, 4096)
        self.assertEqual(result['compared'], 0)
        self.assertIsNone(result['ratios']['exact'])

    def test_duplicate_pid_or_non_rss_cannot_compare(self):
        row = process(42, 0)
        with self.assertRaises(ValueError):
            ACCURACY.rss_comparison([row, row], {42: 0}, 4096)
        row['metric_kind'] = 'footprint'
        with self.assertRaises(ValueError):
            ACCURACY.rss_comparison([row], {42: 0}, 4096)
        for value in [-1, True, 1.5]:
            with self.assertRaises(ValueError):
                ACCURACY.rss_comparison([process(42, value)], {42: 0}, 4096)

    def test_command_failure_bad_json_schema_and_parse_fail_closed(self):
        for mutate in [lambda r: r['status'].update(exit_code=1),
                       lambda r: r['status'].update(stdout='{}'),
                       lambda r: r['list'].update(stdout='{'),
                       lambda r: r['status'].update(stdout=json.dumps({'schema_version': 1})),
                       lambda r: r['ps'].update(stdout='bad')]:
            raw = commands()
            mutate(raw)
            result = ACCURACY.analyze(raw)
            self.assertEqual(result['status'], 'unavailable')
            self.assertFalse(ACCURACY.summarize([result])['used_gate_passed'])

    def test_same_second_checks_entire_read_window_and_monotonic_clock(self):
        raw = commands()
        self.assertTrue(ACCURACY.sample_timing(raw)['same_second'])
        raw['list']['finished_unix_ns'] = 2000000000
        self.assertFalse(ACCURACY.sample_timing(raw)['same_second'])
        raw = commands()
        raw['list']['finished_monotonic_ns'] += 10**9
        self.assertFalse(ACCURACY.sample_timing(raw)['same_second'])

    def test_summary_reports_max_median_and_timing_failure(self):
        first = ACCURACY.analyze(commands())
        second = copy.deepcopy(first)
        second['system']['comparisons']['used_bytes']['absolute_difference_bytes'] = 100
        summary = ACCURACY.summarize([first, second])
        self.assertTrue(summary['used_gate_passed'])
        self.assertEqual(summary['system']['used_bytes']['maximum_absolute_difference_bytes'], 100)
        self.assertEqual(summary['system']['used_bytes']['median_absolute_difference_bytes'], 50)
        self.assertEqual(summary['rss']['compared'], 2)
        second['timing']['same_second'] = False
        self.assertFalse(ACCURACY.summarize([first, second])['used_gate_passed'])
        self.assertFalse(ACCURACY.summarize([])['used_gate_passed'])

    def test_other_system_failures_and_no_rss_are_not_overall_success(self):
        for field in ['total_bytes', 'compressed_bytes', 'swap_used_bytes']:
            sample = ACCURACY.analyze(commands())
            sample['system']['comparisons'][field]['within_tolerance'] = False
            result = ACCURACY.summarize([sample])
            self.assertTrue(result['used_gate_passed'])
            self.assertFalse(result['comparison_checks_passed'])
        sample = ACCURACY.analyze(commands())
        sample['rss']['compared'] = 0
        self.assertFalse(ACCURACY.summarize([sample])['comparison_checks_passed'])

    def test_capture_forwards_private_environment_and_lock_without_signals(self):
        child = MagicMock(returncode=0)
        child.communicate.return_value = ('fixture output', '')
        with patch.object(ACCURACY.subprocess, 'Popen', return_value=child) as spawn:
            env = {'HOME': '/synthetic/private', 'TERM': 'xterm-256color', 'COLORTERM': 'truecolor'}
            row = ACCURACY.capture(['fixture'], env, 123)
        self.assertEqual(spawn.call_args.kwargs['env'], env)
        self.assertEqual(spawn.call_args.kwargs['pass_fds'], (123,))
        child.wait.assert_called_once()
        child.send_signal.assert_not_called()
        child.kill.assert_not_called()
        self.assertEqual(row['exit_code'], 0)

    def test_lock_rejects_relative_path(self):
        with self.assertRaises(ValueError):
            with ACCURACY.experiment_lock(Path('relative')):
                self.fail('relative lock accepted')

    def test_complete_runner_uses_private_home_and_preserves_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            output = root / '.artifacts/run'
            binary = root / 'bree'
            binary.write_text('synthetic binary')
            lock = root / 'shared.lock'
            argv = ['accuracy-check', '--binary', str(binary), '--lock', str(lock),
                    '--output', str(output), '--samples', '2']
            environments = []

            def fake_capture(command, env, lock_fd):
                environments.append(dict(env))
                self.assertGreaterEqual(lock_fd, 0)
                if command[-1] == '--version':
                    return dict(exit_code=0, stdout='bree fixture')
                key = {'vm_stat': 'vm_stat', 'ps': 'ps'}.get(Path(command[0]).name)
                if Path(command[0]).name == 'sysctl':
                    key = {'hw.memsize': 'total', 'hw.pagesize': 'pagesize', 'vm.swapusage': 'swap'}[command[-1]]
                return commands()[key or command[1]]

            with patch.object(ACCURACY, 'ROOT', root), \
                    patch.object(ACCURACY.platform, 'system', return_value='Darwin'), \
                    patch.object(ACCURACY.platform, 'mac_ver', return_value=('fixture', (), 'arm64')), \
                    patch.object(ACCURACY.platform, 'machine', return_value='arm64'), \
                    patch.object(ACCURACY, 'capture', side_effect=fake_capture), \
                    patch.object(ACCURACY.subprocess, 'check_output', return_value='fixture'), \
                    patch.object(ACCURACY.time, 'sleep'), \
                    patch.object(sys, 'argv', argv), \
                    patch.dict(os.environ, HOME='/caller/private', TERM='dumb', COLORTERM='inherited', NO_COLOR='1'), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(ACCURACY.main(), 0)
                original = (output / 'summary.json').read_bytes()
                with self.assertRaises(SystemExit) as error:
                    ACCURACY.main()
                self.assertEqual(error.exception.code, 2)
                self.assertEqual((output / 'summary.json').read_bytes(), original)
            self.assertEqual(len(list(output.glob('sample-??-raw.json'))), 2)
            self.assertTrue(json.loads(original)['comparison_checks_passed'])
            for env in environments:
                self.assertTrue(Path(env['HOME']).is_relative_to(output))
                self.assertFalse(Path(env['HOME']).parent.exists())
                self.assertEqual(env['TERM'], 'xterm-256color')
                self.assertEqual(env['COLORTERM'], 'truecolor')
                self.assertEqual(env['LC_ALL'], 'C')
                self.assertNotIn('NO_COLOR', env)


if __name__ == '__main__':
    unittest.main()
