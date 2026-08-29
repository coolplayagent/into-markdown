from __future__ import annotations

import unittest

from common import ReleaseError, run
import release_subprocess


class CommonTests(unittest.TestCase):
    def test_subprocess_api_is_reexported_from_central_authority(self) -> None:
        self.assertIs(run, release_subprocess.run)
        self.assertIs(ReleaseError, release_subprocess.ReleaseError)


if __name__ == "__main__":
    unittest.main()
