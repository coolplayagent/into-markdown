#!/usr/bin/env python3
"""Run one CI command with live output and append its wall time to a TSV file."""

from __future__ import annotations

import argparse
import pathlib
import subprocess
import sys
import time


def run(stage: str, output: pathlib.Path, command: list[str]) -> int:
    if not stage or any(character in stage for character in "\t\r\n"):
        raise ValueError("stage must be a non-empty single-line TSV field")
    if not command:
        raise ValueError("a command is required")
    started = time.perf_counter()
    try:
        return subprocess.run(command, check=False).returncode
    finally:
        elapsed = time.perf_counter() - started
        output.parent.mkdir(parents=True, exist_ok=True)
        with output.open("a", encoding="utf-8", newline="") as handle:
            handle.write(f"{stage}\t{elapsed:.3f}\n")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--stage", required=True)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    arguments = parser.parse_args()
    command = arguments.command
    if command[:1] == ["--"]:
        command = command[1:]
    return run(arguments.stage, arguments.output, command)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as error:
        print(f"timed CI command failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
