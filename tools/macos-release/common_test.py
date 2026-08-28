from __future__ import annotations

import contextlib
import io
import os
import pathlib
import sys
import unittest
from unittest import mock

from common import ReleaseError, run
import release_subprocess


class CommonTests(unittest.TestCase):
    def test_subprocess_api_is_reexported_from_central_authority(self) -> None:
        self.assertIs(run, release_subprocess.run)
        self.assertIs(ReleaseError, release_subprocess.ReleaseError)

    def test_stream_flag_forwards_macos_release_stdout_and_stderr(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        script = "import sys; print('mac-out'); print('mac-err', file=sys.stderr)"
        with mock.patch.dict(os.environ, {"INTO_MD_RELEASE_STREAM_LOGS": "1"}):
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                output = run([sys.executable, "-c", script])

        self.assertEqual(output.strip(), "mac-out")
        self.assertIn("mac-out", stdout.getvalue())
        self.assertIn("mac-err", stderr.getvalue())

    def test_release_build_prefetches_complete_locked_cargo_closure(self) -> None:
        source = pathlib.Path(__file__).with_name("release.py").read_text(
            encoding="utf-8"
        )
        self.assertIn('run(["cargo", "fetch", "--locked"]', source)

if __name__ == "__main__":
    unittest.main()
