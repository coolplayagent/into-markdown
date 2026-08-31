#!/usr/bin/env python3
"""Freeze independently sourced documents before running OCR memory experiments."""

import argparse
import concurrent.futures
import hashlib
import io
import json
from pathlib import Path
import subprocess
import urllib.parse
import urllib.request
import zipfile

KINDS = 'pdf doc docx ppt pptx xls xlsx odt ods odp rtf epub html ipynb zip msg image'.split()
REPOS = {
    'apache/poi': ['test-data/'],
    'apache/tika': ['tika-parsers/'],
    'LibreOffice/core': ['sd/qa/', 'sw/qa/', 'sc/qa/'],
    'jupyter/nbconvert': ['tests/', 'docs/'],
}
MAX_BYTES = 32 * 1024 * 1024


def github(endpoint):
    return json.loads(subprocess.check_output(['gh', 'api', endpoint], text=True))


def tree(repo, revision, prefix, cache):
    """Query subtrees so GitHub's recursive-tree truncation cannot hide candidates."""
    identity = hashlib.sha256(f'{repo}/{revision}/{prefix}'.encode()).hexdigest()
    saved = cache / f'{identity}.json'
    if saved.exists():
        return json.loads(saved.read_text())
    node = revision
    for component in prefix.strip('/').split('/'):
        if component:
            entries = github(f'repos/{repo}/git/trees/{node}')['tree']
            node = next(x['sha'] for x in entries if x['path'] == component)
    result = github(f'repos/{repo}/git/trees/{node}?recursive=1')
    if result.get('truncated'):
        raise RuntimeError(f'truncated source inventory: {repo}/{prefix}')
    saved.write_text(json.dumps(result))
    return result


def candidates(cache):
    result = {kind: [] for kind in KINDS}
    for repo, prefixes in REPOS.items():
        pin = cache / (repo.replace('/', '-') + '-revision.txt')
        if not pin.exists():
            pin.write_text(github(f'repos/{repo}/commits/HEAD')['sha'])
        revision = pin.read_text().strip()
        for prefix in prefixes:
            for entry in tree(repo, revision, prefix, cache)['tree']:
                path = prefix + entry['path']
                ext = Path(path).suffix.lower().lstrip('.')
                kind = 'image' if ext in {'png', 'jpg', 'jpeg', 'tif', 'tiff', 'webp', 'bmp'} else ext
                if kind not in result or entry['type'] != 'blob':
                    continue
                if not 256 <= entry.get('size', 0) <= MAX_BYTES:
                    continue
                url = f'https://raw.githubusercontent.com/{repo}/{revision}/{urllib.parse.quote(path)}'
                result[kind].append(dict(kind=kind, extension=ext, url=url, repository=repo,
                                         revision=revision, sourcePath=path, size=entry['size'],
                                         sourceClass='upstream-regression',
                                         rightsUrl=f'https://github.com/{repo}/tree/{revision}',
                                         redistribution='external-download-only'))
    for book in [11, 84, 98, 174, 2701, 1342, 1661, 1952, 2542, 345, 5200, 64317]:
        result['epub'].append(dict(kind='epub', extension='epub',
            url=f'https://www.gutenberg.org/ebooks/{book}.epub3.images',
            repository='Project Gutenberg', revision=None, sourcePath=str(book), size=0,
            sourceClass='published-book', rightsUrl=f'https://www.gutenberg.org/ebooks/{book}',
            redistribution='external-download-only; retain book-specific license'))
    return result


def score(item):
    name = item['sourcePath'].lower()
    negative = any(word in name for word in ['encrypt', 'password', 'malformed', 'corrupt',
        'invalid', 'error', 'crash', 'fuzz', 'broken', 'bug', 'cve', 'bomb'])
    visual = any(word in name for word in ['image', 'picture', 'graphic', 'photo', 'embedded', 'sample'])
    digest = hashlib.sha256(item['url'].encode()).hexdigest()
    return negative, not visual, digest


def download(item):
    req = urllib.request.Request(item['url'], headers={'User-Agent': 'IntoMarkdown-Issue340-Evidence'})
    with urllib.request.urlopen(req, timeout=45) as response:
        data = response.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES:
        raise ValueError('sample exceeds acquisition bound')
    if data.startswith(b'version https://git-lfs.github.com/spec/'):
        raise ValueError('Git LFS pointer requires separate content acquisition')
    if item['kind'] == 'epub' and not zipfile.is_zipfile(io.BytesIO(data)):
        raise ValueError('EPUB response is not a ZIP container')
    entry = dict(item, sha256=hashlib.sha256(data).hexdigest(), size=len(data))
    if zipfile.is_zipfile(io.BytesIO(data)):
        with zipfile.ZipFile(io.BytesIO(data)) as archive:
            names = archive.namelist()
            entry['embeddedRasterEntries'] = sum(Path(n).suffix.lower() in {
                '.png', '.jpg', '.jpeg', '.gif', '.tiff', '.bmp', '.webp'} for n in names)
            entry['archiveEntries'] = len(names)
            entry['encryptedEntries'] = sum(bool(x.flag_bits & 1) for x in archive.infolist())
    entry['expectation'] = 'record content preservation and typed outcome; source classification frozen before conversion'
    return entry, data


def acquire(root, manifest):
    root.mkdir(parents=True, exist_ok=True)
    cache = root / 'inventory'
    cache.mkdir(exist_ok=True)
    if manifest.exists():
        raise SystemExit('manifest already frozen; use fetch to reproduce its exact inputs')
    pools = candidates(cache)
    selected, rejected, identities = [], [], set()
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
        for kind in KINDS:
            pool = sorted(pools[kind], key=score)
            count = 0
            for start in range(0, len(pool), 11):
                pending = [(item, executor.submit(download, item)) for item in pool[start:start + 11]]
                for item, future in pending:
                    try:
                        entry, data = future.result()
                    except Exception as error:
                        rejected.append(dict(url=item['url'], reason=str(error)))
                        continue
                    if entry['sha256'] in identities or count >= 11:
                        continue
                    identities.add(entry['sha256'])
                    entry['path'] = f"samples/{kind}/{entry['sha256'][:16]}.{entry['extension']}"
                    dest = root / entry['path']
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    dest.write_bytes(data)
                    selected.append(entry)
                    count += 1
                if count == 11:
                    break
            print(f'{kind}: {count}/11', flush=True)
            if count != 11:
                (root / 'incomplete.json').write_text(json.dumps({'samples': selected, 'rejected': rejected}, indent=2))
                raise SystemExit(f'insufficient distinct samples for {kind}')
    manifest.parent.mkdir(parents=True, exist_ok=True)
    manifest.write_text(json.dumps({'schemaVersion': 1, 'samples': selected,
        'acquisitionFailures': rejected}, indent=2, ensure_ascii=False) + '\n')


def fetch(root, manifest):
    for item in json.loads(manifest.read_text())['samples']:
        path = root / item['path']
        if path.exists() and hashlib.sha256(path.read_bytes()).hexdigest() == item['sha256']:
            continue
        entry, data = download(item)
        if entry['sha256'] != item['sha256']:
            raise SystemExit(f"upstream content drift: {item['url']}")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['acquire', 'fetch'])
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--manifest', type=Path, required=True)
    args = parser.parse_args()
    {'acquire': acquire, 'fetch': fetch}[args.action](args.root, args.manifest)


if __name__ == '__main__':
    main()
