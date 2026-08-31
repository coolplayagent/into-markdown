"""Replay a recorded public corpus without consulting mutable listing pages."""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
from corpus import fetch, verify_format


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    manifest = json.loads(args.manifest.read_text())
    attempts = []
    args.output.mkdir(parents=True, exist_ok=True)
    for group in manifest['formats']:
        for item in group['items']:
            record = {'url': item['url'], 'file': item['file'], 'expectedSha256': item['sha256'],
                      'attemptedAt': datetime.datetime.now(datetime.timezone.utc).isoformat()}
            try:
                destination = args.output / item['file']
                if not destination.resolve().is_relative_to(args.output.resolve()):
                    raise ValueError('manifest path escapes corpus directory')
                data, final, content_type = fetch(item['url'])
                record.update(finalUrl=final, contentType=content_type, bytes=len(data),
                              sha256=hashlib.sha256(data).hexdigest())
                verify_format(data, group['format'])
                if record['sha256'] != item['sha256'] or len(data) != item['bytes']:
                    raise ValueError('source bytes changed from pinned acquisition')
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(data)
                record['status'] = 'verified'
            except Exception as error:
                record.update(status='failed', error=str(error))
            attempts.append(record)
            (args.output / 'replay-attempts.json').write_text(json.dumps(attempts, indent=2) + '\n')
    (args.output / 'manifest.json').write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + '\n')
    if any(item['status'] != 'verified' for item in attempts):
        raise SystemExit('replay incomplete; see replay-attempts.json')


if __name__ == '__main__':
    main()
