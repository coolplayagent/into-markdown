#!/usr/bin/env python3
"""Acquire only the current CI target's hash-pinned PDFium library."""
import argparse
import hashlib
import io
import json
import os
import pathlib
import tarfile
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', required=True)
    parser.add_argument('--root', type=pathlib.Path, required=True)
    args = parser.parse_args()
    manifest = json.loads(pathlib.Path('third_party/pdfium/manifest.json').read_text())
    record = manifest['targets'][args.target]
    with urllib.request.urlopen(f"{manifest['release_download_base']}/{record['asset']}", timeout=60) as response:
        archive = response.read(record['archive_size'] + 1)
    assert len(archive) == record['archive_size']
    assert hashlib.sha256(archive).hexdigest() == record['archive_sha256']
    with tarfile.open(fileobj=io.BytesIO(archive), mode='r:gz') as contents:
        member = contents.getmember(record['library'])
        assert member.isfile() and member.size == record['library_size']
        library = contents.extractfile(member).read(record['library_size'] + 1)
    assert hashlib.sha256(library).hexdigest() == record['library_sha256']
    args.root.mkdir(parents=True, exist_ok=True)
    root = args.root.resolve()
    path = root / pathlib.PurePosixPath(record['library']).name
    path.write_bytes(library)
    temporary = root / 'tmp'
    temporary.mkdir(exist_ok=True)
    if 'GITHUB_ENV' in os.environ:
        with open(os.environ['GITHUB_ENV'], 'a', encoding='utf-8') as output:
            for key, value in [('PDFIUM_LIBRARY', path), ('TMPDIR', temporary), ('TMP', temporary), ('TEMP', temporary)]:
                output.write(f'{key}={value}\n')
    print(path)


if __name__ == '__main__':
    main()
