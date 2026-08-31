#!/usr/bin/env python3
"""Run frozen OCR samples, recording outcomes and sampled process-tree RSS separately."""

import argparse
import hashlib
import json
import os
import sys
from pathlib import Path
import subprocess
import time

import psutil
from observations import content

MODES = {
    'off': ('off', 'auto', 'best-effort'),
    'auto': ('auto', 'auto', 'best-effort'),
    '16gib': ('auto', '16GiB', 'best-effort'),
    'off-16gib': ('off', '16GiB', 'best-effort'),
    'always': ('always', '16GiB', 'best-effort'),
    'strict': ('auto', '16GiB', 'strict'),
    'always-strict': ('always', '16GiB', 'strict'),
}


def sample_processes(process):
    rows = []
    try:
        processes = [process, *process.children(recursive=True)]
    except psutil.Error:
        processes = [process]
    for child in processes:
        try:
            row = {'pid': child.pid, 'name': child.name(), 'rssBytes': child.memory_info().rss}
            if 'onnxruntime-worker' in row['name']:
                arguments = child.cmdline()
                for flag, field in [('--physical-limit', 'physicalLimitBytes'),
                                    ('--address-limit', 'addressLimitBytes')]:
                    if flag in arguments:
                        index = arguments.index(flag) + 1
                        if index < len(arguments) and arguments[index].isdigit():
                            row[field] = int(arguments[index])
            rows.append(row)
        except psutil.Error:
            pass
    return rows


def measure(command, directory, timeout):
    snapshot = psutil.virtual_memory()
    start = time.monotonic()
    peak, peak_rows, os_peak = 0, [], None
    provider_peak, model_peak = 0, 0
    model_limits = set()
    with (directory / 'stdout.json').open('wb') as stdout, (directory / 'stderr.jsonl').open('wb') as stderr:
        child = subprocess.Popen(command, stdout=stdout, stderr=stderr)
        process = psutil.Process(child.pid)
        timed_out = False
        while True:
            rows = sample_processes(process)
            rss = sum(row['rssBytes'] for row in rows)
            provider_peak = max(provider_peak, sum('into-md-ocr-provider' in row['name'] for row in rows))
            model_peak = max(model_peak, sum('onnxruntime-worker' in row['name'] for row in rows))
            model_limits.update((row.get('physicalLimitBytes'), row.get('addressLimitBytes'))
                                for row in rows if 'physicalLimitBytes' in row)
            if rss > peak:
                peak, peak_rows = rss, rows
            if os.name == 'posix':
                pid, status, usage = os.wait4(child.pid, os.WNOHANG)
                if pid:
                    child.returncode = os.waitstatus_to_exitcode(status)
                    os_peak = usage.ru_maxrss * (1 if sys.platform == 'darwin' else 1024)
                    break
            else:
                try:
                    info = process.memory_info()
                    os_peak = max(os_peak or 0, getattr(info, 'peak_wset', 0)) or None
                except psutil.Error:
                    pass
                if child.poll() is not None:
                    break
            if time.monotonic() - start > timeout and not timed_out:
                timed_out = True
                try:
                    descendants = process.children(recursive=True)
                except psutil.Error:
                    descendants = []
                for descendant in reversed(descendants):
                    try:
                        descendant.kill()
                    except psutil.Error:
                        pass
                child.kill()
            time.sleep(0.05)
        code = child.wait()
    return {'command': command, 'exitCode': code, 'harnessTimeout': timed_out,
            'durationSeconds': time.monotonic() - start,
            'hostTotalBytes': snapshot.total, 'hostAvailableBytes': snapshot.available,
            'hostProbe': 'psutil.virtual_memory (CLI selection separately records sysinfo)',
            'sampleIntervalMs': 50, 'processTreeRssSamplePeakBytes': peak,
            'processesAtSamplePeak': peak_rows, 'operatingSystemPeakBytes': os_peak,
            'operatingSystemPeakSource': 'wait4.ru_maxrss; kernel child accounting, not concurrent tree sum'
                if os.name == 'posix' else 'root process peak_wset',
            'peakConcurrentOcrProviders': provider_peak, 'peakConcurrentModelWorkers': model_peak,
            'observedModelWorkerLimits': [{'physicalLimitBytes': physical, 'addressLimitBytes': address}
                                         for physical, address in sorted(model_limits)]}


