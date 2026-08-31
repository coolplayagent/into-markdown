"""Run a hash-pinned public corpus through one CLI and an independent GFM consumer."""
import argparse
import base64
from collections import Counter
import hashlib
import html.parser
import datetime
import json
from pathlib import Path
import subprocess
import time
import urllib.parse

class Consumer(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.images = []
        self.tags = Counter()
        self.text = []

    def handle_starttag(self, tag, attrs):
        self.tags[tag] += 1
        if tag == 'img':
            self.images.extend(value for key, value in attrs if key == 'src')

    def handle_data(self, data):
        self.text.append(data)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def canonical_content(value):
    """Keep substantive IR data, including text marks, list labels and table shape."""
    if isinstance(value, list):
        return [item for child in value if (item := canonical_content(child)) is not None]
    if not isinstance(value, dict):
        return value
    if 'block' in value:
        block = value['block']
        data = block.get('data')
        inlines = data if isinstance(data, list) else (data or {}).get('content', []) if isinstance(data, dict) else []
        text = ''.join(item.get('data', {}).get('value', '') for item in inlines
                       if isinstance(item, dict) and isinstance(item.get('data'), dict))
        if block.get('type') in ('paragraph', 'heading') and (not text.strip() or text == 'Speaker notes'):
            # Empty placeholders and generated notes labels are intentional deltas.
            if not inlines or all(item.get('type') in ('text', 'sourceText', 'lineBreak') for item in inlines):
                return None
        return canonical_content(block)
    result = {}
    for key, item in value.items():
        if key in ('id', 'provenance', 'metadata', 'schemaVersion'):
            continue
        normalized = canonical_content(item)
        result[key] = sorted(normalized) if key == 'marks' else normalized
    return result


def verify_assets(result, consumer, output_path):
    inventory = Counter(sha(base64.b64decode(asset['dataBase64'], validate=True))
                        for asset in result['assets'] if asset['dataBase64'])
    images, failures = [], []
    for source in consumer.images:
        parsed = urllib.parse.urlsplit(source)
        if parsed.scheme or parsed.netloc:
            images.append({'uri': source, 'external': True})
            continue
        try:
            if parsed.query or parsed.fragment:
                raise ValueError('local asset became a query or fragment')
            target = output_path.parent / urllib.parse.unquote(parsed.path)
            digest = sha(target.read_bytes())
            if digest not in inventory:
                raise ValueError('resolved asset bytes differ from DTO inventory')
            images.append({'uri': source, 'sha256': digest, 'bytes': target.stat().st_size})
        except Exception as error:
            failures.append({'uri': source, 'error': str(error)})
    return {'images': images, 'failures': failures, 'inventory': dict(sorted(inventory.items()))}


def run_one(item, args):
    source = args.corpus / item['file']
    if sha(source.read_bytes()) != item['sha256']:
        raise ValueError(f'corpus hash changed: {source}')
    destination = args.output / Path(item['file']).with_suffix('')
    destination.mkdir(parents=True, exist_ok=True)
    output = destination / 'result.json'
    command = [str(args.cli), '--no-config', str(source.resolve()), '--ocr', 'off', '--error-policy', 'best-effort',
               '--max-memory-size', '2GiB', '--emit', 'result-json', '--asset-mode', 'extract',
               '--assets-dir', str((destination / '素材 中文 (a)#%&').resolve()),
               '--output', str(output.resolve()), '--conflict', 'overwrite']
    record = {'file': item['file'], 'sha256': item['sha256'], 'command': command,
              'startedAt': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'processTimeoutSeconds': args.process_timeout}
    start = time.monotonic()
    try:
        process = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=args.process_timeout)
        record.update(exit=process.returncode, seconds=round(time.monotonic() - start, 3))
        (destination / 'stderr.txt').write_bytes(process.stderr)
        if process.returncode:
            record.update(status='conversion-failed', error=process.stderr.decode('utf-8', 'replace')[-5000:])
            return record
        result = json.loads(output.read_text())
        markdown = result['markdown']
        md_path = destination / 'result.md'
        md_path.write_text(markdown)
        rendered = subprocess.run([str(args.probe), str(md_path)], capture_output=True, check=True).stdout
        (destination / 'result.html').write_bytes(rendered)
        consumer = Consumer()
        consumer.feed(rendered.decode('utf-8'))
        assets = verify_assets(result, consumer, output)
        record.update(status='success', markdownSha256=sha(markdown.encode()),
                      visibleTextSha256=sha(''.join(''.join(consumer.text).replace('Speaker notes', '').split()).encode()),
                      semanticSha256=sha(json.dumps(canonical_content(result['document']), sort_keys=True, ensure_ascii=False).encode()),
                      assetVerification=assets, htmlTags=dict(consumer.tags),
                      sourceMarkerComments=markdown.count('<!-- source-marker:'),
                      notesHeadings=markdown.count('Speaker notes'), diagnostics=result['diagnostics'])
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        record.update(status='qa-failed', error=str(error), seconds=round(time.monotonic() - start, 3))
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('cli', 'probe', 'corpus', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--revision', required=True)
    parser.add_argument('--process-timeout', type=int, default=120)
    args = parser.parse_args()
    args.cli = args.cli.resolve()
    args.probe = args.probe.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    manifest = json.loads((args.corpus / 'manifest.json').read_text())
    items = [item for group in manifest['formats'] for item in group['items']]
    report = {'schemaVersion': 1, 'revision': args.revision, 'cliSha256': sha(args.cli.read_bytes()),
              'consumer': 'pulldown-cmark 0.13.4 (GFM tables, strikethrough, tasks, footnotes)',
              'processTimeoutSeconds': args.process_timeout, 'items': []}
    for item in items:
        record = run_one(item, args)
        report['items'].append(record)
        (args.output / 'report.json').write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
        print(item['file'], record['status'], record.get('seconds'), flush=True)
    print(dict(Counter(item['status'] for item in report['items'])), flush=True)


if __name__ == '__main__':
    main()
