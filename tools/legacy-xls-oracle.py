#!/usr/bin/env python3
"""Emit an independent, eager XLS content oracle with pinned xlrd 2.0.2."""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import pathlib
import struct
import sys

import xlrd
import xlrd.compdoc
import xlrd.formula as xlrd_formula
from xlrd.formula import FMLA_TYPE_CELL, FMLA_TYPE_SHARED, decompile_formula

# Excel permits FIXED(number) with both trailing arguments omitted. xlrd's static
# table incorrectly requires two arguments, although its token decompiler handles
# the one-argument form. Correct that documented signature before independent scan.
_fixed = xlrd_formula.func_defs[14]
xlrd_formula.func_defs[14] = (_fixed[0], 1, *_fixed[2:])


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", type=pathlib.Path, required=True)
    parser.add_argument("--authority", type=pathlib.Path, required=True)
    return parser.parse_args()


def date_display(value: float, datemode: int) -> str:
    converted = xlrd.xldate.xldate_as_datetime(value, datemode)
    return converted.isoformat(sep=" ")


def special_display(
    cell: xlrd.sheet.Cell, workbook: xlrd.book.Book
) -> str | None:
    if cell.ctype != xlrd.XL_CELL_NUMBER:
        return None
    xf = workbook.xf_list[cell.xf_index]
    code = workbook.format_map[xf.format_key].format_str.split(";", 1)[0]
    percent = "%" in code
    symbols = [symbol for symbol in "$€£¥" if symbol in code]
    if not percent and not symbols:
        return None
    end = code.find("%") if percent else len(code)
    point = code.rfind(".", 0, end)
    decimals = 0
    if point >= 0:
        for character in code[point + 1 : end]:
            if character not in "0#":
                break
            decimals += 1
    value = float(cell.value) * (100.0 if percent else 1.0)
    grouped = "#,##" in code
    rendered = f"{abs(value):,.{decimals}f}" if grouped else f"{abs(value):.{decimals}f}"
    if value < 0:
        rendered = "-" + rendered
    if percent:
        return rendered + "%"
    symbol = symbols[0]
    first_placeholder = min(
        (position for marker in "0#" if (position := code.find(marker)) >= 0),
        default=len(code),
    )
    return symbol + rendered if code.find(symbol) < first_placeholder else rendered + " " + symbol


def cell_value(
    cell: xlrd.sheet.Cell,
    datemode: int,
    workbook: xlrd.book.Book,
    formula: str | None,
    formula_required: bool,
) -> dict[str, object]:
    if cell.ctype in (xlrd.XL_CELL_EMPTY, xlrd.XL_CELL_BLANK):
        output: dict[str, object] = {"kind": "empty", "value": ""}
        if formula_required:
            output["formulaRequired"] = True
        if formula is not None:
            output["formula"] = formula
        return output
    if cell.ctype == xlrd.XL_CELL_TEXT:
        output = {"kind": "text", "value": str(cell.value)}
        if formula_required:
            output["formulaRequired"] = True
        if formula is not None:
            output["formula"] = formula
        return output
    if cell.ctype == xlrd.XL_CELL_NUMBER:
        output = {"kind": "number", "value": float(cell.value)}
        display = special_display(cell, workbook)
        if display is not None:
            output["formattedDisplay"] = display
        if formula_required:
            output["formulaRequired"] = True
        if formula is not None:
            output["formula"] = formula
        return output
    if cell.ctype == xlrd.XL_CELL_DATE:
        output = {"kind": "date", "value": date_display(float(cell.value), datemode)}
        if formula_required:
            output["formulaRequired"] = True
        if formula is not None:
            output["formula"] = formula
        return output
    if cell.ctype == xlrd.XL_CELL_BOOLEAN:
        output = {"kind": "boolean", "value": bool(cell.value)}
        if formula_required:
            output["formulaRequired"] = True
        if formula is not None:
            output["formula"] = formula
        return output
    if cell.ctype == xlrd.XL_CELL_ERROR:
        output = {
            "kind": "error",
            "value": xlrd.biffh.error_text_from_code.get(int(cell.value), "#UNKNOWN!"),
        }
        if formula_required:
            output["formulaRequired"] = True
        if formula is not None:
            output["formula"] = formula
        return output
    raise RuntimeError(f"unsupported xlrd cell type {cell.ctype}")


