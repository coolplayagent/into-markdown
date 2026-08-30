#!/usr/bin/env python3
"""Run the public XLS content oracle against strict and best-effort CLI output."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile

import xlrd


def load_sibling(name: str, filename: str):
    path = pathlib.Path(__file__).with_name(filename)
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cli", type=pathlib.Path, required=True)
    parser.add_argument("--fixture", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    return parser.parse_args()


def sha256(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main() -> int:
    args = parse_args()
    if xlrd.__version__ != "2.0.2":
        raise SystemExit(f"public XLS gate requires xlrd 2.0.2, got {xlrd.__version__}")
    cli = args.cli.resolve(strict=True)
    fixture = args.fixture.resolve(strict=True)
    output = args.output.resolve(strict=False)
    if output.exists() or not output.parent.is_dir():
        raise SystemExit("output must be a new file in an existing directory")
    oracle_module = load_sibling("legacy_xls_oracle", "legacy-xls-oracle.py")
    performance_module = load_sibling(
        "legacy_office_performance", "legacy-office-performance.py"
    )
    oracle = oracle_module.workbook_oracle(fixture)
    results: dict[str, object] = {}
    with tempfile.TemporaryDirectory(prefix="into-md-public-xls-") as temporary:
        root = pathlib.Path(temporary)
        for policy in ("strict", "best-effort"):
            ir = root / f"{policy}.ir.json"
            report = root / f"{policy}.report.json"
            completed = subprocess.run(
                [
                    str(cli),
                    "--no-config",
                    str(fixture),
                    "--format",
                    "xls",
                    "--error-policy",
                    policy,
                    "--emit",
                    "ir-json",
                    "--asset-mode",
                    "embed",
                    "--quiet",
                    "--output",
                    str(ir),
                    "--report",
                    str(report),
                ],
                check=False,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=30,
            )
            if completed.returncode != 0 or not ir.is_file() or not report.is_file():
                raise SystemExit(
                    f"{policy} public XLS conversion failed: "
                    + completed.stderr.decode("utf-8", errors="replace")
                )
            quality = performance_module.verify_xls_oracle(ir, oracle)
            if not quality["verified"]:
                raise SystemExit(f"{policy} public XLS oracle failed: {quality['errors']}")
            results[policy] = {
                "irSha256": sha256(ir),
                "qualityOracle": quality,
                "report": json.loads(report.read_text(encoding="utf-8")),
            }
    if results["strict"]["irSha256"] != results["best-effort"]["irSha256"]:
        raise SystemExit("canonical public XLS output differs between strict and best-effort")
    payload = {
        "schemaVersion": 1,
        "fixture": fixture.name,
        "oracle": "xlrd-2.0.2-eager-cell-read",
        "policies": results,
        "passed": True,
    }
    output.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
