"""Repeat the XLSX cohort as paired runs, retaining every initial and repeat attempt."""
import argparse
import json
from pathlib import Path
from run import run_one, sha


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('baseline', 'candidate', 'baseline-cli', 'candidate-cli', 'probe', 'corpus', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    args.probe = args.probe.resolve()
    reports = {name: json.loads(getattr(args, name).read_text()) for name in ('baseline', 'candidate')}
    manifest = json.loads((args.corpus / 'manifest.json').read_text())
    items = next(group['items'] for group in manifest['formats'] if group['format'] == 'xlsx')
    args.process_timeout = 600
    root = args.output
    for item in items:
        for name, report in reports.items():
            args.cli = getattr(args, name + '_cli').resolve()
            assert sha(args.cli.read_bytes()) == report['cliSha256'], 'candidate binary changed'
            args.output = root / name
            original = next(row for row in report['items'] if row['file'] == item['file'])
            attempt = run_one(item, args)
            history = original.get('attempts', []) + [{key: value for key, value in original.items() if key != 'attempts'}]
            original.clear()
            original.update(attempt, attempts=history)
            report['repeatPolicy'] = 'All eleven XLSX sources repeated in baseline/candidate pairs; identical CLI parameters; subprocess allowance 600 s; initial attempts retained.'
            (root / (name + '.json')).write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
            print(name, item['file'], attempt['status'], attempt.get('seconds'), flush=True)


if __name__ == '__main__':
    main()
