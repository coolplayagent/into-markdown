#!/usr/bin/env python3
"""Build one deterministic, content-deduplicated release audit archive."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import stat
import tempfile
import zipfile


MAX_FILES = 4096
MAX_TOTAL_BYTES = 2 * 1024 * 1024 * 1024
FIXED_ZIP_TIME = (1980, 1, 1, 0, 0, 0)
SECRET_SUFFIXES = {".key", ".p8", ".p12", ".pem", ".pfx", ".pk8"}
SECRET_NAMES = {"credentials", "secrets", "release-signing.pfx", "plugin-integrity-key.pk8"}


def sha256_bytes(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def normalized_relative(root: pathlib.Path, path: pathlib.Path) -> str:
    relative = path.relative_to(root)
    if any(part in {"", ".", ".."} for part in relative.parts):
        raise RuntimeError(f"unsafe release evidence path: {relative}")
    value = relative.as_posix()
    if value.startswith("/") or "\\" in value or "\0" in value:
        raise RuntimeError(f"unsafe release evidence path: {relative}")
    return value


def reject_secret(relative: str) -> None:
    path = pathlib.PurePosixPath(relative)
    lower = {part.lower() for part in path.parts}
    if (
        path.suffix.lower() in SECRET_SUFFIXES
        or path.name.lower() in SECRET_NAMES
        or lower & {"credentials", "secrets"}
    ):
        raise RuntimeError(f"release evidence contains signing material: {relative}")


def read_source(source: pathlib.Path) -> tuple[dict[str, bytes], dict[str, list[str]]]:
    if not source.is_dir() or source.is_symlink():
        raise RuntimeError(f"release evidence source is missing or unsafe: {source}")
    objects: dict[str, bytes] = {}
    paths: dict[str, list[str]] = {}
    total = 0
    count = 0
    for path in sorted(source.rglob("*"), key=lambda item: item.as_posix()):
        metadata = path.lstat()
        if path.is_symlink():
            raise RuntimeError(f"release evidence source entry is a link: {path}")
        if stat.S_ISDIR(metadata.st_mode):
            continue
        if not stat.S_ISREG(metadata.st_mode):
            raise RuntimeError(f"release evidence source entry is unsafe: {path}")
        relative = normalized_relative(source, path)
        reject_secret(relative)
        content = path.read_bytes()
        total += len(content)
        count += 1
        if count > MAX_FILES or total > MAX_TOTAL_BYTES:
            raise RuntimeError("release evidence source exceeds bounded archive limits")
        digest = sha256_bytes(content)
        previous = objects.get(digest)
        if previous is not None and previous != content:
            raise RuntimeError("release evidence SHA-256 collision")
        objects[digest] = content
        paths.setdefault(digest, []).append(relative)
    if not objects:
        raise RuntimeError("release evidence source is empty")
    return objects, paths


def object_name(digest: str, source_paths: list[str]) -> str:
    names = sorted({pathlib.PurePosixPath(path).name for path in source_paths})
    leaf = names[0] if len(names) == 1 else "content.bin"
    return f"objects/{digest}/{leaf}"


def write_bundle(
    objects: dict[str, bytes],
    paths: dict[str, list[str]],
    output: pathlib.Path,
    source_revision: str,
) -> str:
    if output.exists() or output.is_symlink():
        raise RuntimeError(f"release evidence output already exists: {output}")
    if not source_revision or any(
        character not in "0123456789abcdef" for character in source_revision.lower()
    ):
        raise RuntimeError("source revision must be a hexadecimal commit identity")
    manifest_objects = []
    archive_entries: dict[str, bytes] = {}
    for digest in sorted(objects):
        stored = object_name(digest, paths[digest])
        archive_entries[stored] = objects[digest]
        manifest_objects.append(
            {
                "bytes": len(objects[digest]),
                "sha256": digest,
                "sourcePaths": sorted(paths[digest]),
                "storedPath": stored,
            }
        )
    manifest = {
        "schemaVersion": 1,
        "sourceRevision": source_revision,
        "objects": manifest_objects,
    }
    archive_entries["manifest.json"] = (
        json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    ).encode("utf-8")
    output.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=output.parent, prefix=f".{output.name}.", suffix=".tmp"
    )
    os.close(descriptor)
    temporary = pathlib.Path(temporary_name)
    try:
        with zipfile.ZipFile(
            temporary, mode="w", compression=zipfile.ZIP_DEFLATED, compresslevel=9
        ) as bundle:
            for name in sorted(archive_entries):
                info = zipfile.ZipInfo(name, FIXED_ZIP_TIME)
                info.compress_type = zipfile.ZIP_DEFLATED
                info.create_system = 3
                info.external_attr = 0o100644 << 16
                bundle.writestr(info, archive_entries[name])
        os.replace(temporary, output)
    finally:
        temporary.unlink(missing_ok=True)
    return hashlib.sha256(output.read_bytes()).hexdigest()


def build_bundle(
    source: pathlib.Path,
    output: pathlib.Path,
    source_revision: str,
    existing: pathlib.Path | None = None,
) -> str:
    if existing is not None:
        raise RuntimeError("incremental evidence merging is unsupported; aggregate all targets once")
    objects, paths = read_source(source)
    return write_bundle(objects, paths, output, source_revision)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--source-revision", required=True)
    arguments = parser.parse_args()
    build_bundle(arguments.source.resolve(), arguments.output.resolve(), arguments.source_revision)


if __name__ == "__main__":
    main()
