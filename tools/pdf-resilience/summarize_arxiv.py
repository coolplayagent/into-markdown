#!/usr/bin/env python3
"""Produce compact, source-bound evidence from two complete corpus replays."""
import argparse
import collections
import hashlib
import json
import pathlib
import statistics


def summarize(manifest, baseline, candidate, audit, quality_gate=None):
    with manifest.open('rb') as stream:
        authority = hashlib.file_digest(stream, 'sha256').hexdigest()
    papers = json.loads(manifest.read_text())['papers']
    runs = [json.loads(path.read_text()) for path in (baseline, candidate)]
    quality = json.loads(audit.read_text())
    expected = {(paper['id'], mode) for paper in papers for mode in ('auto', 'off')}
    for run in runs:
        keys = [(case['id'], case['ocr']) for case in run['cases']]
        if run['manifestSha256'] != authority or len(keys) != len(expected) or set(keys) != expected:
            raise ValueError('evidence requires every frozen source and both OCR modes exactly once')
    if quality['manifestSha256'] != authority or quality['binarySha256'] != runs[1]['binarySha256']:
        raise ValueError('quality evidence belongs to another corpus or executable')
    quality_cases = {(c['id'], c['ocr']): c for c in quality['cases']}
    if set(quality_cases) != expected:
        raise ValueError('quality evidence is incomplete')
    content_gate = json.loads(quality_gate.read_text()) if quality_gate else None
    if content_gate:
        with candidate.open('rb') as stream:
            report_sha = hashlib.file_digest(stream, 'sha256').hexdigest()
        if (content_gate['reportSha256'] != report_sha
                or content_gate['binarySha256'] != runs[1]['binarySha256']):
            raise ValueError('content gate belongs to another replay')
    records = []
    totals = []
    for run in runs:
        totals.append(dict(binarySha256=run['binarySha256'], version=run['version'],
                           passed=sum(c['passed'] for c in run['cases']),
                           failed=sum(not c['passed'] for c in run['cases']),
                           seconds=round(sum(c['seconds'] for c in run['cases']), 3)))
    indexes = [{(c['id'], c['ocr']): c for c in run['cases']} for run in runs]
    for paper in papers:
        record = {key: paper[key] for key in ('id', 'version', 'title', 'group', 'pages', 'sha256', 'url')}
        record['results'] = {}
        for mode in ('auto', 'off'):
            pair = {}
            for label, cases in zip(('baseline', 'candidate'), indexes):
                case = cases[(paper['id'], mode)]
                item = (case.get('items') or [{}])[0]
                pair[label] = dict(passed=case['passed'], exitCode=case['exitCode'],
                                   seconds=case['seconds'], outcome=item.get('outcome'),
                                   errorCode=item.get('errorCode'), reason=item.get('message'),
                                   markdownSha256=case.get('markdownSha256'),
                                   peakRssBytes=case.get('peakRssBytes'),
                                   effectiveBudgetBytes=(case.get('resourceUsage') or {}).get('memory', {}).get('effectiveBudgetBytes'),
                                   sharedLeasePeakBytes=(case.get('resourceUsage') or {}).get('sharedLeasePeakBytes'),
                                   ocrRuntime=(case.get('resourceUsage') or {}).get('ocrRuntime'),
                                   diagnosticCounts=dict(collections.Counter(d['code'] for d in item.get('diagnostics', []))))
            pair['pageRepresentations'] = quality_cases[(paper['id'], mode)]['counts']
            pages = quality_cases[(paper['id'], mode)]['pages']
            recalls = [p['nativeTextNgramRecall'] for p in pages if p['nativeTextNgramRecall'] is not None]
            pair['nativeTextNgramRecallMean'] = round(statistics.mean(recalls), 4) if recalls else None
            pair['manualReviewRequiredPages'] = [p['page'] for p in pages if p['reviewRequired']]
            record['results'][mode] = pair
        records.append(record)
    return dict(schemaVersion=1, manifestSha256=authority, sources=len(papers),
                sourcePages=sum(p['pages'] for p in papers), expectedConversions=len(expected),
                conversionAcceptancePassed=totals[1]['failed'] == 0,
                releaseAcceptancePassed=totals[1]['failed'] == 0 and bool(content_gate and content_gate['qualityPassed']),
                contentQualityPassed=bool(content_gate and content_gate['qualityPassed']),
                manualReviewComplete=quality['manualReviewComplete'], baseline=totals[0], candidate=totals[1], papers=records)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('manifest', 'baseline', 'candidate', 'audit', 'output'):
        parser.add_argument('--' + name, type=pathlib.Path, required=True)
    parser.add_argument('--quality-gate', type=pathlib.Path, required=True)
    args = parser.parse_args()
    args.output.write_text(json.dumps(summarize(args.manifest, args.baseline, args.candidate, args.audit, args.quality_gate), ensure_ascii=False, indent=2) + '\n')
