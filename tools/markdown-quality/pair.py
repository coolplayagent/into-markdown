"""Run both revisions in source-by-source pairs with identical conversion options."""
import argparse
from collections import Counter
import json
from pathlib import Path
from run import run_one, sha


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('baseline-cli', 'candidate-cli', 'probe', 'corpus', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    for name in ('baseline-revision', 'candidate-revision'):
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--process-timeout', type=int, default=600)
    args = parser.parse_args()
    args.probe = args.probe.resolve()
    manifest = json.loads((args.corpus / 'manifest.json').read_text())
    items = [item for group in manifest['formats'] for item in group['items']]
    root = args.output
    root.mkdir(parents=True, exist_ok=True)
    reports = {}
    for name in ('baseline', 'candidate'):
        cli = getattr(args, name + '_cli').resolve()
        reports[name] = {
            'schemaVersion': 1, 'revision': getattr(args, name + '_revision'),
            'cliSha256': sha(cli.read_bytes()),
            'consumer': 'pulldown-cmark 0.13.4 (GFM tables, strikethrough, tasks, footnotes)',
            'processTimeoutSeconds': args.process_timeout,
            'policy': 'All sources paired; identical explicit CLI options; user configuration disabled.',
            'items': [],
        }
    for item in items:
        for name, report in reports.items():
            args.cli = getattr(args, name + '_cli').resolve()
            if sha(args.cli.read_bytes()) != report['cliSha256']:
                raise ValueError('CLI bytes changed during paired runs')
            args.output = root / name
            attempt = run_one(item, args)
            report['items'].append(attempt)
            (root / (name + '.json')).write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
            print(name, item['file'], attempt['status'], attempt.get('seconds'), flush=True)
    for name, report in reports.items():
        print(name, dict(Counter(item['status'] for item in report['items'])), flush=True)


if __name__ == '__main__':
    main()
