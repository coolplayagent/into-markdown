#!/usr/bin/env python3
"""Audit per-document OCR processing separately from text recognition quality."""
import argparse
import json
import pathlib

from quality_gate import check_ocr, digest


def audit(report, proof=None, minimum=0.95):
    if not 0 < minimum <= 1:
        raise ValueError('completion threshold must be in (0, 1]')
    records, absent = [], []
    for case in report['cases']:
        if case['ocr'] != 'auto':
            continue
        usage = (case.get('resourceUsage') or {}).get('ocrRuntime')
        if usage is None and not case.get('passed'):
            absent.append(case['id'])
            continue  # Input/command failures are checked by conversion acceptance.
        errors = check_ocr(usage)
        excluded = 0
        if not errors:
            seen = set()
            transparent_refusal = any(
                d.get('code') == 'embeddedVisualOcr.unsupportedVisual'
                and 'fully transparent embedded raster' in d.get('message', '')
                for item in case.get('items', []) for d in item.get('diagnostics', []))
            asset_hashes = {a['sha256'] for a in case.get('assets', [])}
            for item in (proof or {}).get('records', []):
                key = item.get('sha256')
                if (item.get('id') == case['id']
                        and item.get('sourceSha256') == case['sourceSha256']
                        and key in asset_hashes and key not in seen
                        and item.get('alphaExtrema') == [0, 0] and transparent_refusal):
                    seen.add(key)
                    excluded += 1
            excluded = min(excluded, usage['imagesFailed'])
            attempted = usage['imagesAttempted'] - excluded
            completed = usage['imagesCompleted']
            rate = completed / attempted if attempted else None
            if rate is not None and rate < minimum:
                errors.append('OCR processing completion below threshold')
            unprocessed_skip = any(
                d.get('code') in ('embeddedVisualOcr.optionalRecognitionMemorySkipped',
                                  'resource.max_memory.unitOmitted', 'pdf.optionalOcrSkipped')
                for item in case.get('items', []) for d in item.get('diagnostics', []))
            if unprocessed_skip and usage['imagesSkipped']:
                admitted_rate = completed / (attempted + usage['imagesSkipped'])
                if admitted_rate < minimum:
                    errors.append('unprocessed images reduce OCR coverage below threshold')
            recovery_pages = {(d.get('locator') or {}).get('page')
                              for item in case.get('items', []) for d in item.get('diagnostics', [])
                              if d.get('code') in ('pdf.recovery.pageImage', 'pdf.pageOcrPlacement', 'pdf.scannedPage')}
            recovery_pages.discard(None)
            if completed < minimum * len(recovery_pages):
                errors.append('recovery page images lack completed OCR')
        else:
            rate = None
        records.append(dict(id=case['id'], sourceSha256=case['sourceSha256'],
                            ocr=usage, excludedTransparentFailures=excluded,
                            completionRate=rate, errors=errors))
    return dict(schemaVersion=1, complete=report.get('complete', False),
                scope='OCR processing and recovery-page coverage; recognized text requires the independent text-bearing-image benchmark.',
                requiredCompletionRate=minimum,
                processingQualityPassed=report.get('complete', False) and all(not c['errors'] for c in records),
                conversionFailuresWithoutOcr=absent, cases=records)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=pathlib.Path, required=True)
    parser.add_argument('--proof', type=pathlib.Path)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    args = parser.parse_args()
    result = audit(json.loads(args.report.read_text()),
                   json.loads(args.proof.read_text()) if args.proof else None)
    result.update(reportSha256=digest(args.report), proofSha256=digest(args.proof) if args.proof else None)
    args.output.write_text(json.dumps(result, indent=2)+'\n')
    raise SystemExit(0 if result['processingQualityPassed'] else 1)
