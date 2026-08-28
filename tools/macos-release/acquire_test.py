from __future__ import annotations

import hashlib
import io
import pathlib
import tempfile
import unittest
import urllib.error
from unittest import mock

import acquire


class Response(io.BytesIO):
    def geturl(self) -> str:
        return "https://github.com/owner/repository/download/runtime"


class AcquireTests(unittest.TestCase):
    def test_local_asset_is_copied_without_network_access(self) -> None:
        payload = b"repository-owned model"
        item = {
            "id": "runtime",
            "path": "third_party/runtime-assets/models/test-model.bin",
            "source_url": "https://example.invalid/upstream-model",
            "bytes": len(payload),
            "sha256": hashlib.sha256(payload).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source = root / item["path"]
            source.parent.mkdir(parents=True)
            source.write_bytes(payload)
            with (
                mock.patch.object(acquire, "ROOT", root),
                mock.patch.object(
                    acquire, "authority", return_value={"downloads": [item]}
                ),
                mock.patch("urllib.request.urlopen") as opened,
            ):
                acquire.acquire(root / "cache")
            opened.assert_not_called()
            self.assertEqual((root / "cache/runtime").read_bytes(), payload)

    def test_local_asset_rejects_escape_and_symlink(self) -> None:
        payload = b"x"
        digest = hashlib.sha256(payload).hexdigest()
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            outside = root / "outside"
            outside.mkdir()
            (outside / "model.bin").write_bytes(payload)
            cases = [
                (
                    "third_party/runtime-assets/models/../outside/model.bin",
                    "portable POSIX spelling",
                ),
                (
                    "third_party/runtime-assets/models/model.bin",
                    "contains a link",
                ),
            ]
            link = root / "third_party/runtime-assets/models"
            link.parent.mkdir(parents=True)
            try:
                link.symlink_to(outside, target_is_directory=True)
            except OSError:
                cases.pop()
            for local_path, diagnostic in cases:
                item = {
                    "id": "runtime",
                    "path": local_path,
                    "source_url": "https://example.invalid/upstream-model",
                    "bytes": len(payload),
                    "sha256": digest,
                }
                with self.subTest(path=local_path), mock.patch.object(
                    acquire, "ROOT", root
                ), mock.patch.object(
                    acquire, "authority", return_value={"downloads": [item]}
                ):
                    with self.assertRaisesRegex(acquire.ReleaseError, diagnostic):
                        acquire.acquire(root / "cache")

    def test_ambiguous_local_authority_is_rejected_before_cache_hit(self) -> None:
        payload = b"cached"
        item = {
            "id": "runtime",
            "path": "third_party/runtime-assets/models/model.bin",
            "url": "https://example.invalid/model.bin",
            "source_url": "https://example.invalid/upstream-model",
            "bytes": len(payload),
            "sha256": hashlib.sha256(payload).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            cache = root / "cache"
            cache.mkdir()
            (cache / "runtime").write_bytes(payload)
            with mock.patch.object(acquire, "ROOT", root), mock.patch.object(
                acquire, "authority", return_value={"downloads": [item]}
            ), mock.patch("urllib.request.urlopen") as opened:
                with self.assertRaisesRegex(
                    acquire.ReleaseError, "exactly one of path or url"
                ):
                    acquire.acquire(cache)
            opened.assert_not_called()

    def test_local_authority_requires_source_size_and_hash(self) -> None:
        payload = b"cached"
        base = {
            "id": "runtime",
            "path": "third_party/runtime-assets/models/model.bin",
            "source_url": "https://example.invalid/upstream-model",
            "bytes": len(payload),
            "sha256": hashlib.sha256(payload).hexdigest(),
        }
        cases = {
            "source_url": "invalid source_url",
            "bytes": "invalid bytes",
            "sha256": "invalid SHA-256",
        }
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            cache = root / "cache"
            cache.mkdir()
            (cache / "runtime").write_bytes(payload)
            for missing, diagnostic in cases.items():
                item = dict(base)
                del item[missing]
                with self.subTest(missing=missing), mock.patch.object(
                    acquire, "ROOT", root
                ), mock.patch.object(
                    acquire, "authority", return_value={"downloads": [item]}
                ):
                    with self.assertRaisesRegex(acquire.ReleaseError, diagnostic):
                        acquire.acquire(cache)

    def test_authority_uses_local_small_models_and_repository_whisper(self) -> None:
        value = acquire.authority()
        downloads = {item["id"]: item for item in value["downloads"]}
        local_identities = {
            "ocr-detector",
            "ocr-recognizer",
            "ocr-dictionary",
            "silero-vad",
            "3dspeaker",
        }
        for identity in local_identities:
            item = downloads[identity]
            self.assertTrue(
                item["path"].startswith("third_party/runtime-assets/models/")
            )
            self.assertTrue(item["source_url"].startswith("https://"))
            self.assertNotIn("url", item)
        whisper = downloads["whisper-small"]
        self.assertEqual(
            whisper["url"],
            "https://github.com/coolplayagent/into-markdown/releases/download/"
            "runtime-assets/ggml-small.bin",
        )
        self.assertTrue(whisper["source_url"].startswith("https://huggingface.co/"))

        with tempfile.TemporaryDirectory() as temporary, mock.patch(
            "urllib.request.urlopen"
        ) as opened:
            cache = pathlib.Path(temporary)
            acquire.acquire(cache, local_identities)
            opened.assert_not_called()
            self.assertEqual(
                {path.name for path in cache.iterdir()},
                local_identities,
            )

    def test_transient_http_error_is_retried_and_verified(self) -> None:
        payload = b"verified-runtime"
        item = {
            "id": "runtime",
            "url": "https://paddle-model-ecology.bj.bcebos.com/runtime",
            "mirror_urls": [
                "https://github.com/owner/repository/download/runtime"
            ],
            "bytes": len(payload),
            "sha256": hashlib.sha256(payload).hexdigest(),
        }
        denied = urllib.error.HTTPError(item["url"], 403, "Forbidden", {}, None)
        with tempfile.TemporaryDirectory() as temporary:
            cache = pathlib.Path(temporary)
            with mock.patch.object(acquire, "authority", return_value={"downloads": [item]}), mock.patch(
                "urllib.request.urlopen", side_effect=[denied, Response(payload)]
            ) as opened:
                acquire.acquire(cache)
            self.assertEqual(opened.call_count, 2)
            self.assertEqual(
                opened.call_args_list[1].args[0].full_url,
                item["mirror_urls"][0],
            )
            self.assertEqual((cache / "runtime").read_bytes(), payload)


if __name__ == "__main__":
    unittest.main()
