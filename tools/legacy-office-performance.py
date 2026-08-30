#!/usr/bin/env python3
"""Compare a baseline and candidate Core CLI on the Office 97-2003 corpus."""

from __future__ import annotations

import argparse
import json
import pathlib
import tempfile

from legacy_office_perf.constants import FORMATS
from legacy_office_perf.core_runner import baseline_report, executable_report
from legacy_office_perf.corpus import xls_corpus_report
from legacy_office_perf.gates import gate
from legacy_office_perf.oracle import (
    load_xls_authority,
    load_xls_oracle,
    verify_xls_oracle,
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-cli", type=pathlib.Path, required=True)
    parser.add_argument("--candidate-cli", type=pathlib.Path, required=True)
    parser.add_argument("--fixtures", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--parallelism", type=int, default=3)
    parser.add_argument("--xls-corpus", type=pathlib.Path)
    parser.add_argument("--xls-authority", type=pathlib.Path)
    parser.add_argument("--xls-xlrd-path", type=pathlib.Path)
    parser.add_argument("--xls-regression-limit", type=float, default=0.5)
    return parser.parse_args()

def main() -> int:
    args = parse_args()
    if args.iterations < 3 or not 2 <= args.parallelism <= len(FORMATS):
        raise SystemExit("iterations must be >= 3 and parallelism must be between 2 and 3")
    xls_arguments = (args.xls_corpus, args.xls_authority, args.xls_xlrd_path)
    if any(value is not None for value in xls_arguments) and any(
        value is None for value in xls_arguments
    ):
        raise SystemExit(
            "--xls-corpus, --xls-authority, and --xls-xlrd-path must be supplied together"
        )
    if not 0 <= args.xls_regression_limit < 1:
        raise SystemExit("--xls-regression-limit must be in the range 0..1")
    baseline = args.baseline_cli.resolve(strict=True)
    candidate = args.candidate_cli.resolve(strict=True)
    fixtures = args.fixtures.resolve(strict=True)
    output = args.output.resolve(strict=False)
    if output.exists() or not output.parent.is_dir():
        raise SystemExit("output must be a new file in an existing directory")
    for format_name in FORMATS:
        if not (fixtures / f"normal.{format_name}").is_file():
            raise SystemExit(f"missing normal.{format_name}")
    xls_corpus = args.xls_corpus.resolve(strict=True) if args.xls_corpus else None
    xls_authority = args.xls_authority.resolve(strict=True) if args.xls_authority else None
    xls_xlrd_path = args.xls_xlrd_path.resolve(strict=True) if args.xls_xlrd_path else None
    authority = (
        load_xls_authority(xls_authority, xls_corpus)
        if xls_authority is not None and xls_corpus is not None
        else None
    )
    oracle_tool = pathlib.Path(__file__).with_name("legacy-xls-oracle.py").resolve(strict=True)
    oracle_bundle = (
        load_xls_oracle(
            oracle_tool,
            xls_xlrd_path,
            xls_corpus,
            xls_authority,
        )
        if xls_xlrd_path is not None
        and xls_corpus is not None
        and xls_authority is not None
        else None
    )
    if oracle_bundle is not None:
        oracle, classifications = oracle_bundle
        authority = [
            {
                **item,
                "authorityValid": item["valid"],
                "safetyExpectedFailure": classifications[str(item["file"])][
                    "expectedOutcome"
                ]
                == "fail-closed",
                "classificationReason": classifications[str(item["file"])]["reason"],
            }
            for item in authority
        ]
    else:
        oracle = None
    with tempfile.TemporaryDirectory(prefix="into-md-office-performance-") as temporary:
        root = pathlib.Path(temporary)
        baseline_metrics = baseline_report(baseline, fixtures, root / "baseline", args.iterations)
        candidate_metrics, candidate_failures = executable_report(
            candidate, fixtures, root / "candidate", args.iterations, args.parallelism
        )
        failures = [*baseline_metrics["failures"], *candidate_failures]
        xls_metrics, xls_checks = (
            xls_corpus_report(
                baseline,
                candidate,
                xls_corpus,
                authority,
                root / "xls-corpus",
                args.iterations,
                args.xls_regression_limit,
                oracle,
            )
            if xls_corpus is not None and authority is not None and oracle is not None
            else (None, [])
        )
    checks, checks_passed = gate(candidate_metrics, baseline_metrics)
    checks.extend(xls_checks)
    checks_passed = checks_passed and all(check["passed"] for check in xls_checks)
    report = {
        "schemaVersion": 2,
        "corpus": "repository-authored-office-97-2003",
        "baseline": baseline_metrics,
        "candidate": candidate_metrics,
        "xlsCorpus": xls_metrics,
        "gates": checks,
        "failures": failures,
        "passed": checks_passed and not failures,
    }
    temporary_output = output.with_name(f".{output.name}.tmp")
    temporary_output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    temporary_output.replace(output)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
