"""Exact-size, exact-hash downloads for native platform release inputs."""

from __future__ import annotations

import os
import pathlib
import shutil
import sys
import urllib.parse
import urllib.error
import urllib.request

from common import ReleaseError, sha256

_TOOLS_ROOT = pathlib.Path(__file__).resolve().parents[1]
if str(_TOOLS_ROOT) not in sys.path:
    sys.path.insert(0, str(_TOOLS_ROOT))

from release_asset import ReleaseAssetError, validate_acquisition_item  # noqa: E402

ROOT = pathlib.Path(__file__).resolve().parents[2]

ALLOWED_HOSTS = {
    "cdn-lfs-us-1.hf.co",
    "ffmpeg.org",
    "github.com",
    "huggingface.co",
    "mirror.aarnet.edu.au",
    "objects.githubusercontent.com",
    "paddle-model-ecology.bj.bcebos.com",
    "raw.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "us.aws.cdn.hf.co",
}
DOWNLOAD_ATTEMPTS = 4


class IncompleteDownload(Exception):
    """A trusted endpoint closed before the authority-declared byte count."""


def acquire(cache: pathlib.Path, downloads: dict[str, dict]) -> None:
    cache.mkdir(parents=True, exist_ok=True)
    for identity, item in sorted(downloads.items()):
        try:
            local_asset = validate_acquisition_item(
                identity, item, repository_root=ROOT
            )
        except ReleaseAssetError as error:
            raise ReleaseError(str(error)) from error
        destination = cache / identity
        if destination.is_file() and valid(destination, item):
            continue
        temporary = destination.with_suffix(".download")
        temporary.unlink(missing_ok=True)
        if local_asset is not None:
            copy_local(local_asset, temporary)
            if not valid(temporary, item):
                temporary.unlink(missing_ok=True)
                raise ReleaseError(f"{identity} local asset differs from authority")
            os.replace(temporary, destination)
            continue
        urls = [item["url"], *item.get("mirror_urls", [])]
        for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
            try:
                download_once(identity, item, temporary, urls[(attempt - 1) % len(urls)])
            except (IncompleteDownload, TimeoutError, urllib.error.URLError, OSError) as error:
                if attempt == DOWNLOAD_ATTEMPTS:
                    temporary.unlink(missing_ok=True)
                    raise ReleaseError(
                        f"{identity} download failed after {DOWNLOAD_ATTEMPTS} attempts"
                    ) from error
                continue
            if not valid(temporary, item):
                temporary.unlink(missing_ok=True)
                raise ReleaseError(f"{identity} download differs from authority")
            os.replace(temporary, destination)
            break


def copy_local(source: pathlib.Path, temporary: pathlib.Path) -> None:
    shutil.copyfile(source, temporary)


def download_once(
    identity: str, item: dict, temporary: pathlib.Path, url: str
) -> None:
    expected = item.get("bytes")
    offset = temporary.stat().st_size if temporary.exists() else 0
    headers = {"User-Agent": "into-markdown-release/1"}
    if offset:
        headers["Range"] = f"bytes={offset}-"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=180) as response:
        parsed = urllib.parse.urlparse(response.geturl())
        if parsed.scheme != "https" or parsed.hostname not in ALLOWED_HOSTS:
            raise ReleaseError(f"{identity} redirected outside the release allowlist")
        status = getattr(response, "status", 200)
        if offset and status == 206:
            content_range = response.headers.get("Content-Range", "")
            expected_range = f"bytes {offset}-"
            expected_total = f"/{expected}" if expected is not None else "/"
            if not content_range.startswith(expected_range) or expected_total not in content_range:
                temporary.unlink(missing_ok=True)
                raise ReleaseError(f"{identity} returned an invalid content range")
            mode = "ab"
            total = offset
        else:
            mode = "wb"
            total = 0
        with temporary.open(mode) as output:
            while chunk := response.read(1024 * 1024):
                total += len(chunk)
                if expected is not None and total > expected:
                    raise ReleaseError(f"{identity} download exceeds authority")
                output.write(chunk)
    if expected is not None and total != expected:
        raise IncompleteDownload(f"{identity} downloaded {total} of {expected} bytes")


def valid(path: pathlib.Path, item: dict) -> bool:
    expected = item.get("bytes")
    return (
        (expected is None or path.stat().st_size == expected)
        and sha256(path) == item["sha256"]
    )
