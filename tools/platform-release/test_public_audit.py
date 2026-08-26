#!/usr/bin/env python3

from __future__ import annotations

import pathlib
import sys
import urllib.request
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from public_audit import SafeRedirectHandler


class SafeRedirectHandlerTests(unittest.TestCase):
    def redirect(self, destination: str) -> urllib.request.Request:
        source = urllib.request.Request(
            "https://api.github.com/repos/example/project/actions/artifacts/1/zip",
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": "Bearer official-release-token",
            },
        )
        redirected = SafeRedirectHandler().redirect_request(
            source, None, 302, "Found", {}, destination
        )
        assert redirected is not None
        return redirected

    def test_cross_origin_artifact_redirect_drops_authorization(self) -> None:
        redirected = self.redirect(
            "https://productionresultssa.blob.core.windows.net/actions-results/"
            "artifact.zip?sig=pre-signed"
        )
        self.assertIsNone(redirected.get_header("Authorization"))
        self.assertEqual(
            redirected.get_header("Accept"), "application/vnd.github+json"
        )

    def test_same_origin_redirect_keeps_authorization(self) -> None:
        redirected = self.redirect(
            "https://api.github.com/repositories/1/actions/artifacts/1/zip"
        )
        self.assertEqual(
            redirected.get_header("Authorization"),
            "Bearer official-release-token",
        )


if __name__ == "__main__":
    unittest.main()
