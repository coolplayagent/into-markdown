"""Independent XLS authority loading and cell-level output verification."""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys

from .monitor import file_sha256


def load_xls_authority(
    authority_path: pathlib.Path,
    corpus: pathlib.Path,
) -> list[dict[str, object]]:
    authority = json.loads(authority_path.read_text(encoding="utf-8"))
    if authority.get("schemaVersion") != 1:
        raise SystemExit("XLS authority schemaVersion must be 1")
    classifier = authority.get("classifier", {})
    if classifier.get("name") != "xlrd" or classifier.get("version") != "2.0.2":
        raise SystemExit("XLS authority must record the independent xlrd 2.0.2 classifier")
    items = authority.get("items")
    if not isinstance(items, list) or len(items) != 60:
        raise SystemExit("XLS authority must contain exactly 60 items")
    names = [item.get("file") for item in items]
    if len(set(names)) != len(names) or any(
        not isinstance(name, str)
        or pathlib.PurePath(name).name != name
        or not name.endswith(".xls")
        for name in names
    ):
        raise SystemExit("XLS authority contains duplicate or unsafe file names")
    actual = sorted(path.name for path in corpus.iterdir() if path.is_file())
    if actual != sorted(names):
        raise SystemExit("XLS corpus file set does not exactly match the authority")
    valid_count = 0
    raw_count = 0
    for item in items:
        source = corpus / str(item["file"])
        if source.is_symlink() or not source.is_file():
            raise SystemExit(f"XLS corpus item is not a regular file: {source.name}")
        if source.stat().st_size != item.get("bytes") or file_sha256(source) != item.get("sha256"):
            raise SystemExit(f"XLS corpus item differs from the authority: {source.name}")
        valid_count += int(item.get("valid") is True)
        raw_count += int(item.get("container") == "raw")
    if valid_count != 56 or raw_count != 1:
        raise SystemExit("XLS authority must classify exactly 56 valid files and one raw BIFF file")
    return items


def load_xls_oracle(
    oracle_tool: pathlib.Path,
    xlrd_path: pathlib.Path,
    corpus: pathlib.Path,
    authority_path: pathlib.Path,
) -> tuple[dict[str, dict[str, object]], dict[str, dict[str, object]]]:
    environment = os.environ.copy()
    environment["PYTHONPATH"] = os.pathsep.join(
        filter(None, [str(xlrd_path), environment.get("PYTHONPATH", "")])
    )
    result = subprocess.run(
        [
            sys.executable,
            str(oracle_tool),
            "--corpus",
            str(corpus),
            "--authority",
            str(authority_path),
        ],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        timeout=120,
    )
    if result.returncode != 0:
        raise SystemExit(
            "legacy XLS oracle failed: "
            + result.stderr.decode("utf-8", errors="replace")[-2048:]
        )
    decoded = json.loads(result.stdout)
    if decoded.get("schemaVersion") != 1 or decoded.get("classifier") != {
        "name": "xlrd",
        "version": "2.0.2",
    }:
        raise SystemExit("legacy XLS oracle returned an unexpected schema or xlrd version")
    classifications = decoded.get("classifications")
    if decoded.get("safetyClassifier") != "cfb-workbook-alias-v1" or not isinstance(
        classifications, list
    ) or len(classifications) != 60:
        raise SystemExit("legacy XLS oracle must return 60 independent safety classifications")
    classification_index = {str(item.get("file")): item for item in classifications}
    if len(classification_index) != len(classifications):
        raise SystemExit("legacy XLS oracle returned duplicate safety classifications")
    items = decoded.get("items")
    effective_valid = sum(item.get("effectiveValid") is True for item in classifications)
    if not isinstance(items, list) or len(items) != effective_valid:
        raise SystemExit("legacy XLS oracle item count disagrees with effective classification")
    indexed = {str(item.get("file")): item for item in items}
    if len(indexed) != len(items):
        raise SystemExit("legacy XLS oracle returned duplicate files")
    return indexed, classification_index


def inline_text(inline: dict[str, object]) -> str:
    kind = inline.get("type")
    data = inline.get("data")
    if kind == "text" and isinstance(data, dict):
        return str(data.get("value", ""))
    if kind == "code":
        return str(data or "")
    if isinstance(data, dict) and isinstance(data.get("content"), list):
        return "".join(inline_text(item) for item in data["content"])
    return ""


def candidate_cell_text(cell: dict[str, object]) -> tuple[str, tuple[int, int] | None]:
    text: list[str] = []
    coordinate = None
    for node in cell.get("blocks", []):
        block = node.get("block", {})
        if block.get("type") == "paragraph" and isinstance(block.get("data"), list):
            text.extend(inline_text(item) for item in block["data"])
        locator = node.get("provenance", {}).get("locator", {})
        cell_ref = locator.get("cell") if isinstance(locator, dict) else None
        if isinstance(cell_ref, dict):
            coordinate = (int(cell_ref["row"]), int(cell_ref["column"]))
    return "".join(text), coordinate


