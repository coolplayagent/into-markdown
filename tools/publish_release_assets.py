#!/usr/bin/env python3
"""Materialize flat, target-qualified plugin assets from verified local packages."""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import re
import shutil


PLUGIN_IDS = ("official.ocr.ppocrv6", "official.media.whisper")
TARGET = re.compile(r"[a-z0-9_]+(?:-[a-z0-9_]+)+")


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def materialize(source: pathlib.Path, output: pathlib.Path, target: str) -> None:
    if not TARGET.fullmatch(target):
        raise RuntimeError("release target is not a bounded target triple")
    if output.exists():
        raise RuntimeError("published plugin output already exists")
    output.mkdir(parents=True)
    for plugin_id in PLUGIN_IDS:
        local = source / f"{plugin_id}.imp"
        if not local.is_file() or local.is_symlink():
            raise RuntimeError(f"verified local plugin is missing or unsafe: {local.name}")
        destination = output / f"{plugin_id}-{target}.imp"
        shutil.copyfile(local, destination)
        (output / f"{destination.name}.sha256").write_text(
            f"{sha256(destination)}  {destination.name}\n", encoding="ascii"
        )
        signature = local.with_name(f"{local.name}.asc")
        if signature.exists():
            if not signature.is_file() or signature.is_symlink():
                raise RuntimeError(f"plugin detached signature is unsafe: {signature.name}")
            shutil.copyfile(signature, output / f"{destination.name}.asc")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--target", required=True)
    arguments = parser.parse_args()
    materialize(arguments.source.resolve(), arguments.output.resolve(), arguments.target)


if __name__ == "__main__":
    main()
