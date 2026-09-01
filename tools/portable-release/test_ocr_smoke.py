"""Installed OCR acceptance must prove recognized text and bounded execution."""

import copy
import importlib.util
import pathlib
import unittest

SPEC = importlib.util.spec_from_file_location("ocr_smoke", pathlib.Path(__file__).with_name("ocr_smoke.py"))
smoke = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(smoke)


class OcrSmokeTests(unittest.TestCase):
    def test_signed_process_group_budget_matches_release_contract(self):
        self.assertEqual(smoke.SIGNED_WORKER_BYTES, 2048 * 1024**2)

    def test_structured_ocr_survives_markdown_escaping_and_rejects_native_only_text(self):
        document = {"markdown": "Clear scans\\.", "document": {"blocks": [{"type": "text",
            "data": {"value": "Clear scans.", "provenance": {"kind": "localOcr"}}}]}}
        report = {"failed": 0, "resourceUsage": {"sharedLeaseBudgetBytes": smoke.MEMORY_BYTES,
            "sharedLeasePeakBytes": 1024, "ocr": {"recognizedChars": 12},
            "ocrRuntime": {"requests": 1, "recognitionMemoryRefusals": 0,
                "workerBudgetMinBytes": smoke.SIGNED_WORKER_BYTES,
                "workerBudgetMaxBytes": smoke.SIGNED_WORKER_BYTES}}}
        self.assertEqual(smoke.verify_result(document, report, "Clear scans.", ValueError),
                         report["resourceUsage"])
        native = copy.deepcopy(document)
        native["document"]["blocks"][0]["data"]["provenance"]["kind"] = "nativeParser"
        with self.assertRaisesRegex(ValueError, "expected text"):
            smoke.verify_result(native, report, "Clear scans.", ValueError)
        for field, value in (("requests", 0), ("recognitionMemoryRefusals", 1),
                             ("workerBudgetMaxBytes", smoke.SIGNED_WORKER_BYTES + 1)):
            invalid = copy.deepcopy(report)
            invalid["resourceUsage"]["ocrRuntime"][field] = value
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "evidence"):
                smoke.verify_result(document, invalid, "Clear scans.", ValueError)


if __name__ == "__main__":
    unittest.main()
