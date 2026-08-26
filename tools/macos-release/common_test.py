from __future__ import annotations

import pathlib
import sys
import unittest

from common import ReleaseError, run


class CommonTests(unittest.TestCase):
    def test_release_build_prefetches_complete_locked_cargo_closure(self) -> None:
        source = pathlib.Path(__file__).with_name("release.py").read_text(
            encoding="utf-8"
        )
        self.assertIn('run(["cargo", "fetch", "--locked"]', source)

    def test_command_failure_preserves_bounded_diagnostic_tail(self) -> None:
        script = (
            "import sys; "
            "[print(f'diagnostic-{index}', file=sys.stderr) for index in range(45)]; "
            "raise SystemExit(7)"
        )
        with self.assertRaises(ReleaseError) as raised:
            run([sys.executable, "-c", script])
        message = str(raised.exception)
        self.assertIn("exit 7", message)
        self.assertNotIn("diagnostic-4\n", message)
        self.assertIn("diagnostic-5", message)
        self.assertIn("diagnostic-44", message)


if __name__ == "__main__":
    unittest.main()
