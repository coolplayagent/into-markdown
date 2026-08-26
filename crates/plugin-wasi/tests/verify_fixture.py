#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Verify or reproducibly rebuild the checked-in WASIp2 fixture."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import subprocess


ROOT = pathlib.Path(__file__).resolve().parents[1]
AUTHORITY = ROOT / "tests" / "fixtures" / "authority.json"


def digest(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_digest(path: pathlib.Path) -> str:
    raw = path.read_bytes()
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise SystemExit(f"fixture source is not strict UTF-8: {path.name}: {error}") from error
    if "\r" in text.replace("\r\n", ""):
        raise SystemExit(f"fixture source contains an isolated CR: {path.name}")
    return hashlib.sha256(text.replace("\r\n", "\n").encode("utf-8")).hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rebuild", action="store_true")
    args = parser.parse_args()
    authority = json.loads(AUTHORITY.read_text(encoding="utf-8"))
    build = authority["build"]
    rustc = subprocess.run(
        ["rustc", f"+{authority['rustToolchain']['version']}", "-vV"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    expected_release = f"release: {authority['rustToolchain']['version']}"
    expected_commit = f"commit-hash: {authority['rustToolchain']['commit']}"
    expected_date = f"commit-date: {authority['rustToolchain']['commitDate']}"
    for field in (expected_release, expected_commit, expected_date):
        if field not in rustc.splitlines():
            raise SystemExit(f"fixture Rust toolchain authority mismatch: {field}")
    for relative, expected in build["sourceFiles"].items():
        actual = source_digest(ROOT / relative)
        if actual != expected:
            raise SystemExit(f"fixture source digest mismatch: {relative}: {actual}")
    if args.rebuild:
        subprocess.run(
            [
                "cargo",
                "+1.97.1",
                "build",
                "--manifest-path",
                str(ROOT / "guest-fixture" / "Cargo.toml"),
                "--target",
                authority["guestTarget"],
                "--release",
                "--locked",
                "-j1",
            ],
            check=True,
        )
        rebuilt = (
            ROOT
            / "guest-fixture"
            / "target"
            / authority["guestTarget"]
            / "release"
            / "into-markdown-wasi-test-guest.wasm"
        )
        rebuilt_digest = digest(rebuilt)
        host_family = "windows" if os.name == "nt" else "unix"
        expected_rebuild_digest = build["rebuildSha256ByHostFamily"][host_family]
        if rebuilt_digest != expected_rebuild_digest:
            raise SystemExit(f"rebuilt component digest mismatch: {rebuilt_digest}")
    component = ROOT / build["component"]["path"]
    if component.stat().st_size != build["component"]["bytes"]:
        raise SystemExit("checked-in component size mismatch")
    if digest(component) != build["component"]["sha256"]:
        raise SystemExit("checked-in component digest mismatch")
    print(f"fixture authority PASS: {build['component']['sha256']}")


if __name__ == "__main__":
    main()
