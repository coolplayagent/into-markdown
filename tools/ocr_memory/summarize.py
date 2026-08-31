#!/usr/bin/env python3
"""Reanalyze immutable measurements with current DTO projections and compare runs."""

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path

from observations import content


def fingerprint(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(path):
    return json.loads(path.read_text())


def analyze(root, samples):
    records = []
    identities = {Path(item['path']).stem: item for item in samples}
    for receipt in sorted(root.rglob('measurement.json')):
        run = load(receipt)
        mode, grouping, _ = receipt.relative_to(root).parts[:3]
        report_path = receipt.parent / 'report.json'
        report = load(report_path) if report_path.exists() else {}
        items = {Path(item['input']).stem: item for item in report.get('items', [])}
        for identity in run['samples']:
            sample = identities[identity[:16]]
            item = items.get(identity[:16], {})
            artifact = receipt.parent / 'stdout.json' if grouping == 'single' else (
                receipt.parent / 'outputs' / (identity[:16] + '.json'))
            projection = None
            if artifact.exists() and artifact.stat().st_size:
                document = load(artifact)
                if 'document' in document:
                    projection = content(document)
            records.append({
                'mode': mode, 'grouping': grouping, 'sha256': identity,
                'kind': sample['kind'], 'binarySha256': run['binarySha256'],
                'receipt': str(receipt.relative_to(root)), 'receiptSha256': fingerprint(receipt),
                'artifactSha256': fingerprint(artifact) if artifact.exists() else None,
                'exitCode': run['exitCode'], 'status': item.get('status', 'no-report'),
                'reasonCode': item.get('reasonCode'), 'errorCode': item.get('errorCode'),
                'limit': item.get('limit'), 'harnessTimeout': run['harnessTimeout'],
                'content': projection,
                'resourceUsage': report.get('resourceUsage'),
                'processTreeRssSamplePeakBytes': run['processTreeRssSamplePeakBytes'],
                'operatingSystemPeakBytes': run.get('operatingSystemPeakBytes'),
                'operatingSystemPeakSource': run.get('operatingSystemPeakSource'),
                'peakConcurrentOcrProviders': run.get('peakConcurrentOcrProviders'),
                'peakConcurrentModelWorkers': run.get('peakConcurrentModelWorkers'),
                'observedModelWorkerLimits': run.get('observedModelWorkerLimits'),
            })
    return records


def compare(records, baseline):
    key = lambda row: (row['mode'], row['grouping'], row['sha256'])
    previous = {key(row): row for row in baseline}
    result = []
    for row in records:
        old = previous.get(key(row))
        current = row['content']
        before = old.get('content') if old else None
        result.append({
            'mode': row['mode'], 'grouping': row['grouping'], 'sha256': row['sha256'],
            'hasBaseline': old is not None,
            'previousStatus': old['status'] if old else None, 'status': row['status'],
            'previousReason': old['reasonCode'] if old else None, 'reason': row['reasonCode'],
            'nativeBodyEqual': current['nativeUnits'] == before['nativeUnits']
                if current and before else None,
            'assetsEqual': current['assetInventory'] == before['assetInventory']
                if current and before else None,
            'previousOcrBlocks': len(before['ocrBlocks']) if before else None,
            'ocrBlocks': len(current['ocrBlocks']) if current else None,
        })
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--baseline', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    records = analyze(args.root, load(args.manifest)['samples'])
    counts = Counter((row['mode'], row['grouping'], row['status']) for row in records)
    result = {'schemaVersion': 1, 'projectionSha256': fingerprint(Path(__file__).with_name('observations.py')),
              'manifestSha256': fingerprint(args.manifest), 'records': records,
              'counts': [{'mode': m, 'grouping': g, 'status': s, 'count': n}
                         for (m, g, s), n in sorted(counts.items())]}
    if args.baseline:
        result['comparisons'] = compare(records, load(args.baseline)['records'])
    args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
