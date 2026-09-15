#!/usr/bin/env python3
"""Compare real Web and CLI conversions against identical source bytes and options."""
import argparse
import base64
import contextlib
import itertools
import hashlib
import json
import pathlib
import re
import signal
import subprocess
import time
import urllib.request

COUNTERS = ('imageSources', 'imagesAttempted', 'imagesCompleted', 'imagesWithText', 'imagesFailed', 'imagesSkipped', 'requests')


def sha(data):
    return hashlib.sha256(data).hexdigest()


def encode(value):
    return base64.urlsafe_b64encode(value).decode().rstrip('=')


def normalized_markdown(text):
    # Presentation paths differ because Web downloads use opaque artifact IDs.
    # Keep filenames, visible text, ordering and every non-asset character intact.
    return re.sub(r'(?<=<)(?:[^<>\n]*?/)?(asset-[^/<>\n]+)(?=>)', r'\1', text)


def file_sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def copy_response(response, destination=None):
    digest = hashlib.sha256()
    size = 0
    with (destination.open('wb') if destination else contextlib.nullcontext(None)) as output:
        while chunk := response.read(64 * 1024):
            digest.update(chunk)
            size += len(chunk)
            if output is not None:
                output.write(chunk)
    return digest.hexdigest(), size


def markdown_equal(left, right):
    if file_sha(left) == file_sha(right):
        return True
    with left.open(encoding='utf-8') as a, right.open(encoding='utf-8') as b:
        return all(x is not None and y is not None and normalized_markdown(x) == normalized_markdown(y)
                   for x, y in itertools.zip_longest(a, b))


def run(args):
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=True)
    executable = args.into_md.resolve()
    request_template = json.loads(args.request.read_text())
    assert request_template['options']['error_policy'] == 'best-effort'
    assert request_template['options']['ocr']['policy'] == 'auto'
    assert 'limits' not in request_template['options']
    report = {'schemaVersion': 1, 'binarySha256': sha(executable.read_bytes()),
              'requestSha256': sha(args.request.read_bytes()), 'cases': []}
    session_log = root / 'session.log'
    with session_log.open('wb') as log:
        server = subprocess.Popen([str(executable), '--no-config', 'ui', '--no-open', '--data-dir', str(root / 'state')], stdout=log, stderr=log)
    session_log.chmod(0o600)
    try:
        deadline = time.monotonic() + 60
        while True:
            found = re.search(r'(http://127\.0\.0\.1:\d+)/#[^=\s]+=([^\s]+)', session_log.read_text())
            if found:
                origin, token = found.groups()
                break
            if server.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('Web service did not start; inspect private session log')
            time.sleep(.2)

        def call(path, data=None, headers=None):
            headers = {'X-Into-Md-Session': token, 'Origin': origin, **(headers or {})}
            with urllib.request.urlopen(urllib.request.Request(origin + path, data=data, headers=headers), timeout=120) as response:
                return b"".join(iter(lambda: response.read(64 * 1024), b""))

        for index, source in enumerate(args.sources):
            source = source.resolve()
            content = source.read_bytes()
            for mode in ('auto', 'off'):
                case_root = root / f'{index:03}-{mode}'
                case_root.mkdir()
                cli_md = case_root / 'document.md'
                cli_report = case_root / 'cli.json'
                command = [str(executable), '--no-config', '--error-policy', 'best-effort', '--ocr', mode,
                           '--report', str(cli_report), '--output', str(cli_md), str(source)]
                with (case_root / 'cli.log').open('wb') as log:
                    cli = subprocess.run(command, stdout=log, stderr=log, timeout=args.timeout)
                request = json.loads(json.dumps(request_template))
                request['options']['ocr']['policy'] = mode
                task = json.loads(call('/api/tasks', content, {
                    'Content-Type': 'application/octet-stream',
                    'X-Into-Md-Filename-B64': encode(source.name.encode()),
                    'X-Into-Md-Request': encode(json.dumps(request).encode()),
                }))
                task_id = task['id']
                deadline = time.monotonic() + args.timeout
                while task['status'] not in ('succeeded', 'failed', 'interrupted', 'cancelled'):
                    if time.monotonic() > deadline:
                        raise RuntimeError(f'Web conversion watchdog expired: {source.name} {mode}')
                    time.sleep(.5)
                    task = json.loads(call('/api/tasks/' + task_id))
                (case_root / 'web-task.json').write_text(json.dumps(task, indent=2))
                artifacts = {}
                asset_hashes = []
                for artifact in task['artifacts']:
                    destination = (case_root / ('web.md' if artifact['kind'] == 'markdown' else 'web-diagnostics.json')
                                   if artifact['kind'] in ('markdown', 'diagnostics') else None)
                    download = urllib.request.Request(
                        origin + f'/api/tasks/{task_id}/artifacts/{artifact["storageKey"]}',
                        headers={'X-Into-Md-Session': token, 'Origin': origin})
                    with urllib.request.urlopen(download, timeout=120) as response:
                        artifact_sha, artifact_bytes = copy_response(response, destination)
                    if artifact_sha != artifact['sha256'] or artifact_bytes != artifact['byteLen']:
                        raise ValueError('Web artifact integrity mismatch')
                    if artifact['kind'] == 'asset':
                        asset_hashes.append(artifact_sha)
                    elif artifact['kind'] in ('markdown', 'diagnostics'):
                        artifacts[artifact['kind']] = destination
                result = {'source': source.name, 'sourceSha256': sha(content), 'ocr': mode,
                          'cliExit': cli.returncode, 'webStatus': task['status']}
                if cli.returncode == 0 and task['status'] == 'succeeded':
                    cli_data = json.loads(cli_report.read_text())
                    web_diagnostics = json.loads(artifacts['diagnostics'].read_text())
                    cli_ocr = cli_data['resourceUsage']['ocrRuntime']
                    web_ocr = web_diagnostics.get('ocrRuntime')
                    cli_assets = sorted(file_sha(p) for p in cli_md.with_name('document_assets').glob('*') if p.is_file())
                    result.update(bodyEqual=markdown_equal(cli_md, artifacts['markdown']),
                                  imagesEqual=cli_assets == sorted(asset_hashes),
                                  ocrEqual=web_ocr is not None and all(cli_ocr[k] == web_ocr[k] for k in COUNTERS),
                                  cliOcr=cli_ocr, webOcr=web_ocr,
                                  cliMarkdownSha256=file_sha(cli_md), webMarkdownSha256=file_sha(artifacts['markdown']),
                                  assetHashes=sorted(asset_hashes))
                result['passed'] = all(result.get(k, False) for k in ('bodyEqual', 'imagesEqual', 'ocrEqual'))
                report['cases'].append(result)
                report['passed'] = sum(case['passed'] for case in report['cases'])
                report['failed'] = len(report['cases']) - report['passed']
                (root / 'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
                print(f'{source.name} {mode}: {result["passed"]}', flush=True)
    finally:
        server.send_signal(signal.SIGINT)
        try:
            server.wait(timeout=30)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()
    return int(report['failed'] != 0)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--into-md', type=pathlib.Path, required=True)
    parser.add_argument('--request', type=pathlib.Path, required=True)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    parser.add_argument('--timeout', type=int, default=3600)
    parser.add_argument('sources', nargs='+', type=pathlib.Path)
    raise SystemExit(run(parser.parse_args()))
