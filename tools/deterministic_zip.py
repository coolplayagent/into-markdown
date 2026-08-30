"""Canonical ZIP writer for release materials shared by every platform adapter."""

from __future__ import annotations

import pathlib
import zipfile


def create_deterministic_zip(
    source: pathlib.Path,
    destination: pathlib.Path,
    files: list[pathlib.Path],
) -> None:
    """Write a reproducible compressed release archive."""
    _create_deterministic_zip(
        source,
        destination,
        files,
        compression=zipfile.ZIP_DEFLATED,
        compresslevel=9,
    )


def create_deterministic_stored_zip(
    source: pathlib.Path,
    destination: pathlib.Path,
    files: list[pathlib.Path],
) -> None:
    """Write a source archive whose digest does not depend on host zlib."""
    _create_deterministic_zip(
        source,
        destination,
        files,
        compression=zipfile.ZIP_STORED,
        compresslevel=None,
    )


def _create_deterministic_zip(
    source: pathlib.Path,
    destination: pathlib.Path,
    files: list[pathlib.Path],
    *,
    compression: int,
    compresslevel: int | None,
) -> None:
    with zipfile.ZipFile(
        destination,
        "w",
        compression=compression,
        compresslevel=compresslevel,
    ) as output:
        for path in files:
            relative = path.relative_to(source).as_posix()
            info = zipfile.ZipInfo(relative, (2026, 1, 1, 0, 0, 0))
            info.create_system = 0
            info.external_attr = 0o100644 << 16
            output.writestr(
                info,
                path.read_bytes(),
                compress_type=compression,
                compresslevel=compresslevel,
            )
