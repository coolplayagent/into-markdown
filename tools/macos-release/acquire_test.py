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
