#!/usr/bin/env python3
"""Compare a freshly built Bree with macOS counters and ps, without process actions.

Example (build first, under the same shared lock):
  python3 scripts/accuracy-check.py --lock /absolute/shared/native-experiment.lock \
      --output .artifacts/m2/run-001

Each round starts near a wall-clock second boundary and launches all readers
concurrently. A round crossing that second is retained but cannot pass timing.
RSS ratios use only matched PIDs with ok, non-null RSS. PID reuse between tools
cannot be excluded by ps's PID-only output. Raw evidence is private and local.
"""
import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import contextlib
from decimal import Decimal
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import time

from check_env import empty_bree_environment

ROOT = Path(__file__).resolve().parents[1]
MIB = 1024 ** 2
METRICS = ('used_bytes', 'total_bytes', 'compressed_bytes', 'swap_used_bytes')
# Apple system_cmds vm_stat: display name -> vm_statistics64 field.
# https://github.com/apple-oss-distributions/system_cmds/blob/main/vm_stat/vm_stat.c
VM_FIELDS = {
    'Anonymous pages': 'internal_page_count',
    'Pages purgeable': 'purgeable_count',
    'Pages wired down': 'wire_count',
    'Pages occupied by compressor': 'compressor_page_count',
}


def parse_vm_stat(text):
    header = re.search(r'page size of (\d+) bytes', text)
    if not header or int(header[1]) <= 0:
        raise ValueError('vm_stat page size is missing or invalid')
    counters = {}
    for line in text.splitlines():
        match = re.fullmatch(r'\s*([^:]+):\s*(\d+)\.\s*', line)
        if match and match[1].strip() in VM_FIELDS:
            key = VM_FIELDS[match[1].strip()]
            if key in counters:
                raise ValueError(f'Duplicate vm_stat counter: {key}')
            counters[key] = int(match[2])
    return int(header[1]), counters


def parse_swap(text):
    match = re.search(r'\bused\s*=\s*(\d+(?:\.\d+)?)\s*([BKMGTP])\b', text)
    if not match:
        raise ValueError('sysctl vm.swapusage used value is missing')
    scale = 1024 ** 'BKMGTP'.index(match[2])
    value = Decimal(match[1])
    return {'bytes': int(value * scale),
            'rounding_half_step_bytes': float(Decimal(10) ** value.as_tuple().exponent * scale / 2)}


def parse_ps(text):
    values = {}
    for line in text.splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r'\s*(\d+)\s+(\d+)\s*', line)
        if not match:
            raise ValueError('Malformed ps PID/RSS row')
        pid = int(match[1])
        if pid in values:
            raise ValueError(f'Duplicate ps PID: {pid}')
        values[pid] = int(match[2]) * 1024
    if not values:
        raise ValueError('ps returned no processes')
    return values


def compare_metric(metric, reference, tolerance, reason=None):
    result = {'bree_bytes': metric.get('value'), 'bree_status': metric.get('status'),
              'bree_reason': metric.get('reason'), 'reference_bytes': reference,
              'tolerance_bytes': tolerance, 'difference_bytes': None,
              'absolute_difference_bytes': None, 'within_tolerance': None}
    if reference is None or metric.get('status') != 'ok' or metric.get('value') is None:
        result.update(status='unavailable', reason=reason or metric.get('reason') or 'Missing metric')
        return result
    value = metric['value']
    if type(value) is not int or value < 0:
        raise ValueError('Invalid Bree memory value')
    delta = value - reference
    result.update(status='compared', difference_bytes=delta,
                  absolute_difference_bytes=abs(delta), within_tolerance=abs(delta) <= tolerance)
    return result


