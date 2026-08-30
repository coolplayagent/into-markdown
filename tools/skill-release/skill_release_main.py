#!/usr/bin/env python3
"""Command-line entry point for the deterministic agent skill release."""

from __future__ import annotations

import argparse
import pathlib
import sys

from skill_release import SkillReleaseError, core_inputs, create_archive, materialize, validate, verify_release


def main() -> None:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    build = commands.add_parser("build")
    build.add_argument("--archive", required=True, type=pathlib.Path)
    build.add_argument("--windows-x86-64-core", required=True, type=pathlib.Path)
    build.add_argument("--windows-x86-64-pdfium", required=True, type=pathlib.Path)
    build.add_argument("--linux-x86-64-core", required=True, type=pathlib.Path)
    build.add_argument("--linux-arm64-core", required=True, type=pathlib.Path)
    build.add_argument("--material-authority", required=True, type=pathlib.Path)
    verify = commands.add_parser("verify")
    verify.add_argument("--archive", required=True, type=pathlib.Path)
    verify.add_argument("--material-authority", required=True, type=pathlib.Path)
    copy = commands.add_parser("materialize")
    copy.add_argument("--destination", required=True, type=pathlib.Path)
    commands.add_parser("validate")
    arguments = parser.parse_args()
    if arguments.command == "build":
        create_archive(
            arguments.archive.resolve(),
            core_inputs(
                arguments.windows_x86_64_core.resolve(),
                arguments.windows_x86_64_pdfium.resolve(),
                arguments.linux_x86_64_core.resolve(),
                arguments.linux_arm64_core.resolve(),
            ),
            arguments.material_authority.resolve(),
        )
    elif arguments.command == "verify":
        verify_release(
            arguments.archive.resolve(), arguments.material_authority.resolve()
        )
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