def decode_tsv_value(value: str) -> str:
    output: list[str] = []
    index = 0
    escapes = {"t": "\t", "r": "\r", "n": "\n", "\\": "\\", "`": "`"}
    while index < len(value):
        if value[index] == "\\" and index + 1 < len(value) and value[index + 1] in escapes:
            output.append(escapes[value[index + 1]])
            index += 2
        else:
            output.append(value[index])
            index += 1
    return "".join(output)


def candidate_sheet(
    sheet_node: dict[str, object], metadata_merges: list[list[int]]
) -> dict[str, object]:
    sheet = sheet_node.get("block", {}).get("data", {})
    values: dict[tuple[int, int], str] = {}
    merges: list[list[int]] = list(metadata_merges)
    seen_coordinates: set[tuple[int, int]] = set()
    duplicates: list[tuple[int, int]] = []
    rows_seen = 0
    columns_seen = 0
    provenance_order = True
    coordinate_order = True
    last_coordinate: tuple[int, int] | None = None
    paged_row = 0

    def record(coordinate: tuple[int, int], value: str) -> None:
        nonlocal coordinate_order, last_coordinate
        if coordinate in seen_coordinates:
            duplicates.append(coordinate)
        else:
            seen_coordinates.add(coordinate)
        if last_coordinate is not None and coordinate <= last_coordinate:
            coordinate_order = False
        last_coordinate = coordinate
        if value:
            values[coordinate] = value

    for node in sheet.get("blocks", []):
        block = node.get("block", {})
        if block.get("type") == "table":
            occupied_until: dict[int, int] = {}
            for row_index, row in enumerate(block.get("data", {}).get("rows", [])):
                column = 0
                for cell in row.get("cells", []):
                    while occupied_until.get(column, -1) >= row_index:
                        column += 1
                    row_span = int(cell.get("rowSpan", 1))
                    column_span = int(cell.get("columnSpan", 1))
                    text, provenance = candidate_cell_text(cell)
                    if provenance is not None and provenance != (row_index, column):
                        provenance_order = False
                    record((row_index, column), text)
                    if row_span > 1 or column_span > 1:
                        merges.append(
                            [
                                row_index,
                                column,
                                row_index + row_span - 1,
                                column + column_span - 1,
                            ]
                        )
                    for covered in range(column, column + column_span):
                        if row_span > 1:
                            occupied_until[covered] = row_index + row_span - 1
                    column += column_span
                columns_seen = max(columns_seen, column)
                rows_seen = max(rows_seen, row_index + 1)
        elif block.get("type") == "code" and block.get("data", {}).get("language") == "tsv":
            text = str(block.get("data", {}).get("text", ""))
            for line in text.splitlines():
                fields = line.split("\t")
                for column, field in enumerate(fields):
                    record((paged_row, column), decode_tsv_value(field))
                columns_seen = max(columns_seen, len(fields))
                paged_row += 1
            rows_seen = max(rows_seen, paged_row)
    return {
        "name": str(sheet.get("name", "")),
        "rows": rows_seen,
        "columns": columns_seen,
        "values": values,
        "merges": merges,
        "provenanceOrder": provenance_order and coordinate_order,
        "duplicateCoordinates": duplicates,
    }


def metadata_merges(document: dict[str, object], sheet_index: int) -> list[list[int]]:
    properties = document.get("metadata", {}).get("properties", {})
    if not isinstance(properties, dict):
        return []
    encoded = properties.get(f"spreadsheet.sheet.{sheet_index}.mergedRanges")
    if not isinstance(encoded, str) or not encoded:
        return []
    output: list[list[int]] = []
    for item in encoded.split(";"):
        fields = item.split(",")
        if len(fields) != 4:
            raise ValueError("candidate merged-range metadata is malformed")
        output.append([int(field) for field in fields])
    return output


def cached_display(value: str) -> str | None:
    marker = " [cached: "
    if value.startswith("=") and marker in value and value.endswith("]"):
        return value.rsplit(marker, 1)[1][:-1]
    return None


def formula_expression(value: str) -> str | None:
    if not value.startswith("="):
        return None
    marker = " [cached: "
    expression = value[1:]
    if marker in expression and expression.endswith("]"):
        expression = expression.rsplit(marker, 1)[0]
    return expression


def canonical_formula(value: str) -> str:
    compact = "".join(value.split())
    return re.sub(r"(?<![A-Za-z0-9_.])(\d+)\.0+(?![A-Za-z0-9_.])", r"\1", compact)


