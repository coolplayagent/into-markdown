#!/usr/bin/env python3
"""Compare image OCR against an independently classified, hash-pinned corpus."""
import argparse
import html
import json
import pathlib
import re
import unicodedata

from quality_gate import check_ocr, digest


def recognized_text(text):
    text = re.sub(r'!\[[^\]]*\]\([^\n]*?\)', '', text)
    text = re.sub(r'^## Image frame \d+\s*$', '', text, flags=re.M)
    text = re.sub(r'<[^>]+>', '', text)
    text = re.sub(r'\\([^\w\s])', r'\1', text)
    return ' '.join(unicodedata.normalize('NFKC', html.unescape(text)).split())


def words(text):
    return re.findall(r'\w+', recognized_text(text).casefold())


def compare(manifest_path, baseline_path, candidate_path, minimum=0.95, frames_path=None):
    manifest = json.loads(manifest_path.read_text())
    baseline = json.loads(baseline_path.read_text())
    candidate = json.loads(candidate_path.read_text())
    errors, rows = [], []
    expected = {p['id']: p for p in manifest['papers']}
    frames = json.loads(frames_path.read_text()) if frames_path else None
    if frames and (frames.get('manifestSha256') != digest(manifest_path)
                   or set(frames.get('frameCounts', {})) != set(expected)):
        raise ValueError('independent frame inventory differs from source manifest')
    if len(expected) != len(manifest['papers']):
        errors.append('duplicate source identities')
    runs = {}
    for label, report, path in [('baseline', baseline, baseline_path), ('candidate', candidate, candidate_path)]:
        cases = {c['id']: c for c in report['cases']}
        if (not report.get('complete') or set(cases) != set(expected)
                or len(cases) != len(report['cases']) or report.get('modes') != ['auto']
                or report.get('manifestSha256') != digest(manifest_path)):
            errors.append(label + ' run inventory or manifest authority differs')
        runs[label] = (cases, path.parent)
    for identity, source in expected.items():
        if type(source.get('eligibleText')) is not bool or not source.get('eligibility'):
            errors.append(identity + ': missing independent source classification')
            continue
        row = dict(id=identity, sourceSha256=source['sha256'], eligibleText=source['eligibleText'],
                   eligibility=source['eligibility'])
        count = frames['frameCounts'][identity] if frames else 1
        if type(count) is not int or count < 1:
            raise ValueError('invalid independent source frame count')
        row['imageFrames'] = count
        tokens, texts = {}, {}
        for label, (cases, root) in runs.items():
            case = cases.get(identity, {})
            usage = (case.get('resourceUsage') or {}).get('ocrRuntime')
            accounting = check_ocr(usage)
            # Older binaries can lack counters; keep that absence visible.
            if label == 'candidate' and accounting:
                errors.extend(identity + ': ' + e for e in accounting)
            document = root / identity / 'auto/document.md'
            valid = (case.get('sourceSha256') == source['sha256'] and document.is_file()
                     and digest(document) == case.get('markdownSha256'))
            body = document.read_text() if valid else ''
            texts[label], tokens[label] = recognized_text(body), words(body)
            success = bool(case.get('passed') and valid and tokens[label]
                           and (label == 'baseline' or (not accounting and usage['imagesWithText'] >= count)))
            row[label] = dict(deliveryPassed=bool(case.get('passed')), textRecovered=success,
                              ocr=usage, markdownSha256=case.get('markdownSha256'),
                              wordCount=len(tokens[label]))
            if label == 'candidate' and not case.get('passed'):
                errors.append(identity + ': candidate delivery failed')
        # This is a diagnostic, not a claim that token overlap establishes accuracy.
        from collections import Counter
        old, new = Counter(tokens['baseline']), Counter(tokens['candidate'])
        row['baselineWordRecall'] = (sum((old & new).values()) / sum(old.values())) if old else None
        row['sameRecognizedWords'] = tokens['baseline'] == tokens['candidate']
        row['sameRecognizedText'] = texts['baseline'] == texts['candidate']
        if source['eligibleText'] and row['baseline']['textRecovered'] and not row['sameRecognizedText']:
            errors.append(identity + ': previously recognized source text changed; independent source review is required')
        rows.append(row)
    eligible = [r for r in rows if r['eligibleText']]
    eligible_images = sum(r['imageFrames'] for r in eligible)
    recognized = sum(r['imageFrames'] for r in eligible if r['candidate']['textRecovered'])
    rate = recognized / eligible_images if eligible_images else 0
    if rate < minimum:
        errors.append(f'eligible image OCR success {recognized}/{eligible_images} below {minimum:.0%}')
    return dict(schemaVersion=1, passed=not errors, failures=errors,
                manifestSha256=digest(manifest_path), baselineReportSha256=digest(baseline_path),
                candidateReportSha256=digest(candidate_path), baselineBinarySha256=baseline.get('binarySha256'),
                candidateBinarySha256=candidate.get('binarySha256'),
                frameInventorySha256=digest(frames_path) if frames_path else None,
                totalInputs=len(rows), eligibleInputs=len(eligible),
                totalImages=sum(r['imageFrames'] for r in rows), eligibleImages=eligible_images,
                excludedImages=sum(r['imageFrames'] for r in rows if not r['eligibleText']),
                eligibleRecognized=recognized, eligibleSuccessRate=rate, requiredSuccessRate=minimum,
                changedTextCases=[r['id'] for r in eligible if not r['sameRecognizedWords']], cases=rows)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('manifest', 'baseline', 'candidate', 'output'):
        parser.add_argument('--'+name, type=pathlib.Path, required=True)
    parser.add_argument('--frames', type=pathlib.Path, help='independent source frame-count inventory')
    args = parser.parse_args()
    result = compare(args.manifest, args.baseline, args.candidate, frames_path=args.frames)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2)+'\n')
    raise SystemExit(0 if result['passed'] else 1)
