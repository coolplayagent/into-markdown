"""Fail-closed schema and local-path validation for release assets."""

from __future__ import annotations

import hashlib
import pathlib
import re
import stat
import urllib.parse
from collections.abc import Mapping


_LOCAL_MODEL_PREFIX = ("third_party", "runtime-assets", "models")
_SHA256 = re.compile(r"[0-9a-f]{64}")
_WINDOWS_RESERVED_NAMES = frozenset(
    {"CON", "PRN", "AUX", "NUL"}
    | {f"COM{index}" for index in range(1, 10)}
    | {f"LPT{index}" for index in range(1, 10)}
)


class ReleaseAssetError(ValueError):
    """A release asset authority entry is malformed or unsafe."""


def validate_acquisition_item(
    identity: str,
    item: Mapping[str, object],
    *,
    repository_root: pathlib.Path,
) -> pathlib.Path | None:
    """Validate one authority entry and return its verified local source, if any."""
    if (
        not isinstance(identity, str)
        or not identity
        or pathlib.PurePosixPath(identity).name != identity
        or "\\" in identity
        or ":" in identity
        or _is_windows_ambiguous_name(identity)
    ):
        raise ReleaseAssetError("release asset identity must be a simple file name")

    has_path = "path" in item
    has_url = "url" in item
    if has_path == has_url:
        raise ReleaseAssetError(
            f"{identity} authority must contain exactly one of path or url"
        )

    digest = item.get("sha256")
    if not isinstance(digest, str) or _SHA256.fullmatch(digest) is None:
        raise ReleaseAssetError(f"{identity} authority has an invalid SHA-256")

    if has_url:
        _validate_https_url(identity, "url", item.get("url"))
        source_url = item.get("source_url")
        if source_url is not None:
            _validate_https_url(identity, "source_url", source_url)
        mirrors = item.get("mirror_urls", [])
        if not isinstance(mirrors, list):
            raise ReleaseAssetError(f"{identity} mirror_urls must be a list")
        for mirror in mirrors:
            _validate_https_url(identity, "mirror URL", mirror)
        return None

    if "mirror_urls" in item:
        raise ReleaseAssetError(f"{identity} local authority cannot contain mirror_urls")
    _validate_https_url(identity, "source_url", item.get("source_url"))
    expected_bytes = item.get("bytes")
    if type(expected_bytes) is not int or expected_bytes <= 0:
        raise ReleaseAssetError(f"{identity} local authority has invalid bytes")

    local_path = item.get("path")
    if not isinstance(local_path, str) or not local_path:
        raise ReleaseAssetError(f"{identity} local asset path is invalid")
    relative = pathlib.PurePosixPath(local_path)
    if (
        "\\" in local_path
        or ":" in local_path
        or any(_is_windows_ambiguous_name(part) for part in relative.parts)
    ):
        raise ReleaseAssetError(
            f"{identity} local asset path must use a portable POSIX spelling"
        )
    if (
        relative.is_absolute()
        or relative.as_posix() != local_path
        or ".." in relative.parts
        or len(relative.parts) <= len(_LOCAL_MODEL_PREFIX)
        or relative.parts[: len(_LOCAL_MODEL_PREFIX)] != _LOCAL_MODEL_PREFIX
    ):
        raise ReleaseAssetError(
            f"{identity} local asset path must be inside third_party/runtime-assets/models"
        )

    root = repository_root.resolve()
    model_root = root.joinpath(*_LOCAL_MODEL_PREFIX)
    candidate = root.joinpath(*relative.parts)
    try:
        candidate.resolve().relative_to(model_root.resolve())
    except (OSError, ValueError) as error:
        raise ReleaseAssetError(
            f"{identity} local asset is outside the runtime models directory"
        ) from error

    current = candidate
    while current != root:
        if _is_link_or_reparse(current):
            raise ReleaseAssetError(f"{identity} local asset path contains a link")
        current = current.parent
    if not candidate.is_file():
        raise ReleaseAssetError(f"{identity} local asset is missing")
    if candidate.stat().st_size != expected_bytes or _sha256(candidate) != digest:
        raise ReleaseAssetError(f"{identity} local asset differs from authority")
    return candidate


def _validate_https_url(identity: str, field: str, value: object) -> None:
    if not isinstance(value, str):
        raise ReleaseAssetError(f"{identity} authority has an invalid {field}")
    try:
        parsed = urllib.parse.urlparse(value)
        hostname = parsed.hostname
    except ValueError as error:
        raise ReleaseAssetError(
            f"{identity} authority has an invalid {field}"
        ) from error
    if (
        parsed.scheme != "https"
        or not hostname
        or parsed.username is not None
        or parsed.password is not None
    ):
        raise ReleaseAssetError(f"{identity} authority has an invalid {field}")


def _is_windows_ambiguous_name(value: str) -> bool:
    if value in {".", ".."} or value != value.rstrip(" ."):
        return True
    return value.split(".", 1)[0].upper() in _WINDOWS_RESERVED_NAMES


def _is_link_or_reparse(path: pathlib.Path) -> bool:
    if path.is_symlink():
        return True
    is_junction = getattr(path, "is_junction", None)
    if is_junction is not None and is_junction():
        return True
    try:
        attributes = path.lstat().st_file_attributes
    except (AttributeError, FileNotFoundError):
        return False
    return bool(attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT)


def _sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()
