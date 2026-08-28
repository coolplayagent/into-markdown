"""Shared fail-closed helpers for the macOS ARM64 release adapter."""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import stat
import sys

_TOOLS_ROOT = pathlib.Path(__file__).resolve().parents[1]
if str(_TOOLS_ROOT) not in sys.path:
    sys.path.insert(0, str(_TOOLS_ROOT))

from release_subprocess import ReleaseError, run  # noqa: E402

TARGET = "aarch64-apple-darwin"
ROOT = pathlib.Path(__file__).resolve().parents[2]
AUTHORITY = pathlib.Path(__file__).with_name("authority.json")


def published_plugin_file(filename: str) -> str:
    path = pathlib.PurePosixPath(filename)
    if path.name != filename or path.suffix != ".imp" or not path.stem:
        raise ReleaseError("plugin package filename is invalid")
    return f"{path.stem}-{TARGET}.imp"


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


def write_json(path: pathlib.Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
