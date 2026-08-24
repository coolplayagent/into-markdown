#!/usr/bin/env python3
"""Command-line entry point for the deterministic agent skill release."""

from __future__ import annotations

import argparse
import pathlib
import sys

from skill_release import SkillReleaseError, create_archive, materialize, validate, verify_release


def main() -> None:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    build = commands.add_parser("build")
    build.add_argument("--archive", required=True, type=pathlib.Path)
    verify = commands.add_parser("verify")
    verify.add_argument("--archive", required=True, type=pathlib.Path)
    copy = commands.add_parser("materialize")
    copy.add_argument("--destination", required=True, type=pathlib.Path)
    commands.add_parser("validate")
    arguments = parser.parse_args()
    if arguments.command == "build":
        create_archive(arguments.archive.resolve())
    elif arguments.command == "verify":
        verify_release(arguments.archive.resolve())
    elif arguments.command == "materialize":
        materialize(arguments.destination.resolve())
    else:
        validate()


if __name__ == "__main__":
    try:
        main()
    except SkillReleaseError as error:
        print(f"skill-release: {error}", file=sys.stderr)
        raise SystemExit(1)
