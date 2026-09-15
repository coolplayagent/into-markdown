#!/usr/bin/env python3
"""Check frozen conversion expectations independently of successful exit codes.

The expectation file is reviewed source evidence, never generated from the run
under test. The same checks apply to documents, standalone images and archives.
"""
import argparse
import hashlib
import html
import json
import pathlib
import re
import unicodedata

COUNTERS = ('imageSources', 'imagesAttempted', 'imagesCompleted',
            'imagesWithText', 'imagesFailed', 'imagesSkipped')


def normalize_ligatures(text):
    return unicodedata.normalize('NFC', text).translate(str.maketrans({
        'ﬀ': 'ff', 'ﬁ': 'fi', 'ﬂ': 'fl', 'ﬃ': 'ffi',
        'ﬄ': 'ffl', 'ﬅ': 'st', 'ﬆ': 'st'}))


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def check_ocr(usage):
    errors = []
    if not isinstance(usage, dict) or any(type(usage.get(k)) is not int or usage[k] < 0 for k in COUNTERS):
        return ['OCR image accounting is absent or invalid']
    if usage['imagesCompleted'] + usage['imagesFailed'] != usage['imagesAttempted']:
        errors.append('OCR attempted outcomes do not balance')
    if usage['imagesAttempted'] + usage['imagesSkipped'] != usage['imageSources']:
        errors.append('OCR source outcomes do not balance')
    if usage['imagesWithText'] > usage['imagesCompleted']:
        errors.append('OCR text results exceed completed recognition')
    return errors


def check_case(case, expectation, document, reviews, binary_sha, review_policy='all-recovery'):
    errors = []
    if not case.get('passed'):
        errors.append('conversion did not pass')
    if case.get('sourceSha256') != expectation['sourceSha256']:
        errors.append('source differs from frozen expectation')
    if not document.is_file():
        return errors + ['Markdown artifact is missing']
    output_sha = digest(document)
    if output_sha != case.get('markdownSha256'):
        errors.append('Markdown differs from conversion report')
    assets = case.get('assets', [])
    for asset in assets:
        path = (document.parent / asset['path']).resolve()
        if not path.is_relative_to(document.parent.resolve()) or not path.is_file():
            errors.append('delivered asset missing or outside output: ' + asset['path'])
        elif digest(path) != asset['sha256']:
            errors.append('delivered asset differs from conversion report: ' + asset['path'])
    artifact_sha = hashlib.sha256(json.dumps(
        [output_sha, sorted((asset['path'], asset['sha256']) for asset in assets)],
        separators=(',', ':')).encode()).hexdigest()
    text = html.unescape(document.read_text(encoding='utf-8'))
    text = re.sub(r'\\([!"#$%&\'()*+,\-./:;<=>?@\[\]^_`{|}~])', r'\1', text)
    # Exact fragments deliberately retain spaces, table relationships and OCR
    # words. Character-set recall alone cannot establish these properties.
    # Typeset ligatures and their expanded letters represent the same words.
    # Preserve whitespace so lost word boundaries remain a quality failure.
    semantic_text = normalize_ligatures(text)
    semantic_text = re.sub(r'</?(?:sup|sub)>', '', semantic_text)
    for fragment in expectation.get('requiredText', []):
        if normalize_ligatures(fragment) not in semantic_text:
            errors.append('required content missing: ' + fragment)
    for pattern in expectation.get('requiredTextPatterns', []):
        if not pattern or not re.search(pattern, text):
            errors.append('required source expression missing: ' + pattern)
    for fragment, count in expectation.get('exactTextCounts', {}).items():
        if type(count) is not int or count < 0 or not fragment:
            errors.append('invalid exact content count')
        elif text.count(fragment) != count:
            errors.append(f'content occurrence count differs: {fragment}: {text.count(fragment)} != {count}')
    for fragment in expectation.get('forbiddenText', []):
        if fragment in text:
            errors.append('forbidden corruption present: ' + fragment)
    usage = (case.get('resourceUsage') or {}).get('ocrRuntime')
    errors.extend(check_ocr(usage))
    if not check_ocr(usage):
        for field, minimum in expectation.get('ocrMinimums', {}).items():
            if field not in COUNTERS or type(minimum) is not int or minimum < 0:
                errors.append('invalid OCR minimum: ' + field)
            elif usage[field] < minimum:
                errors.append(f'{field} below reviewed minimum {minimum}: {usage[field]}')
        for field, maximum in expectation.get('ocrMaximums', {}).items():
            if field not in COUNTERS or type(maximum) is not int or maximum < 0:
                errors.append('invalid OCR maximum: ' + field)
            elif usage[field] > maximum:
                errors.append(f'{field} above reviewed maximum {maximum}: {usage[field]}')
    diagnostics = [d for item in case.get('items', []) for d in item.get('diagnostics', [])]
    if expectation.get('requireRecoveryOcr') and not check_ocr(usage):
        recovery_pages = {(d.get('locator') or {}).get('page') for d in diagnostics
                          if d.get('code') == 'pdf.recovery.pageImage'}
        recovery_pages.discard(None)
        if usage['imagesCompleted'] < len(recovery_pages):
            errors.append('recovery page images lack completed OCR')
    for code, maximum in expectation.get('diagnosticMaximums', {}).items():
        count = sum(d.get('code') == code for d in diagnostics)
        if count > maximum:
            errors.append(f'{code} exceeds reviewed maximum {maximum}: {count}')
    required = set(expectation.get('reviewPages', []))
    required.update((d.get('locator') or {}).get('page') for d in diagnostics
                    if d.get('code', '').startswith('pdf.recovery.'))
    required.discard(None)
    if not expectation.get('requiredText') and not required:
        errors.append('expectation has no source content or visual checks')
    reviewed = {review.get('page') for review in reviews
                if review.get('id') == case['id'] and review.get('ocr') == case['ocr']
                and review.get('sourceSha256') == case.get('sourceSha256')
                and review.get('markdownSha256') == output_sha
                and review.get('artifactSha256') == artifact_sha
                and review.get('binarySha256') == binary_sha
                and review.get('verdict') == 'pass' and review.get('notes')}
    if review_policy == 'all-recovery' and required - reviewed:
        errors.append('source/output visual comparison pending: ' + str(sorted(required - reviewed)))
    return errors