def system_comparison(system, vm_text, total_text, swap_text, pagesize_text):
    page_size, counters = parse_vm_stat(vm_text)
    if page_size != int(pagesize_text.strip()):
        raise ValueError('vm_stat and sysctl page sizes disagree')
    total = int(total_text.strip())
    if total <= 0:
        raise ValueError('Physical memory is invalid')
    missing = sorted(set(VM_FIELDS.values()) - counters.keys())
    used = None if missing else (
        max(0, counters['internal_page_count'] - counters['purgeable_count'])
        + counters['wire_count'] + counters['compressor_page_count']) * page_size
    compressed = counters.get('compressor_page_count')
    compressed = None if compressed is None else compressed * page_size
    swap = parse_swap(swap_text)
    references = dict(used_bytes=used, total_bytes=total,
                      compressed_bytes=compressed, swap_used_bytes=swap['bytes'])
    comparisons = {}
    for key, reference in references.items():
        # Used is the M2 gate. Compressed/swap use the same exploratory budget;
        # physical memory must match exactly. Swap's printed rounding is retained.
        tolerance = 0 if key == 'total_bytes' else max(64 * MIB, (reference or 0) / 100)
        reason = 'Missing vm_stat counters: ' + ', '.join(missing) if reference is None else None
        comparisons[key] = compare_metric(system.get(key, {}), reference, tolerance, reason)
    return {'page_size': page_size, 'vm_counters': counters, 'missing_counters': missing,
            'swap_rounding_half_step_bytes': swap['rounding_half_step_bytes'],
            'comparisons': comparisons}


def rss_comparison(processes, ps_values, page_size):
    if page_size <= 0:
        raise ValueError('RSS comparison needs a positive page size')
    counts = Counter(exact=0, within_one_page_nonzero=0, larger=0,
                     non_ok=0, ok_null=0, absent_from_ps=0)
    non_ok = Counter()
    details = []
    seen = set()
    for process in processes:
        pid = process['identity']['pid']
        if pid in seen:
            raise ValueError(f'Duplicate Bree PID: {pid}')
        seen.add(pid)
        metric = process['memory_bytes']
        row = {'pid': pid, 'name': process['name'], 'category': process['category'],
               'bree_status': metric['status'], 'bree_bytes': metric['value'],
               'ps_bytes': ps_values.get(pid), 'reason': metric.get('reason')}
        if metric['status'] != 'ok':
            bucket = 'non_ok'
            non_ok[metric['status']] += 1
        elif metric['value'] is None:
            bucket = 'ok_null'
            row['possible_cause'] = 'Inconsistent metric: ok with null'
        elif pid not in ps_values:
            bucket = 'absent_from_ps'
            row['possible_cause'] = 'Process started/exited between enumerations; not a numeric mismatch'
        else:
            if process.get('metric_kind') != 'rss':
                raise ValueError('Process memory is not RSS')
            if type(metric['value']) is not int or metric['value'] < 0:
                raise ValueError('Invalid Bree RSS value')
            delta = metric['value'] - ps_values[pid]
            row['difference_bytes'] = delta
            bucket = 'exact' if delta == 0 else ('within_one_page_nonzero' if abs(delta) <= page_size else 'larger')
            if bucket == 'larger':
                row['possible_cause'] = 'RSS changed between reads, or PID reuse; ps does not expose instance identity here. Cause unconfirmed.'
        counts[bucket] += 1
        row['bucket'] = bucket
        if bucket not in ('exact', 'within_one_page_nonzero'):
            details.append(row)
    compared = counts['exact'] + counts['within_one_page_nonzero'] + counts['larger']
    return {'counts': dict(counts), 'compared': compared, 'bree_total': len(processes),
            'ps_total': len(ps_values), 'ps_only': sorted(set(ps_values) - seen),
            'non_ok_statuses': dict(non_ok), 'details': details,
            'ratios': {key: counts[key] / compared if compared else None
                       for key in ('exact', 'within_one_page_nonzero', 'larger')}}


def capture(command, env, lock_fd):
    started_wall = time.time_ns()
    started = time.monotonic_ns()
    child = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True, start_new_session=True,
                             pass_fds=(lock_fd,))
    # No signals or timeouts: all readers are finite commands; reap before unlock.
    try:
        stdout, stderr = child.communicate()
    finally:
        child.wait()
    return {'command': command, 'started_unix_ns': started_wall,
            'finished_unix_ns': time.time_ns(), 'started_monotonic_ns': started,
            'finished_monotonic_ns': time.monotonic_ns(), 'exit_code': child.returncode,
            'stdout': stdout, 'stderr': stderr}


