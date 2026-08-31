#!/usr/bin/env python3
"""Record file signatures and embedded rights notices for a frozen external corpus.

Observations do not grant redistribution rights. Review the pinned upstream
license and each recorded notice before marking a file's rights review complete.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import zipfile

NOTICE_WORDS = ('license', 'licence', 'copyright', 'gutenberg', 'rights')
MAX_SCAN_BYTES = 32 * 1024 * 1024
MARKERS = {
    'Apache-2.0': b'apache.org/licenses/license-2.0',
    'Project-Gutenberg': b'project gutenberg',
    'W3C': b'w3.org/consortium/legal',
    'OFL-1.1': b'sil open font license',
    'LGPL': b'gnu lesser general public license',
    'MPL-2.0': b'mozilla public license',
    'rights-reserved': b'all rights reserved',
    'copyright-notice': b'copyright',
}


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def embedded_notices(path):
    if not zipfile.is_zipfile(path):
        return notice_record('file-content', path.read_bytes(), path.stat().st_size)
    result = []
    remaining = MAX_SCAN_BYTES
    with zipfile.ZipFile(path) as archive:
        for member in archive.infolist():
            name = member.filename.lower()
            if member.is_dir():
                continue
            record = {'path': member.filename, 'bytes': member.file_size}
            if member.flag_bits & 1 or member.file_size > remaining:
                record['inspection'] = 'encrypted-or-scan-bound; manual review required'
                result.append(record)
            else:
                try:
                    data = archive.read(member)
                except (NotImplementedError, zipfile.BadZipFile, RuntimeError) as error:
                    record['inspection'] = f'payload unreadable: {type(error).__name__}; manual review required'
                    result.append(record)
                    continue
                remaining -= len(data)
                notices = notice_record(member.filename, data, member.file_size)
                if notices:
                    result.extend(notices)
                elif any(word in name for word in NOTICE_WORDS):
                    record.update(sha256=sha256(data), markers=[], inspection='notice filename')
                    result.append(record)
    return result


def notice_record(name, data, size):
    # Retain marker names and hashes; source body text and email content stay local.
    lower = data.lower().replace(b'\x00', b'')
    markers = [name for name, marker in MARKERS.items() if marker in lower]
    if not markers:
        return []
    return [{'path': name, 'bytes': size, 'sha256': sha256(data), 'markers': markers,
             'inspection': 'marker occurrence; grant and scope require source review'}]


def content_type(path, kind):
    data = path.read_bytes()
    if kind in {'doc', 'xls', 'ppt', 'msg'}:
        return 'compound-binary' if data.startswith(bytes.fromhex('d0cf11e0a1b11ae1')) else 'mismatch'
    if kind == 'pdf':
        return 'pdf' if b'%PDF-' in data[:1024] else 'mismatch'
    if kind == 'rtf':
        return 'rtf' if data.lstrip().startswith(b'{\\rtf') else 'mismatch'
    if kind == 'ipynb':
        try:
            doc = json.loads(data)
            return 'notebook-json' if isinstance(doc, dict) and 'nbformat' in doc else 'mismatch'
        except (ValueError, UnicodeError):
            return 'mismatch'
    if kind == 'html':
        return 'html-markup' if re.search(rb'<(?:!doctype\s+html|html|head|body|p|div)\b', data.lower()) else 'mismatch'
    if kind in {'zip', 'docx', 'pptx', 'xlsx', 'odt', 'ods', 'odp', 'epub'}:
        if not zipfile.is_zipfile(path):
            return 'mismatch'
        with zipfile.ZipFile(path) as archive:
            names = archive.namelist()
            prefix = {'docx': 'word/', 'pptx': 'ppt/', 'xlsx': 'xl/'}.get(kind)
            if prefix:
                return kind if any(n.startswith(prefix) for n in names) else 'mismatch'
            if kind in {'odt', 'ods', 'odp', 'epub'}:
                expected = {'odt': b'application/vnd.oasis.opendocument.text',
                    'ods': b'application/vnd.oasis.opendocument.spreadsheet',
                    'odp': b'application/vnd.oasis.opendocument.presentation',
                    'epub': b'application/epub+zip'}[kind]
                valid = ('mimetype' in names and archive.getinfo('mimetype').file_size <= 128
                         and archive.read('mimetype').strip() == expected)
                return kind if valid else 'mismatch'
        return 'zip'
    return 'image-signature-reviewed-with-file'


def audit(manifest, root):
    samples = json.loads(manifest.read_text())['samples']
    result = []
    identities = set()
    for item in samples:
        path = root / item['path']
        if path.stat().st_size > MAX_SCAN_BYTES:
            raise ValueError(f'file exceeds frozen acquisition bound: {path}')
        digest = sha256(path.read_bytes())
        if digest != item['sha256'] or digest in identities or path.stat().st_size != item['size']:
            raise ValueError(f'changed or duplicate corpus entry: {path}')
        identities.add(digest)
        mime = subprocess.check_output(['file', '--brief', '--mime-type', str(path)], text=True).strip()
        description = subprocess.check_output(['file', '--brief', str(path)]).decode('utf-8', 'backslashreplace').strip()
        result.append({'sha256': digest, 'kind': item['kind'], 'observedMimeType': mime,
                       'contentTypeCheck': content_type(path, item['kind']),
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
