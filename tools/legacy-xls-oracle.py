#!/usr/bin/env python3
"""Emit an independent, eager XLS content oracle with pinned xlrd 2.0.2."""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import pathlib
import sys

import xlrd


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", type=pathlib.Path, required=True)
    parser.add_argument("--authority", type=pathlib.Path, required=True)
    return parser.parse_args()


def date_display(value: float, datemode: int) -> str:
    converted = xlrd.xldate.xldate_as_datetime(value, datemode)
    return converted.isoformat(sep=" ")


def cell_value(cell: xlrd.sheet.Cell, datemode: int) -> dict[str, object]:
    if cell.ctype in (xlrd.XL_CELL_EMPTY, xlrd.XL_CELL_BLANK):
        return {"kind": "empty", "value": ""}
    if cell.ctype == xlrd.XL_CELL_TEXT:
        return {"kind": "text", "value": str(cell.value)}
    if cell.ctype == xlrd.XL_CELL_NUMBER:
        return {"kind": "number", "value": float(cell.value)}
    if cell.ctype == xlrd.XL_CELL_DATE:
        return {"kind": "date", "value": date_display(float(cell.value), datemode)}
    if cell.ctype == xlrd.XL_CELL_BOOLEAN:
        return {"kind": "boolean", "value": bool(cell.value)}
    if cell.ctype == xlrd.XL_CELL_ERROR:
        return {
            "kind": "error",
            "value": xlrd.biffh.error_text_from_code.get(int(cell.value), "#UNKNOWN!"),
        }
    raise RuntimeError(f"unsupported xlrd cell type {cell.ctype}")


def workbook_oracle(path: pathlib.Path) -> dict[str, object]:
    workbook = xlrd.open_workbook(
        str(path), on_demand=False, formatting_info=True, logfile=io.StringIO()
    )
    sheets: list[dict[str, object]] = []
    for index in range(workbook.nsheets):
        sheet = workbook.sheet_by_index(index)
        merges = [
            [row_start, column_start, row_end - 1, column_end - 1]
            for row_start, row_end, column_start, column_end in sheet.merged_cells
        ]
        cells: list[dict[str, object]] = []
        for row in range(sheet.nrows):
            for column in range(sheet.ncols):
                value = cell_value(sheet.cell(row, column), workbook.datemode)
                if value["kind"] != "empty" and value["value"] != "":
                    cells.append({"row": row, "column": column, **value})
        sheets.append(
            {
                "index": index,
                "name": sheet.name,
                "rows": sheet.nrows,
                "columns": sheet.ncols,
                "merges": merges,
                "cells": cells,
            }
        )
    return {"sheets": sheets}


def main() -> int:
    args = parse_args()
    if xlrd.__version__ != "2.0.2":
        raise SystemExit(f"legacy XLS oracle requires xlrd 2.0.2, got {xlrd.__version__}")
    corpus = args.corpus.resolve(strict=True)
    authority = json.loads(args.authority.resolve(strict=True).read_text(encoding="utf-8"))
    items = authority.get("items")
    if not isinstance(items, list):
        raise SystemExit("authority items are missing")
    output = []
    with contextlib.redirect_stdout(io.StringIO()):
        for item in items:
            if item.get("valid") is not True:
                continue
            name = item.get("file")
            if not isinstance(name, str) or pathlib.PurePath(name).name != name:
                raise SystemExit("authority contains an unsafe file name")
            source = corpus / name
            if source.is_symlink() or not source.is_file():
                raise SystemExit(f"oracle source is not a regular file: {name}")
            oracle = workbook_oracle(source)
            output.append({"file": name, **oracle})
    encoded = json.dumps(
        {
            "schemaVersion": 1,
            "classifier": {"name": "xlrd", "version": xlrd.__version__},
            "items": output,
        },
        ensure_ascii=False,
        separators=(",", ":"),
    ).encode("utf-8")
    sys.stdout.buffer.write(encoded + b"\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
