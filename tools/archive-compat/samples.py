"""Fetch the pinned, cache-only public issue-338 corpus; never extract to source paths."""
from __future__ import annotations
import argparse
import binascii
import concurrent.futures
import hashlib
import json
from pathlib import Path
import urllib.request

HERE = Path(__file__).resolve().parent
MANIFEST = HERE / 'samples.json'
MAX_DOWNLOAD = 40 * 1024 * 1024

def sha256(data):
    return hashlib.sha256(data).hexdigest()

def download(url):
    request = urllib.request.Request(url, headers={'User-Agent': 'into-markdown-public-regression'})
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(MAX_DOWNLOAD + 1)
    if len(data) > MAX_DOWNLOAD:
        raise ValueError(f'oversize public fixture: {url}')
    return data

def decode_uu(data):
    # Decode only payload bytes; the uuencoded filename never controls a filesystem path.
    lines = data.splitlines()
    start = next(i for i, line in enumerate(lines) if line.startswith(b'begin ')) + 1
    end = next(i for i in range(start, len(lines)) if lines[i] == b'end')
    return b''.join(binascii.a2b_uu(line) for line in lines[start:end] if line)

def fetch_one(sample, root, record=False):
    path = root / sample['kind'] / sample['name']
    path.parent.mkdir(parents=True, exist_ok=True)
    if not record and path.exists() and sha256(path.read_bytes()) == sample['sha256']:
        return sample
    upstream = download(sample['url'])
    if not record and sha256(upstream) != sample['upstream_sha256']:
        raise ValueError(f'upstream hash drift: {sample["name"]}')
    payload = decode_uu(upstream) if sample.get('encoding') == 'uu' else upstream
    if not record and sha256(payload) != sample['sha256']:
        raise ValueError(f'payload hash drift: {sample["name"]}')
    sample = dict(sample, sha256=sha256(payload), upstream_sha256=sha256(upstream), bytes=len(payload))
    path.write_bytes(payload)
    return sample

def fetch(root, record=False):
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        samples = list(pool.map(lambda item: fetch_one(item, root, record), manifest['samples']))
    if record:
        manifest['samples'] = samples
        MANIFEST.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + '\n', encoding='utf-8')
    for kind in ('pptx', 'zip', 'rar', 'epub'):
        chosen = [item for item in samples if item['kind'] == kind]
        assert len(chosen) >= 12 and len({item['sha256'] for item in chosen}) == len(chosen), kind
    return samples

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--cache', type=Path, required=True)
    parser.add_argument('--record', action='store_true', help='maintainer-only: record reviewed upstream hashes')
    args = parser.parse_args()
    print(json.dumps({'verified_samples': len(fetch(args.cache, args.record))}))
