#!/usr/bin/env python3
"""Record file signatures and embedded rights notices for a frozen external corpus.

Observations do not grant redistribution rights. Review the pinned upstream
license and each recorded notice before marking a file's rights review complete.
"""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import zipfile

MAX_NOTICE_BYTES = 256 * 1024
NOTICE_WORDS = ('license', 'licence', 'copyright', 'gutenberg', 'rights')


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def embedded_notices(path):
    if not zipfile.is_zipfile(path):
        return []
    result = []
    with zipfile.ZipFile(path) as archive:
        for member in archive.infolist():
            name = member.filename.lower()
            if not any(word in name for word in NOTICE_WORDS) or member.is_dir():
                continue
            record = {'path': member.filename, 'bytes': member.file_size}
            if member.flag_bits & 1 or member.file_size > MAX_NOTICE_BYTES:
                record['inspection'] = 'encrypted-or-over-bound; manual review required'
            else:
                record['sha256'] = sha256(archive.read(member))
                record['inspection'] = 'notice content fingerprinted; rights review required'
            result.append(record)
    return result


def audit(manifest, root):
    samples = json.loads(manifest.read_text())['samples']
    result = []
    identities = set()
    for item in samples:
        path = root / item['path']
        digest = sha256(path.read_bytes())
        if digest != item['sha256'] or digest in identities or path.stat().st_size != item['size']:
            raise ValueError(f'changed or duplicate corpus entry: {path}')
        identities.add(digest)
        mime = subprocess.check_output(['file', '--brief', '--mime-type', str(path)], text=True).strip()
        description = subprocess.check_output(['file', '--brief', str(path)]).decode('utf-8', 'backslashreplace').strip()
        result.append({'sha256': digest, 'kind': item['kind'], 'observedMimeType': mime,
                       'signatureDescription': description, 'embeddedNotices': embedded_notices(path)})
    return {'schemaVersion': 1, 'manifestSha256': sha256(manifest.read_bytes()),
            'signatureTool': subprocess.check_output(['file', '--version'], text=True).splitlines()[0],
            'rightsReview': 'pending per-file assessment; signatures and notice hashes establish no license',
            'files': result}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.write_text(json.dumps(audit(args.manifest, args.root), indent=2) + '\n')


if __name__ == '__main__':
    main()