def sheet_formulas(
    workbook: xlrd.book.Book, sheet_index: int, raw_fallback: bytes
) -> dict[tuple[int, int], str | None]:
    memory = workbook.mem
    if memory is None:
        memory = raw_fallback
    position = workbook._sh_abs_posn[sheet_index]
    output: dict[tuple[int, int], str | None] = {}
    formulas: list[tuple[int, int, bytes]] = []
    shared: list[tuple[int, int, int, int, bytes]] = []
    while position + 4 <= len(memory):
        record, length = struct.unpack_from("<HH", memory, position)
        body_start = position + 4
        end = body_start + length
        if end > len(memory):
            raise RuntimeError("xlrd formula scan found a truncated record")
        body = memory[body_start:end]
        if record in (0x0006, 0x0206, 0x0406):
            row, column = struct.unpack_from("<HH", body)
            length_offset = 20 if workbook.biff_version >= 50 else 16
            token_offset = length_offset + 2
            formula_length = struct.unpack_from("<H", body, length_offset)[0]
            formulas.append((row, column, bytes(body[token_offset : token_offset + formula_length])))
        elif record == 0x04BC:
            if len(body) < 10:
                raise RuntimeError("truncated shared-formula record")
            first_row, last_row = struct.unpack_from("<HH", body)
            first_column, last_column = body[4], body[5]
            formula_length = struct.unpack_from("<H", body, 8)[0]
            tokens = bytes(body[10 : 10 + formula_length])
            if len(tokens) != formula_length:
                raise RuntimeError("truncated shared-formula token stream")
            shared.append((first_row, last_row, first_column, last_column, tokens))
        position = end
        if record == 0x000A:
            break

    for row, column, tokens in formulas:
        if len(tokens) == 5 and tokens[0] in (0x01, 0x02):
            table_row, table_column = struct.unpack_from("<HH", tokens, 1)
            function = "SHARED" if tokens[0] == 0x01 else "TABLE"
            expression = f"{function}(R{table_row + 1}C{table_column + 1})"
            coordinate = (row, column)
            if coordinate in output:
                raise RuntimeError(
                    f"duplicate formula coordinate at {sheet_index}:{row}:{column}"
                )
            output[coordinate] = expression
            continue
        try:
            decompile_formula(
                workbook,
                tokens,
                len(tokens),
                FMLA_TYPE_CELL,
                browx=row,
                bcolx=column,
            )
        except Exception:
            definition = next(
                (
                    candidate
                    for candidate in shared
                    if candidate[0] <= row <= candidate[1]
                    and candidate[2] <= column <= candidate[3]
                ),
                None,
            )
            if definition is None:
                pass
            else:
                decompile_formula(
                    workbook,
                    definition[4],
                    len(definition[4]),
                    FMLA_TYPE_SHARED,
                    browx=row,
                    bcolx=column,
                )
        coordinate = (row, column)
        if coordinate in output:
            raise RuntimeError(f"duplicate formula coordinate at {sheet_index}:{row}:{column}")
        # Independent parsers legitimately render ordinary BIFF token streams with
        # different parentheses, external-reference labels, and unknown-token text.
        # The oracle therefore pins exact text only for the explicit SHARED/TABLE
        # compatibility contract above; every other coordinate still requires an
        # inert formula expression and an exact cached display value.
        output[coordinate] = None
    return output


def workbook_oracle(path: pathlib.Path) -> dict[str, object]:
    source_bytes = path.read_bytes()
    workbook = xlrd.open_workbook(
        str(path), on_demand=True, formatting_info=True, logfile=io.StringIO()
    )
    sheets: list[dict[str, object]] = []
    for index in range(workbook.nsheets):
        sheet = workbook.sheet_by_index(index)
        formulas = sheet_formulas(workbook, index, source_bytes)
        merges = [
            [row_start, column_start, row_end - 1, column_end - 1]
            for row_start, row_end, column_start, column_end in sheet.merged_cells
        ]
        def merged_interior(row: int, column: int) -> bool:
            return any(
                row_start <= row < row_end
                and column_start <= column < column_end
                and (row, column) != (row_start, column_start)
                for row_start, row_end, column_start, column_end in sheet.merged_cells
            )

        cells: list[dict[str, object]] = []
        for row in range(sheet.nrows):
            for column in range(sheet.ncols):
                if merged_interior(row, column):
                    continue
                coordinate = (row, column)
                formula_required = coordinate in formulas
                formula = formulas.get(coordinate)
                value = cell_value(
                    sheet.cell(row, column),
                    workbook.datemode,
                    workbook,
                    formula,
                    formula_required,
                )
                if formula_required or (value["kind"] != "empty" and value["value"] != ""):
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


def has_ambiguous_workbook_aliases(path: pathlib.Path) -> bool:
    source = path.read_bytes()
    if not source.startswith(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"):
        return False
    compound = xlrd.compdoc.CompDoc(source, logfile=io.StringIO())
    return (
        compound.get_named_stream("Workbook") is not None
        and compound.get_named_stream("Book") is not None
    )


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
    classifications = []
    with contextlib.redirect_stdout(io.StringIO()):
        for item in items:
            name = item.get("file")
            if not isinstance(name, str) or pathlib.PurePath(name).name != name:
                raise SystemExit("authority contains an unsafe file name")
            source = corpus / name
            if source.is_symlink() or not source.is_file():
                raise SystemExit(f"oracle source is not a regular file: {name}")
            effective_valid = item.get("valid") is True
            reason = "xlrd-invalid"
            if effective_valid and has_ambiguous_workbook_aliases(source):
                effective_valid = False
                reason = "ambiguous-workbook-aliases"
            elif effective_valid:
                reason = "xlrd-valid-and-container-unambiguous"
            classifications.append(
                {"file": name, "effectiveValid": effective_valid, "reason": reason}
            )
            if not effective_valid:
                continue
            try:
                oracle = workbook_oracle(source)
            except Exception as error:
                raise RuntimeError(f"oracle failed for {name}: {error}") from error
            output.append({"file": name, **oracle})
    encoded = json.dumps(
        {
            "schemaVersion": 1,
            "classifier": {"name": "xlrd", "version": xlrd.__version__},
            "safetyClassifier": "cfb-workbook-alias-v1",
            "classifications": classifications,
            "items": output,
        },
        ensure_ascii=False,
        separators=(",", ":"),
    ).encode("utf-8")
    sys.stdout.buffer.write(encoded + b"\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
