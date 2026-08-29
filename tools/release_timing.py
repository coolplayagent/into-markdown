#!/usr/bin/env python3
"""Record visible, machine-readable release phase durations."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess
import sys
import time


class TimingError(RuntimeError):
    """A release timing operation is invalid."""


def load(path: pathlib.Path, target: str) -> dict:
    if not path.exists():
        return {"schemaVersion": 1, "target": target, "phases": {}}
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise TimingError("release timing report is invalid") from error
    if (
        value.get("schemaVersion") != 1
        or value.get("target") != target
        or not isinstance(value.get("phases"), dict)
    ):
        raise TimingError("release timing report authority is invalid")
    return value


def record(path: pathlib.Path, target: str, phase: str, duration_ms: int) -> None:
    if (
        not phase
        or any(character not in "abcdefghijklmnopqrstuvwxyz0123456789-" for character in phase)
        or duration_ms < 0
    ):
        raise TimingError("release timing record is invalid")
    value = load(path, target)
    if phase in value["phases"]:
        raise TimingError(f"release timing phase was already recorded: {phase}")
    value["phases"][phase] = {"durationMs": duration_ms}
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )
    os.replace(temporary, path)
    print(f"::notice title=Release timing::{target} {phase}: {duration_ms} ms", flush=True)


def mark(path: pathlib.Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"{time.time_ns()}\n", encoding="ascii", newline="\n")


def finish(path: pathlib.Path, target: str, phase: str, marker: pathlib.Path) -> None:
    try:
        started_ns = int(marker.read_text(encoding="ascii").strip())
    except (OSError, ValueError) as error:
        raise TimingError("release timing marker is invalid") from error
    elapsed = max(0, (time.time_ns() - started_ns) // 1_000_000)
    record(path, target, phase, elapsed)
    marker.unlink(missing_ok=True)


def run(path: pathlib.Path, target: str, phase: str, command: list[str]) -> None:
    if not command:
        raise TimingError("timed command is absent")
    started = time.monotonic_ns()
    try:
        subprocess.run(command, check=True)
    finally:
        record(path, target, phase, (time.monotonic_ns() - started) // 1_000_000)


def summary(path: pathlib.Path, target: str, output: pathlib.Path) -> None:
    value = load(path, target)
    lines = [f"### Release timings — `{target}`", "", "| Phase | Duration |", "|---|---:|"]
    for phase, entry in value["phases"].items():
        lines.append(f"| `{phase}` | {entry['durationMs'] / 1000:.3f} s |")
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("a", encoding="utf-8", newline="\n") as stream:
        stream.write("\n".join(lines) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    for command in ("record", "finish", "run"):
        child = subparsers.add_parser(command)
        child.add_argument("--report", required=True, type=pathlib.Path)
        child.add_argument("--target", required=True)
        child.add_argument("--phase", required=True)
        if command == "record":
            child.add_argument("--duration-ms", required=True, type=int)
        elif command == "finish":
            child.add_argument("--marker", required=True, type=pathlib.Path)
        else:
            child.add_argument("remainder", nargs=argparse.REMAINDER)
    marker_parser = subparsers.add_parser("mark")
    marker_parser.add_argument("--marker", required=True, type=pathlib.Path)
    summary_parser = subparsers.add_parser("summary")
    summary_parser.add_argument("--report", required=True, type=pathlib.Path)
    summary_parser.add_argument("--target", required=True)
    summary_parser.add_argument("--output", required=True, type=pathlib.Path)
    arguments = parser.parse_args()
    if arguments.command == "mark":
        mark(arguments.marker)
    elif arguments.command == "record":
        record(arguments.report, arguments.target, arguments.phase, arguments.duration_ms)
    elif arguments.command == "finish":
        finish(arguments.report, arguments.target, arguments.phase, arguments.marker)
    elif arguments.command == "summary":
        summary(arguments.report, arguments.target, arguments.output)
    else:
        command = arguments.remainder
        if command and command[0] == "--":
            command = command[1:]
        run(arguments.report, arguments.target, arguments.phase, command)


if __name__ == "__main__":
    main()
