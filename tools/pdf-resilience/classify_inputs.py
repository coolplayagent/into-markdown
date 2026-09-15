#!/usr/bin/env python3
"""Separate proven unreadable encrypted inputs from conversion failures."""
import argparse
import json
import pathlib

from quality_gate import digest


def classify(report_path, proof_path):
    report = json.loads(report_path.read_text())
    proof = json.loads(proof_path.read_text())
    evidence = {p['id']: p for p in proof['records']}
    if len(evidence) != len(proof['records']):
        raise ValueError('duplicate source availability identities')
    unreadable, failures = [], []
    for case in report['cases']:
        if case['passed']:
            continue
        source = evidence.get(case['id'], {})
        codes = [i.get('errorCode') for i in case.get('items', [])]
        entry = dict(id=case['id'], sourceSha256=case['sourceSha256'],
                     exitCode=case['exitCode'], reportedErrorCodes=codes)
        if (source.get('encrypted') is True and source.get('evidence')
                and source.get('sourceSha256') == case['sourceSha256']
                and codes and all(code in ('encrypted', 'unsupported') for code in codes)
                and case['exitCode'] is not None and not case.get('watchdogExpired')):
            entry['reason'] = 'Encrypted source content is unavailable without a password.'
            unreadable.append(entry)
        else:
            failures.append(entry)
    return dict(schemaVersion=1, complete=bool(report.get('complete')),
                readableConversionAcceptancePassed=bool(report.get('complete')) and not failures,
                reportSha256=digest(report_path), sourceAvailabilitySha256=digest(proof_path),
                binarySha256=report['binarySha256'], attemptedInputs=len(report['cases']),
                deliveredInputs=sum(c['passed'] for c in report['cases']),
                failedDeliveryCount=sum(not c['passed'] for c in report['cases']),
                unreadableInputCount=len(unreadable), conversionFailureCount=len(failures),
                unreadableInputs=unreadable, conversionFailures=failures)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('report', 'proof', 'output'):
        parser.add_argument('--'+name, type=pathlib.Path, required=True)
    args = parser.parse_args()
    result = classify(args.report, args.proof)
    args.output.write_text(json.dumps(result, indent=2)+'\n')
    raise SystemExit(0 if result['readableConversionAcceptancePassed'] else 1)
