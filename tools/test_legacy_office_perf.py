from __future__ import annotations

import unittest

from legacy_office_perf.corpus import common_success_metrics
from legacy_office_perf.oracle import require_authority_oracle_count


class PerformanceGateTests(unittest.TestCase):
    def test_authority_denominator_cannot_shrink(self) -> None:
        with self.assertRaises(SystemExit):
            require_authority_oracle_count([{}] * 55, 56)
        require_authority_oracle_count([{}] * 56, 56)

    def test_common_files_are_joined_by_id_before_averaging(self) -> None:
        baseline = {
            "a.xls": {
                "meanMillis": 10,
                "peakRssBytes": 100,
                "peakTemporaryBytes": 20,
            },
            "b.xls": {
                "meanMillis": 30,
                "peakRssBytes": 300,
                "peakTemporaryBytes": 60,
            },
        }
        candidate = {
            "b.xls": {
                "meanMillis": 45,
                "peakRssBytes": 450,
                "peakTemporaryBytes": 90,
            },
            "a.xls": {
                "meanMillis": 5,
                "peakRssBytes": 50,
                "peakTemporaryBytes": 10,
            },
        }
        metrics = common_success_metrics(baseline, candidate, ["a.xls", "b.xls"], 0.5)
        self.assertEqual([item["file"] for item in metrics["files"]], ["a.xls", "b.xls"])
        self.assertEqual(metrics["averages"]["baselineMeanMillis"], 20)
        self.assertEqual(metrics["averages"]["candidateMeanMillis"], 25)
        self.assertEqual(metrics["averages"]["meanMillisRegressionFraction"], 0.25)
        self.assertEqual(metrics["averages"]["peakRssBytesRegressionFraction"], 0.25)
        self.assertEqual(metrics["averages"]["peakTemporaryBytesRegressionFraction"], 0.25)
        self.assertEqual([item["file"] for item in metrics["stress"]], ["b.xls"])


if __name__ == "__main__":
    unittest.main()
