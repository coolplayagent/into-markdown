"""Compare public-corpus conversion, substantive IR, and resolved asset inventories."""
import argparse
from collections import Counter
import json
from pathlib import Path


def compare(before, after, reviewed=()):
    baseline = {item['file']: item for item in before['items']}
    candidate = {item['file']: item for item in after['items']}
    if baseline.keys() != candidate.keys():
        raise ValueError('baseline and candidate source sets differ')
    rows = []
    reviews = {item['file']: item for item in reviewed}
    if len(reviews) != len(reviewed):
        raise ValueError('duplicate rendered-text review')
    for name, old in baseline.items():
        new = candidate[name]
        if old['sha256'] != new['sha256']:
            raise ValueError(f'source hash differs: {name}')
        row = {'file': name, 'baseline': old['status'], 'candidate': new['status']}
        if old['status'] == new['status'] == 'success':
            row.update(contentEqual=old['semanticSha256'] == new['semanticSha256'],
                       visibleTextEqual=old['visibleTextSha256'] == new['visibleTextSha256'],
                       assetsEqual=old['assetVerification']['inventory'] == new['assetVerification']['inventory'],
                       candidateAssetFailures=new['assetVerification']['failures'],
                       baselineAssetFailures=old['assetVerification']['failures'],
                       baselineTags=old['htmlTags'], candidateTags=new['htmlTags'],
                       baselineNotes=old['notesHeadings'], candidateNotes=new['notesHeadings'],
                       removedMarkerComments=old['sourceMarkerComments'] - new['sourceMarkerComments'])
            if name in reviews:
                review = reviews.pop(name)
                if (review['sourceSha256'], review['baselineVisibleTextSha256'], review['candidateVisibleTextSha256']) != (
                        old['sha256'], old['visibleTextSha256'], new['visibleTextSha256']):
                    raise ValueError(f'rendered-text review is stale: {name}')
                row['reviewedVisibleDelta'] = review['reason']
        rows.append(row)
    if reviews:
        raise ValueError('rendered-text review does not match successful corpus items')
    regressions = [row for row in rows if row['baseline'] == 'success' and
                   (row['candidate'] != 'success' or not row['contentEqual'] or
                    (not row['visibleTextEqual'] and not row.get('reviewedVisibleDelta')) or
                    not row['assetsEqual'] or row['candidateAssetFailures'])]
    return {'baselineRevision': before['revision'], 'candidateRevision': after['revision'],
            'baselineCliSha256': before['cliSha256'], 'candidateCliSha256': after['cliSha256'],
            'consumer': after['consumer'], 'testedByFormat': dict(Counter(name.split('/')[0] for name in baseline)),
            'regressions': regressions, 'items': rows}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('baseline', 'candidate', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--reviewed-deltas', type=Path,
                        help='explicit, source- and output-hash-bound review of intentional visible changes')
    args = parser.parse_args()
    reviewed = json.loads(args.reviewed_deltas.read_text()) if args.reviewed_deltas else []
    result = compare(json.loads(args.baseline.read_text()), json.loads(args.candidate.read_text()), reviewed)
    args.output.write_text(json.dumps(result, indent=2, ensure_ascii=False) + '\n')
    print(f"{len(result['items'])} documents; {len(result['regressions'])} substantive regressions")
    if result['regressions']:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