def sample_timing(commands):
    start = min(row['started_unix_ns'] for row in commands.values())
    end = max(row['finished_unix_ns'] for row in commands.values())
    span = (max(row['finished_monotonic_ns'] for row in commands.values())
            - min(row['started_monotonic_ns'] for row in commands.values())) / 1e6
    return {'started_unix_ns': start, 'finished_unix_ns': end, 'window_ms': span,
            'same_second': start // 10**9 == end // 10**9 and 0 <= span < 1000 and end >= start}


def analyze(commands):
    timing = sample_timing(commands)
    result = {'timing': timing}
    failures = [name for name, row in commands.items() if row['exit_code'] != 0]
    if failures:
        return dict(result, status='unavailable', error='Failed commands: ' + ', '.join(failures))
    try:
        status = json.loads(commands['status']['stdout'])
        listing = json.loads(commands['list']['stdout'])
        if status['schema_version'] != 2 or listing['schema_version'] != 2:
            raise ValueError('Expected Bree schema 2')
        system = system_comparison(status['system'], commands['vm_stat']['stdout'],
                                   commands['total']['stdout'], commands['swap']['stdout'],
                                   commands['pagesize']['stdout'])
        rss = rss_comparison(listing['processes'], parse_ps(commands['ps']['stdout']), system['page_size'])
        result.update(status='compared', system=system, rss=rss)
    except (ValueError, KeyError, TypeError) as error:
        result.update(status='unavailable', error=str(error))
    return result


def summarize(samples):
    system = {}
    for metric in METRICS:
        rows = [s['system']['comparisons'][metric] for s in samples if 'system' in s]
        differences = [r['absolute_difference_bytes'] for r in rows if r['status'] == 'compared']
        system[metric] = {'compared': len(differences),
                          'maximum_absolute_difference_bytes': max(differences) if differences else None,
                          'median_absolute_difference_bytes': statistics.median(differences) if differences else None,
                          'within_tolerance': sum(r['within_tolerance'] is True for r in rows),
                          'all_within_tolerance': bool(samples) and len(differences) == len(samples)
                              and all(r['within_tolerance'] is True for r in rows)}
    counts = Counter()
    non_ok = Counter()
    for sample in samples:
        counts.update(sample.get('rss', {}).get('counts', {}))
        non_ok.update(sample.get('rss', {}).get('non_ok_statuses', {}))
    compared = sum(counts[key] for key in ('exact', 'within_one_page_nonzero', 'larger'))
    valid_timing = sum(s['timing']['same_second'] for s in samples)
    return {'samples': len(samples), 'same_second_samples': valid_timing, 'system': system,
            'comparison_checks_passed': bool(samples) and valid_timing == len(samples)
                and all(row['all_within_tolerance'] for row in system.values())
                and all(s.get('rss', {}).get('compared', 0) > 0 for s in samples)
                and counts['ok_null'] == 0,
            'used_gate_passed': bool(samples) and valid_timing == len(samples)
                and system['used_bytes']['all_within_tolerance'],
            'rss': {'counts': dict(counts), 'compared': compared, 'non_ok_statuses': dict(non_ok),
                    'ratios': {key: counts[key] / compared if compared else None
                               for key in ('exact', 'within_one_page_nonzero', 'larger')}}}


@contextlib.contextmanager
def experiment_lock(path):
    if not path.is_absolute():
        raise ValueError('Shared lock must be absolute')
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        print('Waiting for shared experiment lock...', flush=True)
        fcntl.flock(fd, fcntl.LOCK_EX)
        print('Shared experiment lock acquired', flush=True)
        yield fd
    finally:
        os.close(fd)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/release/bree')
    parser.add_argument('--lock', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=30)
    parser.add_argument('--interval', type=float, default=2)
    args = parser.parse_args()
    output = args.output.resolve()
    binary = args.binary.resolve()
    if not args.lock.is_absolute() or not args.lock.parent.is_dir():
        parser.error('--lock must be absolute with an existing parent')
    if output.exists() or not any(output.is_relative_to(ROOT / p) for p in ('.artifacts', 'target')):
        parser.error('--output must be a new directory below this checkout\'s .artifacts or target')
    if args.samples < 1 or not math.isfinite(args.interval) or args.interval < 1:
        parser.error('samples must be positive and interval must be finite and >= 1 second')
    if platform.system() != 'Darwin':
        parser.error('Live comparison requires macOS')
    if not binary.is_file():
        parser.error('Build the release binary first under the same shared lock')
    output.mkdir(parents=True, mode=0o700)
    output.chmod(0o700)
    with experiment_lock(args.lock) as lock_fd, empty_bree_environment(output / 'results.json') as env:
        env['LC_ALL'] = 'C'
        identity = {'commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, env=env, text=True).strip(),
                    'git_status': subprocess.check_output(['git', 'status', '--short'], cwd=ROOT, env=env, text=True),
                    'binary': str(binary), 'binary_sha256': digest(binary),
                    'script_sha256': digest(Path(__file__)), 'platform': platform.system(),
                    'macos': platform.mac_ver()[0], 'architecture': platform.machine(),
                    'version': capture([str(binary), '--version'], env, lock_fd),
                    'lock': str(args.lock), 'vm_field_mapping': VM_FIELDS,
                    'tolerance_policy': 'Used: max(1% of vm_stat reference, 64 MiB); total: exact; compressed/swap: same exploratory budget as used; RSS: one page',
                    'samples_requested': args.samples, 'interval_seconds': args.interval}
        (output / 'identity.json').write_text(json.dumps(identity, indent=2) + '\n')
        commands = {'status': [str(binary), 'status', '--json'], 'list': [str(binary), 'list', '--json'],
                    'vm_stat': ['/usr/bin/vm_stat'], 'total': ['/usr/sbin/sysctl', '-n', 'hw.memsize'],
                    'swap': ['/usr/sbin/sysctl', '-n', 'vm.swapusage'],
                    'pagesize': ['/usr/sbin/sysctl', '-n', 'hw.pagesize'],
                    'ps': ['/bin/ps', '-axo', 'pid=,rss=']}
        samples = []
        with ThreadPoolExecutor(max_workers=len(commands)) as pool:
            next_round = time.monotonic()
            for index in range(args.samples):
                time.sleep(max(0, next_round - time.monotonic()))
                # Start at xx:xx:xx.05, leaving room for initialization + reads.
                time.sleep((1.05 - time.time() % 1) % 1)
                round_start = time.monotonic()
                futures = {name: pool.submit(capture, command, env, lock_fd) for name, command in commands.items()}
                raw = {name: future.result() for name, future in futures.items()}
                (output / f'sample-{index + 1:02}-raw.json').write_text(json.dumps(raw, indent=2) + '\n')
                sample = analyze(raw)
                samples.append(sample)
                (output / f'sample-{index + 1:02}.json').write_text(json.dumps(sample, indent=2) + '\n')
                brief = {key: {'delta': row['difference_bytes'], 'within_tolerance': row['within_tolerance']}
                         for key, row in sample.get('system', {}).get('comparisons', {}).items()}
                print(json.dumps({'sample': index + 1, 'timing': sample['timing'], 'metrics': brief,
                                  'rss': sample.get('rss', {}).get('counts'), 'error': sample.get('error')}), flush=True)
                next_round = round_start + args.interval - .02
        report = summarize(samples)
        report['binary_unchanged'] = digest(binary) == identity['binary_sha256']
        report['version_readable'] = identity['version']['exit_code'] == 0
        report['completed_requested_samples'] = len(samples) == args.samples
        (output / 'summary.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2), flush=True)
        return 0 if (report['comparison_checks_passed'] and report['binary_unchanged']
                     and report['version_readable']) else 1


if __name__ == '__main__':
    raise SystemExit(main())
