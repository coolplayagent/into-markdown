"""Controlled downloader for explicit fixture inputs.

The ordinary build and test graph never calls this module. It exists for a
human-invoked, hash-pinned acquisition step and deliberately rejects redirects.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
from urllib import error, parse, request

MAX_ARTIFACT_BYTES = 64 * 1024 * 1024
_TOP_KEYS = {"schema_version", "artifacts"}
_ARTIFACT_KEYS = {
    "artifact_id",
    "repository",
    "downloaded_file_path",
    "url",
    "allowed_hosts",
    "sha256",
    "size",
    "maximum_redirects",
    "license",
    "manual_only",
    "included_in_release",
}
_ID = re.compile(r"[a-z0-9][a-z0-9_-]*\Z")
_FILE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*\Z")
_SHA256 = re.compile(r"[0-9a-f]{64}\Z")


class FixtureDownloadError(RuntimeError):
    """The download authority or response violated the fixture policy."""


class _NoRedirect(request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001
        return None


def load_artifact(manifest_path: Path, artifact_id: str) -> dict[str, object]:
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise FixtureDownloadError(f"cannot read download authority: {exc}") from exc
    if not isinstance(manifest, dict) or set(manifest) != _TOP_KEYS:
        raise FixtureDownloadError("download authority has unknown or missing top-level fields")
    if manifest["schema_version"] != 1 or not isinstance(manifest["artifacts"], list):
        raise FixtureDownloadError("unsupported download authority")
    matches: list[dict[str, object]] = []
    seen: set[str] = set()
    for item in manifest["artifacts"]:
        if not isinstance(item, dict) or set(item) != _ARTIFACT_KEYS:
            raise FixtureDownloadError("artifact has unknown or missing fields")
        _validate_artifact(item)
        item_id = str(item["artifact_id"])
        if item_id in seen:
            raise FixtureDownloadError("artifact IDs must be unique")
        seen.add(item_id)
        if item_id == artifact_id:
            matches.append(item)
    if len(matches) != 1:
        raise FixtureDownloadError("artifact ID must resolve exactly once")
    artifact = matches[0]
    return artifact


def _validate_artifact(artifact: dict[str, object], *, allow_http_loopback: bool = False) -> None:
    artifact_id = artifact["artifact_id"]
    repository = artifact["repository"]
    filename = artifact["downloaded_file_path"]
    url = artifact["url"]
    hosts = artifact["allowed_hosts"]
    size = artifact["size"]
    if not isinstance(artifact_id, str) or _ID.fullmatch(artifact_id) is None:
        raise FixtureDownloadError("unsafe artifact ID")
    if not isinstance(repository, str) or _ID.fullmatch(repository) is None:
        raise FixtureDownloadError("unsafe repository name")
    if not isinstance(filename, str) or _FILE.fullmatch(filename) is None:
        raise FixtureDownloadError("downloaded_file_path must be one portable ASCII file name")
    if not isinstance(url, str):
        raise FixtureDownloadError("URL must be text")
    parsed = parse.urlsplit(url)
    loopback = parsed.hostname in {"127.0.0.1", "::1", "localhost"}
    if parsed.username or parsed.password or parsed.fragment or not parsed.hostname:
        raise FixtureDownloadError("URL contains forbidden authority fields")
    if parsed.scheme != "https" and not (allow_http_loopback and parsed.scheme == "http" and loopback):
        raise FixtureDownloadError("fixture URL must use HTTPS")
    if hosts != [parsed.hostname]:
        raise FixtureDownloadError("URL host disagrees with the single-host allowlist")
    if not isinstance(size, int) or isinstance(size, bool) or not 0 < size <= MAX_ARTIFACT_BYTES:
        raise FixtureDownloadError("invalid artifact size ceiling")
    if not isinstance(artifact["sha256"], str) or _SHA256.fullmatch(artifact["sha256"]) is None:
        raise FixtureDownloadError("invalid SHA-256")
    if artifact["maximum_redirects"] != 0:
        raise FixtureDownloadError("fixture downloads must reject every redirect")
    if not isinstance(artifact["license"], str) or not artifact["license"]:
        raise FixtureDownloadError("fixture license is required")
    if artifact["manual_only"] is not True or artifact["included_in_release"] is not False:
        raise FixtureDownloadError("fixture artifact must be manual-only and excluded from release")


def download_artifact(
    artifact: dict[str, object],
    output_directory: Path,
    *,
    allow_http_loopback: bool = False,
) -> Path:
    _validate_artifact(artifact, allow_http_loopback=allow_http_loopback)
    output_directory.mkdir(parents=True, exist_ok=True)
    if output_directory.is_symlink() or not output_directory.is_dir():
        raise FixtureDownloadError("output directory must be a real directory")
    repository_directory = output_directory / str(artifact["repository"])
    repository_directory.mkdir(exist_ok=True)
    if repository_directory.is_symlink() or not repository_directory.is_dir():
        raise FixtureDownloadError("repository output must be a real directory")
    target = repository_directory / str(artifact["downloaded_file_path"])
    if target.exists() and (target.is_symlink() or not target.is_file()):
        raise FixtureDownloadError("output target is not a regular file")
    expected_size = int(artifact["size"])
    digest = hashlib.sha256()
    opener = request.build_opener(_NoRedirect())
    req = request.Request(str(artifact["url"]), headers={"User-Agent": "into-markdown-fixture-authority/1"})
    temporary: Path | None = None
    try:
        with opener.open(req, timeout=30) as response:
            if response.status != 200:
                raise FixtureDownloadError(f"unexpected HTTP status {response.status}")
            length = response.headers.get("Content-Length")
            if length is not None:
                try:
                    declared = int(length)
                except ValueError as exc:
                    raise FixtureDownloadError("invalid Content-Length") from exc
                if declared > expected_size:
                    raise FixtureDownloadError("response exceeds the authorized size ceiling")
            with tempfile.NamedTemporaryFile(
                dir=repository_directory, prefix=".fixture-", delete=False
            ) as stream:
                temporary = Path(stream.name)
                received = 0
                while True:
                    chunk = response.read(min(64 * 1024, expected_size - received + 1))
                    if not chunk:
                        break
                    received += len(chunk)
                    if received > expected_size:
                        raise FixtureDownloadError("response exceeds the authorized size ceiling")
                    digest.update(chunk)
                    stream.write(chunk)
                stream.flush()
                os.fsync(stream.fileno())
        if received != expected_size:
            raise FixtureDownloadError("response size disagrees with authority")
        if digest.hexdigest() != artifact["sha256"]:
            raise FixtureDownloadError("response SHA-256 disagrees with authority")
        os.replace(temporary, target)
        temporary = None
        return target
    except error.HTTPError as exc:
        raise FixtureDownloadError(f"HTTP response rejected without redirect: {exc.code}") from exc
    except error.URLError as exc:
        raise FixtureDownloadError(f"download failed: {exc.reason}") from exc
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