def value_matches(expected: dict[str, object], actual: str) -> bool:
    expected_formula = expected.get("formula")
    if expected.get("formulaRequired") is True:
        actual_formula = formula_expression(actual)
        if actual_formula is None or not actual_formula.strip():
            return False
    if expected_formula is not None:
        actual_formula = formula_expression(actual)
        if actual_formula is None or canonical_formula(actual_formula) != canonical_formula(
            str(expected_formula)
        ):
            return False
    cached = cached_display(actual)
    display = actual
    if cached is not None:
        display = cached
    elif expected.get("formulaRequired") is True and formula_expression(actual) is not None:
        display = ""
    formatted = expected.get("formattedDisplay")
    if formatted is not None:
        return display == str(formatted)
    kind = expected["kind"]
    value = expected["value"]
    if kind == "number":
        try:
            candidate = float(display)
        except ValueError:
            return False
        reference = float(value)
        return abs(candidate - reference) <= max(1e-12, abs(reference) * 1e-12)
    if kind == "boolean":
        return display == str(value).lower()
    if kind == "error":
        normalize = lambda text: "".join(character for character in text.upper() if character.isalnum())
        return normalize(display) == normalize(str(value))
    return display == str(value)


def verify_xls_oracle(
    output_path: pathlib.Path,
    oracle: dict[str, object],
) -> dict[str, object]:
    document = json.loads(output_path.read_text(encoding="utf-8"))
    candidate_sheets = [
        candidate_sheet(node, metadata_merges(document, index))
        for index, node in enumerate(document.get("blocks", []))
        if node.get("block", {}).get("type") == "sheet"
    ]
    oracle_sheets = oracle.get("sheets", [])
    errors: list[str] = []
    sheet_order = [sheet["name"] for sheet in candidate_sheets] == [
        str(sheet["name"]) for sheet in oracle_sheets
    ]
    if not sheet_order:
        errors.append("sheet order or names differ from xlrd")
    expected_cells = 0
    matched_cells = 0
    kinds = {"text": 0, "number": 0, "date": 0, "boolean": 0, "error": 0}
    merges_match = len(candidate_sheets) == len(oracle_sheets)
    row_cell_order = True
    exact_shape = len(candidate_sheets) == len(oracle_sheets)
    duplicate_free = True
    extra_cells = 0
    missing_cells = 0
    for index, expected_sheet in enumerate(oracle_sheets):
        if index >= len(candidate_sheets):
            break
        actual_sheet = candidate_sheets[index]
        row_cell_order = row_cell_order and bool(actual_sheet["provenanceOrder"])
        duplicate_free = duplicate_free and not actual_sheet["duplicateCoordinates"]
        if actual_sheet["duplicateCoordinates"]:
            errors.append(f"sheet {index} repeats candidate coordinates")
        if (
            int(actual_sheet["rows"]) != int(expected_sheet["rows"])
            or int(actual_sheet["columns"]) != int(expected_sheet["columns"])
        ):
            exact_shape = False
            errors.append(f"sheet {index} row/column bounds differ from xlrd")
        expected_merges = sorted(expected_sheet["merges"])
        actual_merges = sorted(actual_sheet["merges"])
        if actual_merges != expected_merges:
            merges_match = False
            errors.append(f"sheet {index} merged ranges differ from xlrd")
        expected_by_coordinate: dict[tuple[int, int], dict[str, object]] = {}
        for cell in expected_sheet["cells"]:
            coordinate = (int(cell["row"]), int(cell["column"]))
            if coordinate in expected_by_coordinate:
                errors.append(f"sheet {index} oracle repeats coordinate {coordinate}")
            expected_by_coordinate[coordinate] = cell
        actual_coordinates = set(actual_sheet["values"])
        expected_coordinates = set(expected_by_coordinate)
        extra = sorted(actual_coordinates - expected_coordinates)
        missing = sorted(expected_coordinates - actual_coordinates)
        extra_cells += len(extra)
        missing_cells += len(missing)
        if extra:
            errors.append(f"sheet {index} has extra candidate cells: {extra[:4]}")
        if missing:
            errors.append(f"sheet {index} is missing candidate cells: {missing[:4]}")
        for coordinate, cell in expected_by_coordinate.items():
            expected_cells += 1
            kinds[str(cell["kind"])] += 1
            actual = actual_sheet["values"].get(coordinate)
            if actual is not None and value_matches(cell, actual):
                matched_cells += 1
            else:
                errors.append(f"sheet {index} cell {coordinate} differs from xlrd")
                if len(errors) >= 32:
                    break
    if not row_cell_order:
        errors.append("cell provenance order differs from row/cell emission order")
    recall = 1.0 if expected_cells == 0 else matched_cells / expected_cells
    digest = hashlib.sha256(
        json.dumps(oracle, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    return {
        "verified": (
            not errors
            and recall == 1.0
            and merges_match
            and sheet_order
            and row_cell_order
            and exact_shape
            and duplicate_free
            and extra_cells == 0
            and missing_cells == 0
        ),
        "oracleSha256": digest,
        "expectedNonEmptyCells": expected_cells,
        "matchedNonEmptyCells": matched_cells,
        "recall": round(recall, 9),
        "valueKinds": kinds,
        "sheetOrder": sheet_order,
        "rowCellOrder": row_cell_order,
        "exactShape": exact_shape,
        "duplicateFree": duplicate_free,
        "extraCells": extra_cells,
        "missingCells": missing_cells,
        "mergedRanges": merges_match,
        "errors": errors[:32],
    }
