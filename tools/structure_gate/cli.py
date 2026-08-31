"""Local/CI commands. Only ratchet writes repository state."""

import argparse
import pathlib
import sys
import time

from .baseline import compare, encode, freeze, load_authority, load_baseline
from .model import AUTHORITY_PATH, BASELINE_PATH, BOOTSTRAP_COMMIT, GateError
from .reporting import make_report, print_text
from .scan import scan
from .source import Source
from .storage import replace_baseline
from .telemetry import peak_rss_bytes


def evaluate(root, base_ref, command):
    current = Source(root)
    candidate_bytes = current.optional(BASELINE_PATH)
    candidate = load_baseline(candidate_bytes) if candidate_bytes is not None else None
    authority = load_authority(current.optional(AUTHORITY_PATH))
    base_source = Source(root, base_ref) if base_ref else None
    before, _ = scan(base_source) if base_source else ({}, [])
    after, excluded = scan(current)
    frozen = freeze(after)
    violations = []
    if base_source:
        base_bytes = base_source.optional(BASELINE_PATH)
        if base_bytes is None:
            if base_source.ref != BOOTSTRAP_COMMIT:
                raise GateError("base baseline missing outside the pinned bootstrap commit")
            baseline = freeze(before)
        else:
            baseline = load_baseline(base_bytes)
            if baseline != freeze(before):
                raise GateError("base baseline is not the exact frozen production inventory")
        violations = compare(before, after, baseline, authority)
    elif candidate is not None:
        violations = compare(after, after, candidate, authority)
    if command == "check" and candidate is None:
        violations.append("candidate baseline missing; use ratchet against the exact base commit")
    if candidate is not None and candidate != frozen and command != "ratchet":
        violations.append("candidate baseline differs from measured debt; run ratchet (increases remain forbidden)")
    if command == "ratchet" and not violations:
        replace_baseline(root / BASELINE_PATH, candidate_bytes, encode(frozen))
    report = make_report(before, after, excluded, base_source.ref if base_source else None, violations)
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("report", "check", "ratchet"))
    parser.add_argument("--root", type=pathlib.Path, default=pathlib.Path(__file__).resolve().parents[2])
    parser.add_argument("--base-ref", help="exact PR base commit (required for check/ratchet)")
    parser.add_argument("--format", choices=("text", "json"), default="text")
    parser.add_argument("--output", type=pathlib.Path, help="optional report destination; otherwise stdout")
    args = parser.parse_args(argv)
    if args.command != "report" and not args.base_ref:
        parser.error("check and ratchet require --base-ref")
    start = time.perf_counter()
    try:
        report = evaluate(args.root.resolve(), args.base_ref, args.command)
        report["telemetry"] = {"analysis_seconds": round(time.perf_counter() - start, 3),
                               "peak_rss_bytes": peak_rss_bytes()}
        if args.output:
            if args.output.resolve().is_relative_to(args.root.resolve()):
                raise GateError("write reports outside the repository; only ratchet updates the baseline")
            if args.format != "json":
                raise GateError("--output requires --format json")
            args.output.write_bytes(encode(report))
            print(f"Structure gate: {len(report['violations'])} violations; report: {args.output}")
        elif args.format == "json":
            print(encode(report).decode("utf-8"), end="")
        else:
            print_text(report)
        if args.format != "json" or args.output:
            print(f"Analysis: {report['telemetry']['analysis_seconds']} s; peak RSS: {report['telemetry']['peak_rss_bytes']} bytes")
        return 1 if report["violations"] else 0
    except (GateError, OSError, ImportError, RecursionError) as error:
        print(f"Structure gate error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
