#!/usr/bin/env python3
"""Prepare licensed corpora and promote minimized local fuzz failures."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

TARGETS = ("zip", "xml", "rtf", "pdf", "office", "media", "plugin_protocol")
MAX_SEED_BYTES = 2 * 1024 * 1024


def repository_root(explicit: str | None = None) -> Path:
    return Path(explicit).resolve() if explicit else Path(__file__).resolve().parents[1]


def contained(root: Path, relative: str) -> Path:
    candidate = (root / relative).resolve()
    if candidate != root and root not in candidate.parents:
        raise ValueError(f"path escapes repository: {relative}")
    return candidate


def load_json(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as handle:
        value = json.load(handle)
    if not isinstance(value, dict):
        raise ValueError(f"expected JSON object: {path}")
    return value


def prepare(root: Path, target: str, destination: Path | None = None) -> Path:
    validate_target(target)
    authority = load_json(root / "fuzz" / "seeds.json")
    if authority.get("schema_version") != 1 or authority.get("license") != "Apache-2.0":
        raise ValueError("fuzz seed authority is invalid")
    paths = authority.get("targets", {}).get(target)
    if not isinstance(paths, list) or not paths:
        raise ValueError(f"target has no declared seeds: {target}")
    output = contained(root, str(destination)) if destination else root / "fuzz" / "corpus" / target
    output.mkdir(parents=True, exist_ok=True)
    for existing in output.iterdir():
        if existing.is_dir() and not existing.is_symlink():
            raise ValueError(f"corpus contains unexpected directory: {existing}")
        existing.unlink()
    fixture_manifest = load_json(root / "fixtures" / "manifest.json")
    fixture_records = {
        f"fixtures/{item['path']}": item for item in fixture_manifest.get("fixtures", [])
    }
    for relative in paths:
        if not isinstance(relative, str):
            raise ValueError("seed path must be a string")
        source = contained(root, relative)
        if not source.is_file() or source.is_symlink():
            raise ValueError(f"seed is not a regular file: {relative}")
        payload = source.read_bytes()
        if len(payload) > MAX_SEED_BYTES:
            raise ValueError(f"seed exceeds {MAX_SEED_BYTES} bytes: {relative}")
        digest = hashlib.sha256(payload).hexdigest()
        if relative.startswith("fixtures/"):
            record = fixture_records.get(relative)
            if record is None:
                raise ValueError(f"seed is absent from fixture manifest: {relative}")
            if record.get("license", {}).get("spdx") != "Apache-2.0":
                raise ValueError(f"seed license is not Apache-2.0: {relative}")
        shutil.copyfile(source, output / f"{digest[:16]}-{source.name}")
    regressions = load_json(root / "fuzz" / "regressions" / "manifest.json")
    if regressions.get("schema_version") != 1 or not isinstance(regressions.get("fixtures"), list):
        raise ValueError("regression manifest is invalid")
    for record in regressions["fixtures"]:
        if record.get("target") != target:
            continue
        if record.get("license") != "Apache-2.0":
            raise ValueError("regression fixture license is invalid")
        source = contained(root, record["path"])
        payload = source.read_bytes()
        digest = hashlib.sha256(payload).hexdigest()
        if record.get("bytes") != len(payload) or record.get("sha256") != digest:
            raise ValueError(f"regression fixture drifted: {record['path']}")
        shutil.copyfile(source, output / f"regression-{digest}.bin")
    return output


def promote(root: Path, target: str, artifact: Path) -> Path:
    validate_target(target)
    artifact = artifact.resolve()
    if not artifact.is_file() or artifact.is_symlink():
        raise ValueError("artifact must be a regular file")
    payload = artifact.read_bytes()
    if len(payload) > MAX_SEED_BYTES:
        raise ValueError("artifact exceeds the regression fixture ceiling")
    digest = hashlib.sha256(payload).hexdigest()
    relative = Path("fuzz") / "regressions" / target / f"{digest}.bin"
    destination = root / relative
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(payload)
    manifest_path = root / "fuzz" / "regressions" / "manifest.json"
    manifest = load_json(manifest_path)
    if manifest.get("schema_version") != 1 or not isinstance(manifest.get("fixtures"), list):
        raise ValueError("regression manifest is invalid")
    record = {
        "target": target,
        "path": relative.as_posix(),
        "bytes": len(payload),
        "sha256": digest,
        "license": "Apache-2.0",
        "provenance": "repository-generated-by-continuous-fuzz"
    }
    fixtures = [item for item in manifest["fixtures"] if item.get("path") != record["path"]]
    fixtures.append(record)
    manifest["fixtures"] = sorted(fixtures, key=lambda item: (item["target"], item["sha256"]))
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return destination


def minimize(root: Path, target: str, artifact: Path) -> Path:
    validate_target(target)
    artifact = artifact.resolve()
    if not artifact.is_file() or artifact.is_symlink():
        raise ValueError("artifact must be a regular file")
    digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
    minimized = root / "fuzz" / "artifacts" / target / f"minimized-{digest}"
    minimized.parent.mkdir(parents=True, exist_ok=True)
    command = [
        "cargo", "fuzz", "tmin", target, str(artifact), "--",
        f"-exact_artifact_path={minimized}", "-timeout=10", "-rss_limit_mb=2048"
    ]
    subprocess.run(command, cwd=root / "fuzz", check=True)
    return promote(root, target, minimized)


def report(root: Path, target: str, sanitizer: str, status: int, output: Path) -> None:
    validate_target(target)
    artifacts = root / "fuzz" / "artifacts" / target
    records = []
    if artifacts.is_dir():
        for path in sorted(
            item for item in artifacts.iterdir() if item.is_file() and not item.is_symlink()
        ):
            payload = path.read_bytes()
            records.append({"name": path.name, "bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest()})
    value = {
        "schema_version": 1,
        "target": target,
        "sanitizer": sanitizer,
        "platform": sys.platform,
        "runner_os": os.environ.get("RUNNER_OS", "local"),
        "commit": os.environ.get("GITHUB_SHA", "local"),
        "exit_status": status,
        "artifacts": records,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def validate_target(target: str) -> None:
    if target not in TARGETS:
        raise ValueError(f"unknown fuzz target: {target}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root")
    subparsers = parser.add_subparsers(dest="command", required=True)
    prepare_parser = subparsers.add_parser("prepare")
    prepare_parser.add_argument("target", choices=TARGETS)
    prepare_parser.add_argument("--destination", type=Path)
    promote_parser = subparsers.add_parser("promote")
    promote_parser.add_argument("target", choices=TARGETS)
    promote_parser.add_argument("artifact", type=Path)
    minimize_parser = subparsers.add_parser("minimize")
    minimize_parser.add_argument("target", choices=TARGETS)
    minimize_parser.add_argument("artifact", type=Path)
    report_parser = subparsers.add_parser("report")
    report_parser.add_argument("target", choices=TARGETS)
    report_parser.add_argument("--sanitizer", required=True)
    report_parser.add_argument("--status", required=True, type=int)
    report_parser.add_argument("--output", required=True, type=Path)
    arguments = parser.parse_args()
    root = repository_root(arguments.root)
    if arguments.command == "prepare":
        print(prepare(root, arguments.target, arguments.destination))
    elif arguments.command == "promote":
        print(promote(root, arguments.target, arguments.artifact))
    elif arguments.command == "minimize":
        print(minimize(root, arguments.target, arguments.artifact))
    elif arguments.command == "report":
        report(root, arguments.target, arguments.sanitizer, arguments.status, arguments.output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
