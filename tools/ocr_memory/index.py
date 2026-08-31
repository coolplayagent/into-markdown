#!/usr/bin/env python3
"""Index immutable experiment receipts, keeping source text and artifacts local."""

import argparse
from collections import Counter, defaultdict
import hashlib
import json
from pathlib import Path


def read(path):
    return json.loads(path.read_text())


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def bounds(values):
    values = [value for value in values if value is not None]
    return {'min': min(values), 'max': max(values)} if values else None


def experiment(root):
    groups = defaultdict(list)
    binaries = set()
    for path in sorted(root.glob('*/*/*/measurement.json')):
        mode, entry, label, _ = path.relative_to(root).parts
        if entry != 'single' and label != 'all':
            raise ValueError(f'keep interrupted experiments outside the measured root: {path}')
        receipt = read(path)
        binaries.add(receipt['binarySha256'])
        report_path = path.parent / 'report.json'
        report = read(report_path) if report_path.exists() else {}
        groups[(mode, entry)].append((path, receipt, report))
    rows = []
    for (mode, entry), runs in sorted(groups.items()):
        states, reasons = Counter(), Counter()
        samples, failures = set(), []
        for path, receipt, report in runs:
            if samples.intersection(receipt['samples']):
                raise ValueError(f'duplicate file outcome: {path}')
            samples.update(receipt['samples'])
            if report:
                for item in report['items']:
                    states[item['status']] += 1
                    if item['status'] != 'success':
                        reason = item.get('reasonCode') or item.get('errorCode') or 'unspecified'
                        reasons[reason] += 1
                        failures.append({'sample': Path(item['input']).stem, 'reason': reason})
            else:
                states['no-report'] += len(receipt['samples'])
                reasons[f"invocation-exit-{receipt['exitCode']}"] += len(receipt['samples'])
        usages = [report.get('resourceUsage', {}) for _, _, report in runs]
        rows.append({'mode': mode, 'entry': entry, 'files': len(samples),
            'states': dict(states), 'failureReasons': dict(reasons), 'failedFiles': failures,
            'sharedLeaseBudgetBytes': bounds([u.get('sharedLeaseBudgetBytes') for u in usages]),
            'sharedLeasePeakBytes': bounds([u.get('sharedLeasePeakBytes') for u in usages]),
            'memorySnapshots': {key: bounds([u.get('memory', {}).get(key) for u in usages])
                for key in ('totalBytes', 'availableBytes', 'systemReserveBytes',
                            'autoBudgetBytes', 'effectiveBudgetBytes')},
            'hostTotalBytes': bounds([r.get('hostTotalBytes') for _, r, _ in runs]),
            'hostAvailableBytes': bounds([r.get('hostAvailableBytes') for _, r, _ in runs]),
            'workerBudgetBytes': bounds([u.get('ocrRuntime', {}).get(k) for u in usages
                if u.get('ocrRuntime', {}).get('requests', 0) > 0
                for k in ('workerBudgetMinBytes', 'workerBudgetMaxBytes')]),
            'processTreeRssSamplePeakBytes': bounds([r['processTreeRssSamplePeakBytes'] for _, r, _ in runs]),
            'operatingSystemPeakBytes': bounds([r.get('operatingSystemPeakBytes') for _, r, _ in runs]),
            'operatingSystemPeakSources': sorted({r['operatingSystemPeakSource'] for _, r, _ in runs
                if r.get('operatingSystemPeakSource')}),
            'peakConcurrentOcrProviders': bounds([r.get('peakConcurrentOcrProviders') for _, r, _ in runs]),
            'peakConcurrentModelWorkers': bounds([r.get('peakConcurrentModelWorkers') for _, r, _ in runs]),
            'modelWorkerLimits': [json.loads(value) for value in sorted({json.dumps(limit, sort_keys=True)
                for _, r, _ in runs for limit in r.get('observedModelWorkerLimits', [])})],
            'receipts': [{'path': str(p.relative_to(root)), 'sha256': sha(p),
                'reportSha256': sha(p.parent / 'report.json') if (p.parent / 'report.json').exists() else None}
                for p, _, _ in runs]})
    return {'root': root.name, 'binarySha256': sorted(binaries), 'groups': rows}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--experiments', nargs='+', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    result = {'schemaVersion': 1, 'indexScriptSha256': sha(Path(__file__)),
        'scope': 'observed outcomes; failures retained; success alone does not establish OCR contribution',
        'experiments': [experiment(args.root / name) for name in args.experiments]}
    args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
