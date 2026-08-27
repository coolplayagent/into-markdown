"""Validate the immutable version shared by Cargo and release artifacts."""

from __future__ import annotations

import pathlib
import re
import tomllib


SEMVER = re.compile(
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)"
    r"(?:-(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


class VersionError(ValueError):
    """Raised when release version authority is invalid or inconsistent."""


def workspace_version(root: pathlib.Path) -> str:
    """Return the version declared by the root Cargo workspace."""
    manifest = root / "Cargo.toml"
    try:
        value = tomllib.loads(manifest.read_text(encoding="utf-8"))["workspace"]["package"][
            "version"
        ]
    except (OSError, KeyError, tomllib.TOMLDecodeError) as error:
        raise VersionError("Cargo workspace version authority is unavailable") from error
    if not isinstance(value, str) or SEMVER.fullmatch(value) is None:
        raise VersionError("Cargo workspace version is not valid SemVer")
    return value


def validate_version(requested: str, root: pathlib.Path) -> str:
    """Require a plain SemVer value equal to the Cargo workspace version."""
    if SEMVER.fullmatch(requested) is None:
        raise VersionError("release version must be SemVer without a leading v")
    cargo_version = workspace_version(root)
    if requested != cargo_version:
        raise VersionError(
            f"release version {requested} disagrees with Cargo workspace version {cargo_version}"
        )
    return requested
