#!/usr/bin/env python3
"""Run already-compiled Cargo test harnesses concurrently by package.

Cargo runs test executables one after another.  The PR gate has several independent
packages whose tests spend most of their time in fixture and process isolation
checks, so letting those packages overlap shortens the wall clock without changing
the selected tests or their per-harness thread policy.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import pathlib
import subprocess
import sys
from collections import defaultdict


def compiled_harnesses(paths: list[pathlib.Path]) -> dict[str, list[tuple[str, pathlib.Path]]]:
    packages: dict[str, list[tuple[str, pathlib.Path]]] = defaultdict(list)
    seen: set[pathlib.Path] = set()
    for path in paths:
        for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                raise ValueError(f"{path}:{line_number}: invalid Cargo JSON: {error}") from error
            if record.get("reason") != "compiler-artifact":
                continue
            profile = record.get("profile") or {}
            executable = record.get("executable")
            if not profile.get("test") or not executable:
                continue
            executable_path = pathlib.Path(executable)
            if executable_path in seen:
                continue
            seen.add(executable_path)
            package_id = record.get("package_id")
            target = record.get("target") or {}
            target_name = target.get("name")
            if not isinstance(package_id, str) or not isinstance(target_name, str):
                raise ValueError(f"{path}:{line_number}: test artifact is missing package or target identity")
            packages[package_id].append((target_name, executable_path))
    return dict(packages)


def package_name(package_id: str) -> str:
    fragment = package_id.rsplit("#", 1)[-1]
    return fragment.split("@", 1)[0]


def run_package(
    package_id: str,
    harnesses: list[tuple[str, pathlib.Path]],
    serial_packages: set[str],
) -> None:
    name = package_name(package_id)
    for target_name, executable in sorted(harnesses):
        if not executable.is_file():
            raise RuntimeError(f"compiled test executable is absent: {executable}")
        command = [str(executable)]
        if name in serial_packages:
            command.extend(["--test-threads", "1"])
        print(f"=== {name}/{target_name} ===", flush=True)
        subprocess.run(command, check=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("manifests", nargs="+", type=pathlib.Path)
    parser.add_argument("--expect-target", action="append", default=[])
    parser.add_argument("--serial-package", action="append", default=[])
    arguments = parser.parse_args()

    packages = compiled_harnesses(arguments.manifests)
    actual_targets = {
        target_name for harnesses in packages.values() for target_name, _ in harnesses
    }
    expected_targets = set(arguments.expect_target)
    if actual_targets != expected_targets:
        missing = sorted(expected_targets - actual_targets)
        extra = sorted(actual_targets - expected_targets)
        raise SystemExit(f"compiled test target mismatch: missing={missing}, extra={extra}")

    serial_packages = set(arguments.serial_package)
    actual_packages = {package_name(package_id) for package_id in packages}
    unknown_serial = sorted(serial_packages - actual_packages)
    if unknown_serial:
        raise SystemExit(f"serial package was not compiled: {unknown_serial}")

    with concurrent.futures.ThreadPoolExecutor(max_workers=len(packages)) as executor:
        futures = [
            executor.submit(run_package, package_id, harnesses, serial_packages)
            for package_id, harnesses in sorted(packages.items())
        ]
        for future in futures:
            future.result()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"compiled Rust test runner failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
