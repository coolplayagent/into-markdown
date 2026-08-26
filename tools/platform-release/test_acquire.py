import hashlib
import io
import pathlib
import tempfile
import unittest
import urllib.error
from unittest import mock

from acquire import acquire


class Response(io.BytesIO):
    def __init__(self, value, status=200, headers=None):
        super().__init__(value)
        self.status = status
        self.headers = headers or {}

    def geturl(self):
        return "https://github.com/example/artifact"


class AcquireTests(unittest.TestCase):
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
