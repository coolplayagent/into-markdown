"""Fetch public Office documents into a local-only, hash-pinned QA corpus."""
import argparse
import concurrent.futures
import datetime
import hashlib
import html.parser
import io
import json
from pathlib import Path
import urllib.parse
import urllib.request
import zipfile

SOURCES = {
    'docx': [
        'https://www.scotcourts.gov.uk/about-us/user-groups/personal-injury-users-group',
        'https://www.childrenscommissioner.gov.uk/corporate-governance/audit-and-risk-committee/',
        'https://chevingtonparishcouncil.gov.uk/meetings/',
    ],
    'xlsx': [
        'https://www.ons.gov.uk/census/aboutcensus/censusproducts/topicsummaries',
        'https://www.ons.gov.uk/peoplepopulationandcommunity/populationandmigration/populationprojections/adhocs/29452022basednationalpopulationprojectionshighlifeexpectancyvariantdatasetsforgreatbritainsummaryandmachinereadable',
        'https://www.ons.gov.uk/economy/inflationandpriceindices/datasets/outputandinputproducerpriceinflationcontributionstothe12monthrates',
        'https://www.ons.gov.uk/peoplepopulationandcommunity/wellbeing/datasets/publicopinionsandsocialtrendsgreatbritainworkingarrangements',
    ],
    'pptx': ['https://ece.uwaterloo.ca/~dwharder/aads/Lecture_materials/'],
    'ppt': ['https://people.eecs.ku.edu/~hossein/710/Lectures/'],
    'odp': ['https://cs.uwaterloo.ca/~iang/cs456/F06-lectures/'],
}
MAX_BYTES = 32 * 1024 * 1024

class Links(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.links = []

    def handle_starttag(self, tag, attrs):
        if tag == 'a':
            self.links.extend(value for key, value in attrs if key == 'href' and value)


def fetch(url):
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != 'https' or not parsed.hostname or parsed.username or parsed.password:
        raise ValueError('expected public HTTPS URL')
    request = urllib.request.Request(url, headers={'User-Agent': 'Into-Markdown-issue-341-QA/1.0'})
    with urllib.request.urlopen(request, timeout=40) as response:
        data = response.read(MAX_BYTES + 1)
        if len(data) > MAX_BYTES:
            raise ValueError('download exceeds 32 MiB QA limit')
        return data, response.url, response.headers.get('Content-Type')


def verify_format(data, extension):
    if extension == 'ppt':
        if not data.startswith(bytes.fromhex('d0cf11e0a1b11ae1')):
            raise ValueError('PPT CFB signature missing')
        return
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        expected = {'docx': 'word/document.xml', 'xlsx': 'xl/workbook.xml',
                    'pptx': 'ppt/presentation.xml', 'odp': 'content.xml'}[extension]
        if expected not in archive.namelist():
            raise ValueError('expected document part missing')
        if extension == 'odp' and archive.read('mimetype') != b'application/vnd.oasis.opendocument.presentation':
            raise ValueError('ODP mimetype mismatch')


def candidates(extension, pages, attempts):
    buckets = []
    for page in pages:
        try:
            data, final, _ = fetch(page)
            parser = Links()
            parser.feed(data.decode('utf-8', 'replace'))
            links = sorted(set(urllib.parse.urljoin(final, value) for value in parser.links
                               if urllib.parse.urlsplit(value).path.lower().endswith('.' + extension)
                               or urllib.parse.urlsplit(value).query.lower().endswith('.' + extension)))
            buckets.append([(page, url) for url in links])
            attempts.append({'page': page, 'status': 'listed', 'candidates': len(links)})
        except Exception as error:
            attempts.append({'page': page, 'status': 'failed', 'error': str(error)})
    # Interleave sources so repeated monthly editions do not dominate a format.
    return [bucket[index] for index in range(max(map(len, buckets), default=0))
            for bucket in buckets if index < len(bucket)]


def collect_format(extension, root, minimum):
    attempts, items, hashes = [], [], set()
    for page, url in candidates(extension, SOURCES[extension], attempts):
        record = {'format': extension, 'url': url, 'sourcePage': page}
        try:
            data, final, content_type = fetch(url)
            verify_format(data, extension)
            digest = hashlib.sha256(data).hexdigest()
            if digest in hashes:
                attempts.append(dict(record, status='duplicate', sha256=digest))
                continue
            hashes.add(digest)
            filename = f'{extension}/{len(items) + 1:02}-{digest[:16]}.{extension}'
            destination = root / filename
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(data)
            record.update(file=filename, sha256=digest, bytes=len(data), finalUrl=final,
                          contentType=content_type, fetchedAt=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                          distribution='local QA only; source copyright retained; no redistribution grant inferred')
            items.append(record)
            attempts.append(dict(record, status='downloaded'))
            print(f'{extension}: {len(items)}/{minimum} {len(data)} bytes', flush=True)
            if len(items) >= minimum:
                break
        except Exception as error:
            attempts.append(dict(record, status='failed', error=str(error)))
    return {'format': extension, 'items': items, 'attempts': attempts}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--minimum', type=int, default=11)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=5) as pool:
        results = list(pool.map(lambda extension: collect_format(extension, args.output, args.minimum), SOURCES))
    report = {'schemaVersion': 1, 'minimumPerFormat': args.minimum, 'formats': results}
    (args.output / 'manifest.json').write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
    counts = {result['format']: len(result['items']) for result in results}
    print(json.dumps(counts), flush=True)
    if any(count < args.minimum for count in counts.values()):
        raise SystemExit('public sample minimum not reached; see recorded download failures')


if __name__ == '__main__':
    main()
