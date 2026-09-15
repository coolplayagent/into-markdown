#!/usr/bin/env python3
"""Reconcile hash-bound runs against their full frozen source inventory."""
import argparse
import json
import pathlib
from quality_gate import digest


def reconcile(base_path, replay_paths, manifest_path=None):
    paths = [base_path, *replay_paths]
    reports = [json.loads(p.read_text()) for p in paths]
    if not all(r.get('binarySha256') for r in reports):
        raise ValueError('all runs must identify their executables')
    if manifest_path is None and not all(r.get('complete') for r in reports[1:]):
        raise ValueError('partial replays require the complete frozen source manifest')
    if not reports[0].get('complete') and manifest_path is None:
        raise ValueError('a partial base requires the complete frozen source manifest')
    inventory = {}
    if manifest_path is not None:
        manifest = json.loads(manifest_path.read_text())
        if digest(manifest_path) != reports[0]['manifestSha256']:
            raise ValueError('base manifest authority mismatch')
        inventory = {(p['id'], mode): p['sha256'] for p in manifest['papers'] for mode in reports[0]['modes']}
    base_seen = set()
    for case in reports[0]['cases']:
        key = (case['id'], case['ocr'])
        if key in base_seen or (manifest_path is not None and inventory.get(key) != case['sourceSha256']):
            raise ValueError('duplicate or changed base case')
        base_seen.add(key)
        inventory[key] = case['sourceSha256']
    cases, replacements = {}, []
    for index, (path, report) in enumerate(zip(paths, reports)):
        seen = set()
        for case in report['cases']:
            key = (case['id'], case['ocr'])
            if key in seen or inventory.get(key) != case['sourceSha256']:
                raise ValueError('duplicate, unknown, or changed replay source')
            seen.add(key)
            if key in cases:
                replacements.append(dict(id=key[0], ocr=key[1],
                    previousPassed=cases[key]['passed'], replayPassed=case['passed'],
                    previousBinarySha256=cases[key]['binarySha256'],
                    replayBinarySha256=report['binarySha256']))
            cases[key] = {**case, 'binarySha256': report['binarySha256'],
                          'evidenceReportSha256': digest(path), 'evidenceRunIndex': index}
    executables = {r['binarySha256'] for r in reports}
    result = {**reports[0], 'binarySha256': next(iter(executables)) if len(executables) == 1 else None,
              'evidenceKind': 'hash-bound corpus runs and targeted replays',
              'complete': len(cases) == len(inventory),
              'missingCases': [list(k) for k in inventory if k not in cases],
              'constituentRuns': [dict(reportSha256=digest(p), binarySha256=r['binarySha256'],
                   manifestSha256=r['manifestSha256'], complete=r.get('complete', False), cases=len(r['cases']),
                   modes=r.get('modes'), watchdogSeconds=r.get('watchdogSeconds'),
                   configurationSha256=[c['sha256'] for c in r.get('configurations', [])])
                   for p, r in zip(paths, reports)],
              'replacements': replacements, 'cases': list(cases.values())}
    result.update(passed=sum(c['passed'] for c in cases.values()),
                  failed=sum(not c['passed'] for c in cases.values()))
    return result


def combine_partitions(paths, manifest_path):
    """Combine disjoint runs, retaining each partition's original authority."""
    reports = [json.loads(path.read_text()) for path in paths]
    if not reports or not reports[0].get('binarySha256') or not reports[0].get('modes'):
        raise ValueError('partitions require executable and mode identities')
    reference = reports[0]
    for report in reports:
        for key in ('binarySha256', 'modes', 'watchdogSeconds', 'configurations'):
            if report.get(key) != reference.get(key):
                raise ValueError(f'partition execution options differ: {key}')
    manifest = json.loads(manifest_path.read_text())
    inventory = {(paper['id'], mode): paper for paper in manifest['papers']
                 for mode in reference['modes']}
    if len(inventory) != len(manifest['papers']) * len(reference['modes']):
        raise ValueError('duplicate source or mode in partition inventory')
    cases, runs = {}, []
    for index, (path, report) in enumerate(zip(paths, reports)):
        authority = digest(path)
        for case in report['cases']:
            key = (case['id'], case['ocr'])
            expected = inventory.get(key)
            if key in cases or expected is None or expected['sha256'] != case['sourceSha256']:
                raise ValueError('duplicate, unknown, or changed partition source')
            if 'pages' in expected and case.get('expectedPages') != expected['pages']:
                raise ValueError('partition page count differs from source inventory')
            cases[key] = {**case, 'binarySha256': report['binarySha256'],
                          'evidenceReportSha256': authority, 'evidenceRunIndex': index}
        runs.append(dict(reportSha256=authority, binarySha256=report['binarySha256'],
                         manifestSha256=report['manifestSha256'], complete=report.get('complete', False),
                         cases=len(report['cases']), modes=report['modes'],
                         watchdogSeconds=report.get('watchdogSeconds'),
                         configurationSha256=[c['sha256'] for c in report.get('configurations', [])]))
    missing = [list(key) for key in inventory if key not in cases]
    return {**reference, 'manifestSha256': digest(manifest_path),
            'evidenceKind': 'disjoint partitions of the frozen source inventory',
            'complete': not missing and all(r.get('complete', False) for r in reports),
            'missingCases': missing, 'cases': list(cases.values()), 'constituentRuns': runs,
            'passed': sum(c['passed'] for c in cases.values()),
            'failed': sum(not c['passed'] for c in cases.values())}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument('--base', type=pathlib.Path)
    source.add_argument('--partition', type=pathlib.Path, action='append')
    parser.add_argument('--replay', type=pathlib.Path, action='append')
    parser.add_argument('--manifest', type=pathlib.Path)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    args = parser.parse_args()
    if args.partition:
        if args.manifest is None or args.replay:
            parser.error('partitions require --manifest and use no --replay')
        result = combine_partitions(args.partition, args.manifest)
    else:
        if not args.replay:
            parser.error('--base requires --replay')
        result = reconcile(args.base, args.replay, args.manifest)
    args.output.write_text(json.dumps(result, indent=2)+'\n')
