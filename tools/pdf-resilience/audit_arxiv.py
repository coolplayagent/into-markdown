#!/usr/bin/env python3
"""Audit page delivery and native-text retention against independently extracted PDF text."""
import argparse
import collections
import html
import hashlib
import json
import pathlib
import re
import unicodedata


def normalize(value):
    value = html.unescape(re.sub(r'\\([!"#$%&\'()*+,\-./:;<=>?@\[\]^_`{|}~])', r'\1', value))
    return ''.join(c for c in unicodedata.normalize('NFKC', value).casefold() if c.isalnum())


def ngrams(value, length=12):
    return {value[i:i + length] for i in range(max(0, len(value) - length + 1))}


def audit(manifest, report, inventory):
    papers = {p['id']: p for p in json.loads(manifest.read_text())['papers']}
    run = json.loads(report.read_text())
    with manifest.open('rb') as stream:
        manifest_sha = hashlib.file_digest(stream, 'sha256').hexdigest()
    if run['manifestSha256'] != manifest_sha:
        raise ValueError('audit manifest differs from the replay authority')
    cases = []
    for case in run['cases']:
        paper = papers[case['id']]
        source = json.loads((inventory / case['id'] / 'text.json').read_text())
        document = report.parent / case['id'] / case['ocr'] / 'document.md'
        page_text = {}
        if document.exists():
            sections = re.split(r'<a id="pdf-page-(\d+)"></a>', document.read_text())
            page_text = {int(sections[i]): sections[i + 1] for i in range(1, len(sections), 2)}
        diagnostics = [d for item in case.get('items', []) for d in item.get('diagnostics', [])]
        pages = []
        counts = collections.Counter()
        for number in range(1, paper['pages'] + 1):
            body = page_text.get(number, '')
            related = [d for d in diagnostics if (d.get('locator') or {}).get('page') == number]
            codes = {d['code'] for d in related}
            kind = ('missing' if not body and not case.get('documentOriginalFallback') else
                    'originalPdf' if case.get('documentOriginalFallback') or 'pdf.recovery.originalPdf' in codes else
                    'pageImage' if 'pdf.recovery.pageImage' in codes else
                    'nativeText' if 'pdf.recovery.nativeText' in codes else 'structured')
            counts[kind] += 1
            expected = normalize(source[number - 1]); actual = normalize(body)
            expected_grams = ngrams(expected); actual_grams = ngrams(actual)
            recall = len(expected_grams & actual_grams) / len(expected_grams) if expected_grams else None
            source_lines = [normalize(line) for line in source[number - 1].splitlines()]
            source_lines = [line for line in source_lines if len(line) >= 12]
            line_recall = (sum(line in actual for line in source_lines) / len(source_lines)
                           if source_lines else None)
            pages.append(dict(page=number, representation=kind, nativeSourceChars=len(expected),
                              nativeTextNgramRecall=round(recall, 4) if recall is not None else None,
                              nativeSourceLineRecall=round(line_recall, 4) if line_recall is not None else None,
                              reviewRequired=kind != 'structured' or bool(related) or number in paper['features']['reviewPages'],
                              diagnostics=related))
        cases.append(dict(id=paper['id'], ocr=case['ocr'], passed=case['passed'],
                          counts=dict(counts), pages=pages))
    return dict(schemaVersion=1, manifestSha256=run['manifestSha256'], binarySha256=run['binarySha256'],
                textMetric='NFKC alphanumeric native-source 12-character ngram recall and containment of source lines with at least 12 characters; visual recovery is separately classified',
                manualReviewComplete=False, cases=cases)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('manifest', 'report', 'inventory', 'output'):
        parser.add_argument('--' + name, type=pathlib.Path, required=True)
    args = parser.parse_args()
    args.output.write_text(json.dumps(audit(args.manifest, args.report, args.inventory), ensure_ascii=False, indent=2) + '\n')
