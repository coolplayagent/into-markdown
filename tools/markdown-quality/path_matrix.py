"""Check resolved asset bytes across filesystem spellings, CLI routes, and asset modes."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import subprocess
import urllib.parse
import zipfile
from run import Consumer


def check_markdown(markdown, base, probe, mode, digests, bundle=None):
    path = base / 'consumer.md'
    path.write_text(markdown)
    html = subprocess.run([str(probe), str(path)], capture_output=True, check=True).stdout
    (base / 'consumer.html').write_bytes(html)
    consumer = Consumer()
    consumer.feed(html.decode())
    if mode == 'omit':
        assert not consumer.images, 'omit emitted an image'
    else:
        assert consumer.images, 'image disappeared'
    for uri in consumer.images:
        if uri.startswith('data:'):
            data = base64.b64decode(uri.split(',', 1)[1], validate=True)
        else:
            parsed = urllib.parse.urlsplit(uri)
            assert not (parsed.scheme or parsed.netloc or parsed.query or parsed.fragment), uri
            name = urllib.parse.unquote(parsed.path)
            data = bundle.read(name) if bundle else (base / name).read_bytes()
        assert hashlib.sha256(data).hexdigest() in digests, uri
    return consumer.images


def run_case(args, spelling, route, mode):
    root = args.output / str(spelling[0]) / route / mode
    root.mkdir(parents=True, exist_ok=True)
    command = [str(args.cli), '--no-config', str(args.fixture), '--ocr', 'off', '--asset-mode', mode,
               '--assets-dir', spelling[1], '--conflict', 'overwrite']
    if route == 'file':
        command += ['--output', 'result.md']
    elif route == 'batch':
        (root / '第二份.odt').write_bytes(args.fixture.read_bytes())
        command += ['第二份.odt', '--output-dir', '.', '--report', 'batch.json']
    elif route == 'bundle':
        command += ['--emit', 'bundle', '--output', 'result.mdpkg.zip']
    record = {'spelling': spelling[1], 'route': route, 'mode': mode, 'command': command}
    try:
        result = subprocess.run(command, cwd=root, capture_output=True, timeout=30)
        record['exit'] = result.returncode
        (root / 'stderr.txt').write_bytes(result.stderr)
        if result.returncode:
            raise ValueError(result.stderr.decode())
        if route == 'bundle':
            with zipfile.ZipFile(root / 'result.mdpkg.zip') as bundle:
                markdown = bundle.read('document.md').decode()
                record['images'] = check_markdown(markdown, root, args.probe, mode, args.digests, bundle)
            assert not (root / spelling[1]).exists(), 'bundle leaked external asset directory'
        else:
            markdowns = [result.stdout.decode()] if route == 'stdout' else [path.read_text() for path in root.glob('*.md') if path.name != 'consumer.md']
            assert len(markdowns) == (2 if route == 'batch' else 1), 'primary output disappeared'
            record['images'] = [uri for markdown in markdowns for uri in check_markdown(markdown, root, args.probe, mode, args.digests)]
        record['status'] = 'passed'
    except Exception as error:
        record.update(status='failed', error=str(error))
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('cli', 'probe', 'fixture', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    for name in ('cli', 'probe', 'fixture', 'output'):
        setattr(args, name, getattr(args, name).resolve())
    with zipfile.ZipFile(args.fixture) as archive:
        args.digests = {hashlib.sha256(archive.read(name)).hexdigest() for name in archive.namelist() if name.startswith('Pictures/')}
    spellings = ['中文素材', 'assets with spaces', '(图)#?%&', 'literal%20', 'e\u0301组合', 'nested/../assets', '中文\u00a0空白']
    results = [run_case(args, pair, route, mode) for pair in enumerate(spellings)
               for route in ['file', 'stdout', 'batch', 'bundle'] for mode in ['extract', 'embed', 'omit']]
    report = {'cliSha256': hashlib.sha256(args.cli.read_bytes()).hexdigest(),
              'fixtureSha256': hashlib.sha256(args.fixture.read_bytes()).hexdigest(), 'items': results}
    (args.output / 'report.json').write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
    failed = sum(row['status'] != 'passed' for row in results)
    print(f'{len(results)} cases, {failed} failures')
    if failed:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
