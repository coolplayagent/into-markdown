#!/usr/bin/env python3
"""Export source hashes and conversion metrics without source names or contents."""
import argparse
import collections
import hashlib
import json
import pathlib

from quality_gate import COUNTERS, check_ocr, digest


def compact(path):
    report = json.loads(path.read_text())
    records = []
    for case in report['cases']:
        items = case.get('items', [])
        usage = case.get('resourceUsage') or {}
        records.append(dict(
            id=case['id'], ocr=case['ocr'], sourceSha256=case['sourceSha256'],
            binarySha256=case.get('binarySha256', report['binarySha256']),
            passed=case['passed'], exitCode=case['exitCode'], seconds=case['seconds'],
            peakRssBytes=case.get('peakRssBytes'), markdownSha256=case.get('markdownSha256'),
            assetBundleSha256=hashlib.sha256(json.dumps(sorted(
                (a['path'], a['sha256']) for a in case.get('assets', [])),
                separators=(',', ':')).encode()).hexdigest(),
            outcomes=dict(collections.Counter(i.get('outcome') for i in items)),
            errorCodes=dict(collections.Counter(i['errorCode'] for i in items if i.get('errorCode'))),
            diagnosticCounts=dict(collections.Counter(d['code'] for i in items for d in i.get('diagnostics', []))),
            expectedPages=case.get('expectedPages'),
            uncoveredPages=case.get('uncoveredPages'), brokenAssetCount=len(case.get('brokenAssets', [])),
            assetCount=len(case.get('assets', [])), originalFileFallback=case.get('originalFileFallback', False),
            nestedOriginalFallbackCount=len(case.get('nestedOriginalChecks', [])),
            ocrRuntime=usage.get('ocrRuntime'),
            effectiveBudgetBytes=usage.get('memory', {}).get('effectiveBudgetBytes')))
    counts = {field: sum((r['ocrRuntime'] or {}).get(field, 0) for r in records) for field in COUNTERS}
    return dict(schemaVersion=1, reportSha256=digest(path),
                manifestSha256=report['manifestSha256'], binarySha256=report['binarySha256'],
                modes=report['modes'], complete=report.get('complete', False),
                constituentRuns=report.get('constituentRuns', []),
                replacements=report.get('replacements', []),
                passed=sum(r['passed'] for r in records), failed=sum(not r['passed'] for r in records),
                seconds=round(sum(r['seconds'] for r in records), 3),
                ocrTotals=counts,
                ocrTotalsByMode={mode: {field: sum((r['ocrRuntime'] or {}).get(field, 0)
                    for r in records if r['ocr'] == mode) for field in COUNTERS}
                    for mode in report['modes']},
                ocrUnaccountedCases=[r['id'] for r in records if check_ocr(r['ocrRuntime'])],
                cases=records)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    args = parser.parse_args()
    args.output.write_text(json.dumps(compact(args.report), ensure_ascii=False, indent=2)+'\n')
