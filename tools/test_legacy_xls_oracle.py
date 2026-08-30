from __future__ import annotations

import copy
import json
import pathlib
import tempfile
import unittest

from legacy_office_perf.oracle import verify_xls_oracle


def cell(row: int, column: int, value: str) -> dict[str, object]:
    return {
        "rowSpan": 1,
        "columnSpan": 1,
        "header": False,
        "blocks": [
            {
                "id": f"cell-{row}-{column}",
                "block": {"type": "paragraph", "data": [{"type": "code", "data": value}]},
                "provenance": {
                    "locator": {"cell": {"row": row, "column": column}}
                },
            }
        ],
    }


def candidate() -> dict[str, object]:
    return {
        "metadata": {"properties": {}},
        "blocks": [
            {
                "id": "sheet-0",
                "block": {
                    "type": "sheet",
                    "data": {
                        "name": "Sheet 1",
                        "blocks": [
                            {
                                "id": "table-0",
                                "block": {
                                    "type": "table",
                                    "data": {
                                        "rows": [
                                            {
                                                "cells": [
                                                    cell(0, 0, "source"),
                                                    cell(0, 1, "=A1 [cached: 50%]"),
                                                ]
                                            }
                                        ]
                                    },
                                },
                            }
                        ],
                    },
                },
            }
        ],
    }


def oracle() -> dict[str, object]:
    return {
        "sheets": [
            {
                "name": "Sheet 1",
                "rows": 1,
                "columns": 2,
                "merges": [],
                "cells": [
                    {"row": 0, "column": 0, "kind": "text", "value": "source"},
                    {
                        "row": 0,
                        "column": 1,
                        "kind": "number",
                        "value": 0.5,
                        "formulaRequired": True,
                        "formula": "A1",
                        "formattedDisplay": "50%",
                    },
                ],
            }
        ]
    }


class OracleAdversarialTests(unittest.TestCase):
    def verify(self, document: dict[str, object]) -> dict[str, object]:
        with tempfile.TemporaryDirectory() as temporary:
            path = pathlib.Path(temporary) / "candidate.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            return verify_xls_oracle(path, oracle())

    def test_exact_candidate_passes(self) -> None:
        self.assertTrue(self.verify(candidate())["verified"])

    def test_copied_table_and_duplicate_coordinates_fail(self) -> None:
        document = candidate()
        blocks = document["blocks"][0]["block"]["data"]["blocks"]
        duplicate = copy.deepcopy(blocks[0])
        duplicate["id"] = "table-copy"
        blocks.append(duplicate)
        result = self.verify(document)
        self.assertFalse(result["verified"])
        self.assertFalse(result["duplicateFree"])

    def test_extra_cell_and_column_fail(self) -> None:
        document = candidate()
        row = document["blocks"][0]["block"]["data"]["blocks"][0]["block"]["data"]["rows"][0]
        row["cells"].append(cell(0, 2, "hallucinated"))
        result = self.verify(document)
        self.assertFalse(result["verified"])
        self.assertEqual(result["extraCells"], 1)
        self.assertFalse(result["exactShape"])

    def test_formula_expression_and_formatted_display_are_exact(self) -> None:
        wrong_formula = candidate()
        formula = wrong_formula["blocks"][0]["block"]["data"]["blocks"][0]["block"]["data"]["rows"][0]["cells"][1]
        formula["blocks"][0]["block"]["data"][0]["data"] = "=B1 [cached: 50%]"
        self.assertFalse(self.verify(wrong_formula)["verified"])

        wrong_display = candidate()
        formula = wrong_display["blocks"][0]["block"]["data"]["blocks"][0]["block"]["data"]["rows"][0]["cells"][1]
        formula["blocks"][0]["block"]["data"][0]["data"] = "=A1 [cached: 0.5]"
        self.assertFalse(self.verify(wrong_display)["verified"])

        missing_formula = candidate()
        formula = missing_formula["blocks"][0]["block"]["data"]["blocks"][0]["block"]["data"]["rows"][0]["cells"][1]
        formula["blocks"][0]["block"]["data"][0]["data"] = "50%"
        self.assertFalse(self.verify(missing_formula)["verified"])


if __name__ == "__main__":
    unittest.main()