def run(expectations, report, reviews, review_policy='all-recovery'):
    if review_policy not in ('all-recovery', 'source-content'):
        raise ValueError('unknown visual review policy')
    policy = json.loads(expectations.read_text())
    observed = json.loads(report.read_text())
    ledger = json.loads(reviews.read_text())
    expected = {(c['id'], c['ocr']): c for c in policy['cases']}
    actual = {(c['id'], c['ocr']): c for c in observed['cases']}
    failures = []
    if len(expected) != len(policy['cases']) or len(actual) != len(observed['cases']):
        failures.append(dict(case=None, errors=['duplicate case identities']))
    if expected.keys() != actual.keys():
        failures.append(dict(case=None, errors=['case inventory differs from frozen expectations']))
    if not observed.get('complete'):
        failures.append(dict(case=None, errors=['conversion run is incomplete']))
    for key in sorted(expected.keys() & actual.keys()):
        document = report.parent / key[0] / key[1] / 'document.md'
        errors = check_case(actual[key], expected[key], document, ledger['reviews'], observed['binarySha256'], review_policy)
        if errors:
            failures.append(dict(case=list(key), errors=errors))
    return dict(schemaVersion=1, qualityPassed=not failures, failures=failures,
                expectationSha256=digest(expectations), reportSha256=digest(report),
                reviewSha256=digest(reviews), binarySha256=observed['binarySha256'],
                reviewPolicy=review_policy,
                allRecoveryPagesRequired=review_policy == 'all-recovery')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('expectations', 'report', 'reviews', 'output'):
        parser.add_argument('--' + name, type=pathlib.Path, required=True)
    parser.add_argument('--review-policy', choices=('all-recovery', 'source-content'),
                        default='all-recovery', help='source-content retains content, asset and OCR checks; visual records remain supplementary')
    args = parser.parse_args()
    result = run(args.expectations, args.report, args.reviews, args.review_policy)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    raise SystemExit(0 if result['qualityPassed'] else 1)
