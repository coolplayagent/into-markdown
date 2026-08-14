"""Shared fail-closed helpers for the macOS ARM64 release adapter."""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import stat
import subprocess
from typing import Iterable

TARGET = "aarch64-apple-darwin"
ROOT = pathlib.Path(__file__).resolve().parents[2]
AUTHORITY = pathlib.Path(__file__).with_name("authority.json")


class ReleaseError(RuntimeError):
    """Stable packaging failure."""


def authority() -> dict:
    value = json.loads(AUTHORITY.read_text(encoding="utf-8"))
    if value.get("schemaVersion") != 1 or value.get("target") != TARGET:
        raise ReleaseError("macOS release authority schema or target is invalid")
    return value


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def regular_files(root: pathlib.Path) -> list[pathlib.Path]:
    result: list[pathlib.Path] = []
    for directory, directories, files in os.walk(root, followlinks=False):
        base = pathlib.Path(directory)
        for name in sorted(directories + files):
            path = base / name
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode):
                raise ReleaseError(f"symbolic link is forbidden: {path.relative_to(root)}")
            if not (stat.S_ISDIR(mode) or stat.S_ISREG(mode)):
                raise ReleaseError(f"non-regular archive entry is forbidden: {path.relative_to(root)}")
        result.extend(base / name for name in sorted(files))
    return sorted(result, key=lambda path: path.relative_to(root).as_posix())


def run(arguments: Iterable[str], *, cwd: pathlib.Path | None = None, env: dict | None = None) -> str:
    command = list(arguments)
    completed = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        errors="replace",
    )
    if completed.returncode:
        detail = completed.stderr.strip().splitlines()[-1:] or ["no diagnostic"]
        raise ReleaseError(f"command failed ({command[0]}): {detail[0]}")
    return completed.stdout


def write_json(path: pathlib.Path, value: object) -> None:
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
