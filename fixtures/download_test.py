"""Network-boundary tests for the manual fixture downloader."""

from __future__ import annotations

import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import tempfile
import threading
import unittest

try:
    from fixtures.download_lib import FixtureDownloadError, download_artifact
except ModuleNotFoundError:  # Direct `python fixtures/download_test.py` execution.
    from download_lib import FixtureDownloadError, download_artifact


class _Handler(BaseHTTPRequestHandler):
    payload = b"authorized fixture bytes"
    good_requests = 0

    def do_GET(self):  # noqa: N802
        if self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/good")
            self.end_headers()
            return
        if self.path == "/oversize":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(self.payload + b"x")
            return
        if self.path == "/good":
            type(self).good_requests += 1
            self.send_response(200)
            self.send_header("Content-Length", str(len(self.payload)))
            self.end_headers()
            self.wfile.write(self.payload)
            return
        self.send_error(404)

    def log_message(self, _format, *_args):
        pass


class DownloadTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()

    def authority(self, path: str) -> dict[str, object]:
        return {
            "artifact_id": "test-fixture",
            "repository": "test_fixture",
            "downloaded_file_path": "fixture.bin",
            "url": f"http://127.0.0.1:{self.server.server_port}{path}",
            "allowed_hosts": ["127.0.0.1"],
            "sha256": hashlib.sha256(_Handler.payload).hexdigest(),
            "size": len(_Handler.payload),
            "maximum_redirects": 0,
            "license": "Apache-2.0",
            "manual_only": True,
            "included_in_release": False,
        }

    def test_exact_response_is_streamed_and_verified(self):
        with tempfile.TemporaryDirectory() as directory:
            target = download_artifact(
                self.authority("/good"), Path(directory), allow_http_loopback=True
            )
            self.assertEqual(target.read_bytes(), _Handler.payload)

    def test_redirect_is_rejected_without_requesting_target(self):
        before = _Handler.good_requests
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(FixtureDownloadError, "without redirect"):
                download_artifact(
                    self.authority("/redirect"), Path(directory), allow_http_loopback=True
                )
            self.assertEqual(_Handler.good_requests, before)
            self.assertFalse((Path(directory) / "test_fixture" / "fixture.bin").exists())

    def test_streaming_ceiling_rejects_oversize_without_output(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(FixtureDownloadError, "size ceiling"):
                download_artifact(
                    self.authority("/oversize"), Path(directory), allow_http_loopback=True
                )
            self.assertFalse((Path(directory) / "test_fixture" / "fixture.bin").exists())


if __name__ == "__main__":
    unittest.main()