def artifacts(directory):
    records = []
    for path in sorted(directory.rglob('*.json')):
        if path.name in {'measurement.json', 'report.json'}:
            continue
        try:
            result = json.loads(path.read_text())
        except (ValueError, UnicodeError):
            continue
        if not isinstance(result, dict) or 'markdown' not in result:
            continue
        markdown = result['markdown']
        records.append({'path': str(path.relative_to(directory)),
            'markdownSha256': hashlib.sha256(markdown.encode()).hexdigest(),
            'markdownCharacters': len(markdown), 'assets': len(result.get('assets', [])),
            'diagnostics': result.get('diagnostics', []),
            'outcome': result.get('outcome'), 'content': content(result)})
    return records


def run(args):
    samples = json.loads(args.manifest.read_text())['samples']
    if args.kind:
        samples = [item for item in samples if item['kind'] == args.kind]
    binary = str(args.binary.resolve())
    binary_hash = hashlib.sha256(args.binary.read_bytes()).hexdigest()
    args.output.mkdir(parents=True, exist_ok=True)
    for mode in args.modes.split(','):
        ocr, memory, policy = MODES[mode]
        for grouping in args.groupings.split(','):
            groups = [[sample] for sample in samples] if grouping == 'single' else [samples]
            for group in groups:
                label = group[0]['sha256'][:16] if grouping == 'single' else 'all'
                directory = args.output / mode / grouping / label
                receipt = directory / 'measurement.json'
                directory.mkdir(parents=True, exist_ok=True)
                paths = [str((args.root / item['path']).resolve()) for item in group]
                # The CLI deadline covers the entire invocation. Give a batch
                # the same total allowance as its constituent single-file runs.
                invocation_timeout_ms = 120_000 * len(group)
                command = [binary, '--no-config', '--log-format', 'json', '--ocr', ocr,
                    '--max-memory-size', memory, '--error-policy', policy, '--emit', 'result-json',
                    '--asset-mode', 'embed', '--conflict', 'error',
                    '--timeout-ms', str(invocation_timeout_ms),
                    '--report', str((directory / 'report.json').resolve())]
                if grouping != 'single':
                    command += ['--jobs', grouping.removeprefix('jobs'),
                                '--output-dir', str((directory / 'outputs').resolve())]
                command += paths
                if receipt.exists():
                    previous = json.loads(receipt.read_text())
                    if previous['binarySha256'] != binary_hash:
                        raise SystemExit('output contains measurements from another binary')
                    if previous['samples'] != [s['sha256'] for s in group]:
                        raise SystemExit('output contains a different sample selection')
                    if previous['command'] != command:
                        raise SystemExit('output contains measurements with different invocation settings')
                    continue
                dry = subprocess.run(command + ['--dry-run'], capture_output=True, timeout=60)
                (directory / 'dry-run.log').write_bytes(dry.stdout + dry.stderr)
                result = measure(command, directory, max(150, len(group) * 125))
                result.update(binarySha256=binary_hash, samples=[s['sha256'] for s in group],
                              artifacts=artifacts(directory), dryRunExitCode=dry.returncode)
                receipt.write_text(json.dumps(result, indent=2) + '\n')
                print(f'{mode}/{grouping}/{label}: exit={result["exitCode"]} '
                      f'rss={result["processTreeRssSamplePeakBytes"]}', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ['binary', 'manifest', 'root', 'output']:
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--kind')
    parser.add_argument('--modes', default='off,auto,16gib')
    parser.add_argument('--groupings', default='single,jobs1,jobs4')
    run(parser.parse_args())


if __name__ == '__main__':
    main()
