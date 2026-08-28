#!/usr/bin/env python3
"""Build one deterministic, revision-bound release evidence archive."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import tempfile
import zipfile


MAX_FILES = 512
MAX_TOTAL_BYTES = 512 * 1024 * 1024
FIXED_ZIP_TIME = (1980, 1, 1, 0, 0, 0)
EVIDENCE_NAME = re.compile(
    r"(?:"
    r"into-md-(?:linux-x86_64|linux-arm64|windows-x86_64|macos-arm64)-core\.(?:tar\.gz|zip|dmg)\.sha256|"
    r"official\.media\.whisper-(?:x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-pc-windows-msvc|aarch64-apple-darwin)\.imp\.sha256|"
    r".+\.asc|"
    r".+\.(?:sources|spdx)\.json|"
    r".+\.THIRD_PARTY_NOTICES\.md|"
    r".+-(?:platform-audit|platform-acceptance|installed-smoke|signing-policy)\.json|"
    r"into-markdown-.+-release-set\.json"
    r")"
)
RELEASE_SET_NAME = re.compile(r"into-markdown-.+-release-set\.json")


def sha256_bytes(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def validate_name(name: str) -> None:
    if pathlib.PurePosixPath(name).name != name or not EVIDENCE_NAME.fullmatch(name):
        raise RuntimeError(f"unexpected release evidence name: {name}")


def validate_revision(entries: dict[str, bytes], source_revision: str) -> bool:
    revisions: set[str] = set()
    for name, content in entries.items():
        if not RELEASE_SET_NAME.fullmatch(name):
            continue
        try:
            document = json.loads(content)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RuntimeError(f"invalid release-set JSON: {name}") from error
        revision = document.get("source_revision")
        legacy_revision = document.get("sourceRevision")
        if legacy_revision is not None:
            if revision is not None and revision != legacy_revision:
                raise RuntimeError(f"release-set source revision fields disagree: {name}")
            revision = legacy_revision
        if not isinstance(revision, str) or not revision:
            raise RuntimeError(f"release-set source revision is missing: {name}")
        revisions.add(revision)
    if not revisions:
        raise RuntimeError("release evidence has no release-set authority")
    if len(revisions) != 1:
        raise RuntimeError(f"release evidence mixes source revisions: {sorted(revisions)}")
    return revisions == {source_revision}


def read_source(source: pathlib.Path) -> dict[str, bytes]:
    if not source.is_dir() or source.is_symlink():
        raise RuntimeError(f"release evidence source is missing or unsafe: {source}")
    entries: dict[str, bytes] = {}
    total = 0
    for path in sorted(source.iterdir()):
        if not path.is_file() or path.is_symlink():
            raise RuntimeError(f"release evidence source entry is unsafe: {path}")
        validate_name(path.name)
        content = path.read_bytes()
        total += len(content)
        if len(entries) >= MAX_FILES or total > MAX_TOTAL_BYTES:
            raise RuntimeError("release evidence source exceeds bounded archive limits")
        entries[path.name] = content
    if not entries:
        raise RuntimeError("release evidence source is empty")
    return entries


def read_existing(archive: pathlib.Path) -> dict[str, bytes]:
    if not archive.is_file() or archive.is_symlink():
        raise RuntimeError(f"existing release evidence archive is missing or unsafe: {archive}")
    entries: dict[str, bytes] = {}
    total = 0
    try:
        with zipfile.ZipFile(archive) as bundle:
            for info in bundle.infolist():
                if info.is_dir() or info.flag_bits & 0x1:
                    raise RuntimeError(f"unsafe release evidence ZIP entry: {info.filename}")
                validate_name(info.filename)
                if info.filename in entries:
                    raise RuntimeError(f"duplicate release evidence ZIP entry: {info.filename}")
                total += info.file_size
                if len(entries) >= MAX_FILES or total > MAX_TOTAL_BYTES:
                    raise RuntimeError("existing release evidence exceeds bounded archive limits")
                entries[info.filename] = bundle.read(info)
    except zipfile.BadZipFile as error:
        raise RuntimeError(f"existing release evidence ZIP is invalid: {archive}") from error
    return entries


def merge_entries(
    source: dict[str, bytes], existing: dict[str, bytes], source_revision: str
) -> dict[str, bytes]:
    if not validate_revision(source, source_revision):
        raise RuntimeError("new release evidence does not match the requested source revision")
    if existing and not validate_revision(existing, source_revision):
        existing = {}
    merged = dict(existing)
    for name, content in source.items():
        previous = merged.get(name)
        if previous is not None and previous != content:
            raise RuntimeError(f"release evidence bytes disagree for {name}")
        merged[name] = content
    validate_revision(merged, source_revision)
    return merged


def write_bundle(entries: dict[str, bytes], output: pathlib.Path) -> str:
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
            for name in sorted(entries):
                info = zipfile.ZipInfo(name, FIXED_ZIP_TIME)
                info.compress_type = zipfile.ZIP_DEFLATED
                info.create_system = 3
                info.external_attr = 0o100644 << 16
                bundle.writestr(info, entries[name])
        os.replace(temporary, output)
    finally:
        temporary.unlink(missing_ok=True)
    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    pathlib.Path(f"{output}.sha256").write_text(
        f"{digest}  {output.name}\n", encoding="ascii"
    )
    return digest


def build_bundle(
    source: pathlib.Path,
    output: pathlib.Path,
    source_revision: str,
    existing: pathlib.Path | None = None,
) -> str:
    source_entries = read_source(source)
    existing_entries = read_existing(existing) if existing is not None else {}
    entries = merge_entries(source_entries, existing_entries, source_revision)
    return write_bundle(entries, output)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--existing", type=pathlib.Path)
    arguments = parser.parse_args()
    build_bundle(
        arguments.source.resolve(),
        arguments.output.resolve(),
        arguments.source_revision,
        arguments.existing.resolve() if arguments.existing is not None else None,
    )


if __name__ == "__main__":
    main()
