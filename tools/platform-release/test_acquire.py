import hashlib
import io
import pathlib
import tempfile
import unittest
import urllib.error
from unittest import mock

from acquire import acquire
from common import ReleaseError, authority


class Response(io.BytesIO):
    def __init__(self, value, status=200, headers=None):
        super().__init__(value)
        self.status = status
        self.headers = headers or {}

    def geturl(self):
        return "https://github.com/example/artifact"


class AcquireTests(unittest.TestCase):
    def test_authority_local_assets_are_available_without_network(self):
        downloads = {
            identity: item
            for identity, item in authority()["sharedDownloads"].items()
            if "path" in item
        }
        with tempfile.TemporaryDirectory() as name, mock.patch(
            "urllib.request.urlopen"
        ) as opened:
            cache = pathlib.Path(name)
            acquire(cache, downloads)
            opened.assert_not_called()
            self.assertEqual(
                {path.name for path in cache.iterdir()},
                set(downloads),
            )

    def test_local_asset_is_copied_without_network_access(self):
        expected = b"repository-owned model"
        item = {
            "path": "third_party/runtime-assets/models/test-model.bin",
            "source_url": "https://example.invalid/upstream-model",
            "bytes": len(expected),
            "sha256": hashlib.sha256(expected).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = root / item["path"]
            source.parent.mkdir(parents=True)
            source.write_bytes(expected)
            with (
                mock.patch("acquire.ROOT", root),
                mock.patch("urllib.request.urlopen") as opened,
            ):
                acquire(root / "cache", {"runtime": item})
            opened.assert_not_called()
            self.assertEqual((root / "cache/runtime").read_bytes(), expected)

    def test_local_asset_rejects_escape(self):
        item = {
            "path": "third_party/runtime-assets/models/../../outside.bin",
            "source_url": "https://example.invalid/upstream-model",
            "bytes": 1,
            "sha256": hashlib.sha256(b"x").hexdigest(),
        }
        with tempfile.TemporaryDirectory() as name:
            with self.assertRaisesRegex(
                ReleaseError, "portable POSIX spelling"
            ):
                acquire(pathlib.Path(name) / "cache", {"runtime": item})

    def test_ambiguous_local_authority_is_rejected_before_cache_hit(self):
        expected = b"cached"
        item = {
            "path": "third_party/runtime-assets/models/model.bin",
            "url": "https://example.invalid/model.bin",
            "source_url": "https://example.invalid/upstream-model",
            "bytes": len(expected),
            "sha256": hashlib.sha256(expected).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            (root / "runtime").write_bytes(expected)
            with mock.patch("acquire.ROOT", root), mock.patch(
                "urllib.request.urlopen"
            ) as opened:
                with self.assertRaisesRegex(
                    ReleaseError, "exactly one of path or url"
                ):
                    acquire(root, {"runtime": item})
            opened.assert_not_called()

    def test_local_asset_rejects_windows_separator_and_ads_spellings(self):
        base = {
            "source_url": "https://example.invalid/upstream-model",
            "bytes": 1,
            "sha256": hashlib.sha256(b"x").hexdigest(),
        }
        paths = [
            r"third_party\runtime-assets\models\model.bin",
            "third_party/runtime-assets/models/model.bin:payload",
            "third_party/runtime-assets/models/CON.bin",
        ]
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            for path in paths:
                item = {**base, "path": path}
                with self.subTest(path=path), mock.patch("acquire.ROOT", root):
                    with self.assertRaisesRegex(
                        ReleaseError, "portable POSIX spelling"
                    ):
                        acquire(root / "cache", {"runtime": item})

    def test_local_asset_must_be_in_the_models_directory(self):
        item = {
            "path": "third_party/runtime-assets/README.md",
            "source_url": "https://example.invalid/upstream-model",
            "bytes": 1,
            "sha256": hashlib.sha256(b"x").hexdigest(),
        }
        with tempfile.TemporaryDirectory() as name:
            with self.assertRaisesRegex(
                ReleaseError, "must be inside third_party/runtime-assets/models"
            ):
                acquire(pathlib.Path(name) / "cache", {"runtime": item})

    def test_http_failure_falls_back_to_hash_equivalent_mirror(self):
        expected = b"mirrored artifact"
        item = {
            "url": "https://paddle-model-ecology.bj.bcebos.com/artifact",
            "mirror_urls": ["https://github.com/example/artifact"],
            "bytes": len(expected),
            "sha256": hashlib.sha256(expected).hexdigest(),
        }
        denied = urllib.error.HTTPError(item["url"], 403, "Forbidden", {}, None)
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            with mock.patch(
                "urllib.request.urlopen", side_effect=[denied, Response(expected)]
            ) as opened:
                acquire(root, {"runtime": item})
            self.assertEqual(opened.call_count, 2)
            self.assertEqual(
                opened.call_args_list[1].args[0].full_url,
                item["mirror_urls"][0],
            )
            self.assertEqual((root / "runtime").read_bytes(), expected)

    def test_retries_truncated_response_and_publishes_only_verified_bytes(self):
        expected = b"complete artifact"
        item = {
            "url": "https://github.com/example/artifact",
            "bytes": len(expected),
            "sha256": hashlib.sha256(expected).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            with mock.patch(
                "urllib.request.urlopen",
                side_effect=[
                    Response(expected[:5]),
                    Response(
                        expected[5:],
                        status=206,
                        headers={"Content-Range": f"bytes 5-{len(expected) - 1}/{len(expected)}"},
                    ),
                ],
            ) as opened:
                acquire(root, {"runtime": item})
            self.assertEqual(opened.call_count, 2)
            self.assertEqual(opened.call_args_list[1].args[0].headers["Range"], "bytes=5-")
            self.assertEqual((root / "runtime").read_bytes(), expected)
            self.assertFalse((root / "runtime.download").exists())


if __name__ == "__main__":
    unittest.main()
