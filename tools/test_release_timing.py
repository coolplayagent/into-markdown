from __future__ import annotations

import importlib.util
import json
import pathlib
import tempfile
import unittest
from unittest import mock


PATH = pathlib.Path(__file__).with_name("release_timing.py")
SPEC = importlib.util.spec_from_file_location("release_timing", PATH)
assert SPEC and SPEC.loader
timing = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(timing)


class ReleaseTimingTests(unittest.TestCase):
    def test_records_each_phase_once_and_writes_summary(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            report = root / "timings.json"
            timing.record(report, "target", "helper-provider-build", 1234)
            value = json.loads(report.read_text(encoding="utf-8"))
            self.assertEqual(value["phases"]["helper-provider-build"]["durationMs"], 1234)
            with self.assertRaisesRegex(timing.TimingError, "already recorded"):
                timing.record(report, "target", "helper-provider-build", 1)
            summary = root / "summary.md"
            timing.summary(report, "target", summary)
            self.assertIn("1.234 s", summary.read_text(encoding="utf-8"))

    def test_finish_uses_persisted_wall_clock_marker(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            marker = root / "started"
            with mock.patch.object(timing.time, "time_ns", side_effect=[1_000_000_000, 2_500_000_000]):
                timing.mark(marker)
                timing.finish(root / "report.json", "target", "artifact-upload", marker)
            value = json.loads((root / "report.json").read_text(encoding="utf-8"))
            self.assertEqual(value["phases"]["artifact-upload"]["durationMs"], 1500)


if __name__ == "__main__":
    unittest.main()
