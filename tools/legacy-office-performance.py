#!/usr/bin/env python3
"""Compare a baseline and candidate Core CLI on the Office 97-2003 corpus."""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import pathlib
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass


FORMATS = ("doc", "ppt", "xls")
MIB = 1024 * 1024


@dataclass(frozen=True)
class Observation:
    elapsed_ms: float
    peak_rss_bytes: int | None
    returncode: int
    stdout_sha256: str
    stderr_sha256: str


@dataclass(frozen=True)
class XlsObservation:
    elapsed_ms: float
    peak_rss_bytes: int | None
    returncode: int
    output_bytes: int
    output_sha256: str | None
    output_path: pathlib.Path
    peak_temporary_bytes: int
    temporary_bytes_after: int
    residual_paths: tuple[str, ...]
    report: dict[str, object] | None
    stderr: str


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-cli", type=pathlib.Path, required=True)
    parser.add_argument("--candidate-cli", type=pathlib.Path, required=True)
    parser.add_argument("--fixtures", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--parallelism", type=int, default=3)
    parser.add_argument("--xls-corpus", type=pathlib.Path)
    parser.add_argument("--xls-authority", type=pathlib.Path)
    parser.add_argument("--xls-xlrd-path", type=pathlib.Path)
    parser.add_argument("--xls-regression-limit", type=float, default=0.5)
    return parser.parse_args()


def private_environment(home: pathlib.Path) -> dict[str, str]:
    environment = {
        "HOME": str(home),
        "USERPROFILE": str(home),
        "XDG_CONFIG_HOME": str(home / "xdg-config"),
        "XDG_DATA_HOME": str(home / "xdg-data"),
        "TMPDIR": str(home / "tmp"),
        "TEMP": str(home / "tmp"),
        "TMP": str(home / "tmp"),
        "NO_COLOR": "1",
        "PATH": "",
    }
    for name in ("SystemRoot", "WINDIR"):
        if name in os.environ:
            environment[name] = os.environ[name]
    for directory in (home / "xdg-config", home / "xdg-data", home / "tmp"):
        directory.mkdir(parents=True, exist_ok=True)
    return environment


def linux_rss(pid: int) -> int | None:
    try:
        for line in pathlib.Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    except (FileNotFoundError, ProcessLookupError, ValueError):
        return None
    return None


def macos_rss(pid: int) -> int | None:
    try:
        result = subprocess.run(
            ["/bin/ps", "-o", "rss=", "-p", str(pid)],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=1,
        )
        return int(result.stdout.strip()) * 1024 if result.stdout.strip() else None
    except (OSError, subprocess.SubprocessError, ValueError):
        return None


def windows_rss(process: subprocess.Popen[bytes]) -> int | None:
    if sys.platform != "win32":
        return None

    class Counters(ctypes.Structure):
        _fields_ = [
            ("cb", ctypes.c_ulong),
            ("PageFaultCount", ctypes.c_ulong),
            ("PeakWorkingSetSize", ctypes.c_size_t),
            ("WorkingSetSize", ctypes.c_size_t),
            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
            ("PagefileUsage", ctypes.c_size_t),
            ("PeakPagefileUsage", ctypes.c_size_t),
        ]

    counters = Counters()
    counters.cb = ctypes.sizeof(counters)
    try:
        ok = ctypes.windll.psapi.GetProcessMemoryInfo(
            ctypes.c_void_p(process._handle), ctypes.byref(counters), counters.cb
        )
    except (AttributeError, OSError):
        return None
    return int(counters.PeakWorkingSetSize) if ok else None


def resident_bytes(process: subprocess.Popen[bytes]) -> int | None:
    if sys.platform.startswith("linux"):
        return linux_rss(process.pid)
    if sys.platform == "darwin":
        return macos_rss(process.pid)
    return windows_rss(process)


def observe(
    cli: pathlib.Path,
    arguments: list[str],
    current_dir: pathlib.Path,
    home: pathlib.Path,
) -> Observation:
    started = time.perf_counter()
    process = subprocess.Popen(
        [str(cli), *arguments],
        cwd=current_dir,
        env=private_environment(home),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    peak = 0
    deadline = time.monotonic() + 30
    while True:
        sample = resident_bytes(process)
        peak = max(peak, sample or 0)
        if process.poll() is not None:
            break
        if time.monotonic() >= deadline:
            process.kill()
            process.wait()
            raise RuntimeError("benchmark command exceeded 30 seconds")
        time.sleep(0.002)
    stdout, stderr = process.communicate()
    sample = resident_bytes(process)
    peak = max(peak, sample or 0)
    return Observation(
        elapsed_ms=(time.perf_counter() - started) * 1000,
        peak_rss_bytes=peak or None,
        returncode=process.returncode,
        stdout_sha256=hashlib.sha256(stdout).hexdigest(),
        stderr_sha256=hashlib.sha256(stderr).hexdigest(),
    )


def file_sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def directory_file_bytes(root: pathlib.Path) -> int:
    total = 0
    for path in root.rglob("*"):
        try:
            if path.is_file() and not path.is_symlink():
                total += path.stat().st_size
        except FileNotFoundError:
            continue
    return total


def transient_output_bytes(root: pathlib.Path, final_output: pathlib.Path) -> int:
    total = 0
    for path in root.rglob("*"):
        try:
            if path != final_output and path.is_file() and not path.is_symlink():
                total += path.stat().st_size
        except FileNotFoundError:
            continue
    return total


def output_residuals(root: pathlib.Path, allowed: pathlib.Path | None) -> tuple[str, ...]:
    if not root.exists():
        return ()
    return tuple(
        sorted(
            path.relative_to(root).as_posix()
            for path in root.rglob("*")
            if path != allowed and (path.is_file() or path.is_symlink())
        )
    )


def observe_xls(
    cli: pathlib.Path,
    source: pathlib.Path,
    current_dir: pathlib.Path,
    run_root: pathlib.Path,
) -> XlsObservation:
    home = run_root / "home"
    run_root.mkdir(parents=True)
    output_root = run_root / "output"
    output_root.mkdir(parents=True)
    output = output_root / "output.ir.json"
    report_path = run_root / "report.json"
    started = time.perf_counter()
    process = subprocess.Popen(
        [
            str(cli),
            "--no-config",
            str(source),
            "--format",
            "xls",
            "--error-policy",
            "best-effort",
            "--emit",
            "ir-json",
            "--asset-mode",
            "embed",
            "--quiet",
            "--output",
            str(output),
            "--conflict",
            "error",
            "--report",
            str(report_path),
        ],
        cwd=current_dir,
        env=private_environment(home),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    peak = 0
    peak_temporary = 0
    deadline = time.monotonic() + 120
    while True:
        sample = resident_bytes(process)
        peak = max(peak, sample or 0)
        peak_temporary = max(
            peak_temporary,
            directory_file_bytes(home / "tmp")
            + transient_output_bytes(output_root, output),
        )
        if process.poll() is not None:
            break
        if time.monotonic() >= deadline:
            process.kill()
            process.wait()
            raise RuntimeError(f"XLS benchmark command exceeded 120 seconds: {source.name}")
        time.sleep(0.002)
    _, stderr = process.communicate()
    sample = resident_bytes(process)
    peak = max(peak, sample or 0)
    peak_temporary = max(
        peak_temporary,
        directory_file_bytes(home / "tmp")
        + transient_output_bytes(output_root, output),
    )
    output_bytes = output.stat().st_size if output.is_file() else 0
    temporary = home / "tmp"
    report = json.loads(report_path.read_text(encoding="utf-8")) if report_path.is_file() else None
    return XlsObservation(
        elapsed_ms=(time.perf_counter() - started) * 1000,
        peak_rss_bytes=peak or None,
        returncode=process.returncode,
        output_bytes=output_bytes,
        output_sha256=file_sha256(output) if output_bytes else None,
        output_path=output,
        peak_temporary_bytes=peak_temporary,
        temporary_bytes_after=directory_file_bytes(temporary),
        residual_paths=output_residuals(
            output_root, output if process.returncode == 0 and output.is_file() else None
        ),
        report=report,
        stderr=stderr.decode("utf-8", errors="replace")[-2048:],
    )


def summarized(observations: list[Observation]) -> dict[str, object]:
    elapsed = [item.elapsed_ms for item in observations]
    peaks = [item.peak_rss_bytes for item in observations if item.peak_rss_bytes is not None]
    return {
        "runs": len(observations),
        "medianMillis": round(statistics.median(elapsed), 3),
        "maximumMillis": round(max(elapsed), 3),
        "peakRssBytes": max(peaks) if peaks else None,
        "exitCodes": sorted({item.returncode for item in observations}),
        "stdoutSha256": sorted({item.stdout_sha256 for item in observations}),
        "stderrSha256": sorted({item.stderr_sha256 for item in observations}),
    }


def individual_conversions(
    cli: pathlib.Path,
    fixtures: pathlib.Path,
    home: pathlib.Path,
    iterations: int,
) -> tuple[dict[str, object], list[str]]:
    report: dict[str, object] = {}
    failures: list[str] = []
    for format_name in FORMATS:
        observations = [
            observe(
                cli,
                [
                    "--no-config",
                    str(fixtures / f"normal.{format_name}"),
                    "--format",
                    format_name,
                    "--asset-mode",
                    "embed",
                    "--quiet",
                ],
                fixtures,
                home / f"{format_name}-{iteration}",
            )
            for iteration in range(iterations)
        ]
        summary = summarized(observations)
        report[format_name] = summary
        if summary["exitCodes"] != [0]:
            failures.append(f"{format_name} conversion did not succeed")
        if len(summary["stdoutSha256"]) != 1:
            failures.append(f"{format_name} output was not byte-deterministic")
    return report, failures


def batch_conversion(
    cli: pathlib.Path,
    fixtures: pathlib.Path,
    root: pathlib.Path,
    jobs: int,
) -> Observation:
    output = root / f"output-{jobs}"
    output.mkdir(parents=True)
    arguments = ["--no-config", *[str(fixtures / f"normal.{name}") for name in FORMATS]]
    arguments.extend(["--output-dir", str(output), "--jobs", str(jobs), "--quiet"])
    return observe(cli, arguments, fixtures, root / f"home-{jobs}")


def executable_report(
    cli: pathlib.Path,
    fixtures: pathlib.Path,
    root: pathlib.Path,
    iterations: int,
    parallelism: int,
) -> tuple[dict[str, object], list[str]]:
    cold = [observe(cli, ["--version"], fixtures, root / f"cold-{i}") for i in range(iterations)]
    conversions, failures = individual_conversions(cli, fixtures, root / "individual", iterations)
    serial = batch_conversion(cli, fixtures, root / "serial", 1)
    parallel = batch_conversion(cli, fixtures, root / "parallel", parallelism)
    if serial.returncode != 0:
        failures.append("serial batch conversion failed")
    if parallel.returncode != 0:
        failures.append("parallel batch conversion failed")
    return (
        {
            "coreExecutableBytes": cli.stat().st_size,
            "coldStart": summarized(cold),
            "individualConversions": conversions,
            "warmSerialBatch": summarized([serial]),
            "concurrentBatch": {
                **summarized([parallel]),
                "jobs": parallelism,
                "documentsPerSecond": round(len(FORMATS) / (parallel.elapsed_ms / 1000), 3),
            },
        },
        failures,
    )


def baseline_report(
    cli: pathlib.Path,
    fixtures: pathlib.Path,
    root: pathlib.Path,
    iterations: int,
) -> dict[str, object]:
    cold = [observe(cli, ["--version"], fixtures, root / f"cold-{i}") for i in range(iterations)]
    formats: dict[str, object] = {}
    for format_name in FORMATS:
        result = observe(
            cli,
            [
                "--no-config",
                str(fixtures / f"normal.{format_name}"),
                "--format",
                format_name,
                "--asset-mode",
                "embed",
                "--quiet",
            ],
            fixtures,
            root / format_name,
        )
        formats[format_name] = summarized([result])
    available = all(metric["exitCodes"] == [0] for metric in formats.values())
    note = (
        "The baseline Core converted the corpus directly; availability is recorded but latency is not compared across different parser implementations."
        if available
        else "The baseline Core required an optional runtime, so conversion latency is recorded only as availability evidence and is not compared to native conversion."
    )
    return {
        "coreExecutableBytes": cli.stat().st_size,
        "coldStart": summarized(cold),
        "legacyConversionAvailability": formats,
        "note": note,
    }


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
) -> dict[str, dict[str, object]]:
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
    items = decoded.get("items")
    if not isinstance(items, list) or len(items) != 56:
        raise SystemExit("legacy XLS oracle must describe exactly 56 valid files")
    indexed = {str(item.get("file")): item for item in items}
    if len(indexed) != len(items):
        raise SystemExit("legacy XLS oracle returned duplicate files")
    return indexed


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


def candidate_sheet(sheet_node: dict[str, object]) -> dict[str, object]:
    sheet = sheet_node.get("block", {}).get("data", {})
    values: dict[tuple[int, int], str] = {}
    merges: list[list[int]] = []
    rows_seen = 0
    columns_seen = 0
    provenance_order = True
    paged_row = 0
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
                    values[(row_index, column)] = text
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
                    values[(paged_row, column)] = decode_tsv_value(field)
                columns_seen = max(columns_seen, len(fields))
                paged_row += 1
            rows_seen = max(rows_seen, paged_row)
    return {
        "name": str(sheet.get("name", "")),
        "rows": rows_seen,
        "columns": columns_seen,
        "values": values,
        "merges": merges,
        "provenanceOrder": provenance_order,
    }


def cached_display(value: str) -> str | None:
    marker = " [cached: "
    if value.startswith("=") and marker in value and value.endswith("]"):
        return value.rsplit(marker, 1)[1][:-1]
    return None


def value_matches(expected: dict[str, object], actual: str) -> bool:
    cached = cached_display(actual)
    display = cached if cached is not None else actual
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
        candidate_sheet(node)
        for node in document.get("blocks", [])
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
    for index, expected_sheet in enumerate(oracle_sheets):
        if index >= len(candidate_sheets):
            break
        actual_sheet = candidate_sheets[index]
        row_cell_order = row_cell_order and bool(actual_sheet["provenanceOrder"])
        expected_merges = sorted(expected_sheet["merges"])
        actual_merges = sorted(actual_sheet["merges"])
        if actual_merges != expected_merges:
            merges_match = False
            errors.append(f"sheet {index} merged ranges differ from xlrd")
        for cell in expected_sheet["cells"]:
            expected_cells += 1
            kinds[str(cell["kind"])] += 1
            coordinate = (int(cell["row"]), int(cell["column"]))
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
        "verified": not errors and recall == 1.0 and merges_match and sheet_order and row_cell_order,
        "oracleSha256": digest,
        "expectedNonEmptyCells": expected_cells,
        "matchedNonEmptyCells": matched_cells,
        "recall": round(recall, 9),
        "valueKinds": kinds,
        "sheetOrder": sheet_order,
        "rowCellOrder": row_cell_order,
        "mergedRanges": merges_match,
        "errors": errors[:32],
    }


def summarize_xls_file(
    item: dict[str, object],
    observations: list[XlsObservation],
    oracle: dict[str, object] | None,
) -> dict[str, object]:
    successful = [
        observation
        for observation in observations
        if observation.returncode == 0 and observation.output_bytes > 0
    ]
    output_hashes = sorted(
        {observation.output_sha256 for observation in successful if observation.output_sha256}
    )
    structured_failures = []
    for observation in observations:
        report_items = observation.report.get("items") if observation.report else None
        report_item = report_items[0] if isinstance(report_items, list) and len(report_items) == 1 else None
        structured_failures.append(
            isinstance(report_item, dict)
            and report_item.get("status") == "failed"
            and report_item.get("errorCode") == "resourceLimit"
            and report_item.get("reasonCode") == "documentNodes"
            and isinstance(report_item.get("limit"), dict)
            and report_item["limit"].get("name") == "documentNodes"
            and observation.output_bytes == 0
            and not observation.residual_paths
            and observation.temporary_bytes_after == 0
        )
    hard_limit = not successful and len(observations) > 0 and all(structured_failures)
    quality = (
        verify_xls_oracle(successful[0].output_path, oracle)
        if successful and oracle is not None
        else None
    )
    return {
        "file": item["file"],
        "valid": item["valid"],
        "container": item["container"],
        "runs": len(observations),
        "successfulRuns": len(successful),
        "nonEmptyRuns": sum(observation.output_bytes > 0 for observation in observations),
        "deterministic": len(successful) == len(observations) and len(output_hashes) == 1,
        "hardLimit": hard_limit,
        "structuredDocumentNodesRuns": sum(structured_failures),
        "qualityOracle": quality,
        "meanMillis": round(statistics.mean(o.elapsed_ms for o in observations), 3),
        "maximumMillis": round(max(o.elapsed_ms for o in observations), 3),
        "peakRssBytes": max((o.peak_rss_bytes or 0) for o in observations) or None,
        "peakTemporaryBytes": max(o.peak_temporary_bytes for o in observations),
        "temporaryBytesAfter": max(o.temporary_bytes_after for o in observations),
        "residualPaths": sorted({path for o in observations for path in o.residual_paths}),
        "exitCodes": sorted({o.returncode for o in observations}),
        "outputSha256": output_hashes,
        "failureEvidence": sorted(
            {o.stderr.strip() for o in observations if o.returncode != 0 and o.stderr.strip()}
        ),
    }


def summarize_xls_corpus(
    cli: pathlib.Path,
    authority: list[dict[str, object]],
    observations_by_file: dict[str, list[XlsObservation]],
    oracle: dict[str, dict[str, object]] | None,
) -> dict[str, object]:
    items = []
    for item in authority:
        name = str(item["file"])
        observations = observations_by_file[name]
        items.append(summarize_xls_file(item, observations, oracle.get(name) if oracle else None))
    passed = {item["file"] for item in items if item["deterministic"]}
    valid = {str(item["file"]) for item in authority if item["valid"] is True}
    hard_limits = {item["file"] for item in items if item["valid"] and item["hardLimit"]}
    return {
        "binary": str(cli),
        "binarySha256": file_sha256(cli),
        "rawPass": {"passed": len(passed), "total": len(items)},
        "validPass": {"passed": len(passed & valid), "total": len(valid)},
        "validHardLimits": sorted(hard_limits),
        "maximumPeakRssBytes": max((item["peakRssBytes"] or 0) for item in items) or None,
        "maximumPeakTemporaryBytes": max(item["peakTemporaryBytes"] for item in items),
        "maximumTemporaryBytesAfter": max(item["temporaryBytesAfter"] for item in items),
        "items": items,
    }


def xls_corpus_report(
    baseline_cli: pathlib.Path,
    candidate_cli: pathlib.Path,
    corpus: pathlib.Path,
    authority: list[dict[str, object]],
    root: pathlib.Path,
    iterations: int,
    regression_limit: float,
    oracle: dict[str, dict[str, object]],
) -> tuple[dict[str, object], list[dict[str, object]]]:
    warm_item = next(item for item in authority if item["valid"] is True)
    warm_source = corpus / str(warm_item["file"])
    observe_xls(baseline_cli, warm_source, corpus, root / "warmup" / "baseline")
    observe_xls(candidate_cli, warm_source, corpus, root / "warmup" / "candidate")
    baseline_observations = {str(item["file"]): [] for item in authority}
    candidate_observations = {str(item["file"]): [] for item in authority}
    for file_index, item in enumerate(authority):
        name = str(item["file"])
        for iteration in range(iterations):
            order = (
                (
                    ("baseline", baseline_cli, baseline_observations),
                    ("candidate", candidate_cli, candidate_observations),
                )
                if (file_index + iteration) % 2 == 0
                else (
                    ("candidate", candidate_cli, candidate_observations),
                    ("baseline", baseline_cli, baseline_observations),
                )
            )
            for label, cli, destination in order:
                destination[name].append(
                    observe_xls(
                        cli,
                        corpus / name,
                        corpus,
                        root / "runs" / name / str(iteration) / label,
                    )
                )
    baseline = summarize_xls_corpus(
        baseline_cli, authority, baseline_observations, None
    )
    candidate = summarize_xls_corpus(
        candidate_cli, authority, candidate_observations, oracle
    )
    baseline_items = {item["file"]: item for item in baseline["items"]}
    candidate_items = {item["file"]: item for item in candidate["items"]}
    common = sorted(
        name
        for name in baseline_items.keys() & candidate_items.keys()
        if baseline_items[name]["deterministic"] and candidate_items[name]["deterministic"]
    )
    common_hash_matches = [
        name
        for name in common
        if baseline_items[name]["outputSha256"] == candidate_items[name]["outputSha256"]
    ]
    baseline_mean = statistics.mean(baseline_items[name]["meanMillis"] for name in common)
    candidate_mean = statistics.mean(candidate_items[name]["meanMillis"] for name in common)
    regression = candidate_mean / baseline_mean - 1
    valid_passed = int(candidate["validPass"]["passed"])
    valid_total = int(candidate["validPass"]["total"])
    hard_limits = len(candidate["validHardLimits"])
    oracle_verified = [
        item["file"]
        for item in candidate["items"]
        if item["valid"]
        and item["deterministic"]
        and item["qualityOracle"] is not None
        and item["qualityOracle"]["verified"]
    ]
    invalid_successes = [
        item["file"] for item in candidate["items"] if not item["valid"] and item["deterministic"]
    ]
    residuals = [item["file"] for item in candidate["items"] if item["residualPaths"]]
    checks = [
        {
            "name": "xls-valid-accounted",
            "passed": valid_passed == valid_total - 1
            and hard_limits == 1
            and valid_passed + hard_limits == valid_total,
            "detail": f"passed={valid_passed}, hardLimits={hard_limits}, total={valid_total}",
        },
        {
            "name": "xls-quality-oracle",
            "passed": len(oracle_verified) == valid_passed == 55,
            "detail": (
                f"verified={len(oracle_verified)}, successfulValid={valid_passed}, required=55"
            ),
        },
        {
            "name": "xls-invalid-inputs-fail-closed",
            "passed": not invalid_successes,
            "detail": f"invalidSuccesses={invalid_successes}",
        },
        {
            "name": "xls-common-success-byte-stability",
            "passed": len(common) == int(baseline["validPass"]["passed"])
            and len(common_hash_matches) == len(common),
            "detail": (
                f"common={len(common)}, byteIdentical={len(common_hash_matches)}, "
                f"baselineValid={baseline['validPass']['passed']}"
            ),
        },
        {
            "name": "xls-valid-coverage-not-regressed",
            "passed": valid_passed >= int(baseline["validPass"]["passed"]),
            "detail": (
                f"candidate={valid_passed}, baseline={baseline['validPass']['passed']}, "
                f"total={valid_total}"
            ),
        },
        {
            "name": "xls-common-success-mean-regression",
            "passed": regression < regression_limit,
            "detail": (
                f"common={len(common)}, baseline={baseline_mean:.3f}ms, "
                f"candidate={candidate_mean:.3f}ms, regression={regression:.3%}, "
                f"limit={regression_limit:.3%}"
            ),
        },
        {
            "name": "xls-peak-rss",
            "passed": candidate["maximumPeakRssBytes"] is not None
            and int(candidate["maximumPeakRssBytes"]) <= 2560 * MIB,
            "detail": (
                f"peak={candidate['maximumPeakRssBytes']}, "
                f"limit={2560 * MIB} (2 GiB shared budget + 512 MiB margin)"
            ),
        },
        {
            "name": "xls-peak-temporary-storage",
            "passed": int(candidate["maximumPeakTemporaryBytes"]) <= 2 * 1024 * MIB,
            "detail": (
                f"peak={candidate['maximumPeakTemporaryBytes']}, limit={2 * 1024 * MIB}"
            ),
        },
        {
            "name": "xls-temporary-cleanup",
            "passed": candidate["maximumTemporaryBytesAfter"] == 0 and not residuals,
            "detail": (
                f"maximumTemporaryBytesAfter={candidate['maximumTemporaryBytesAfter']}, "
                f"residualFiles={residuals}"
            ),
        },
    ]
    return (
        {
            "authority": "xlrd-2.0.2-eager-cell-read",
            "iterations": iterations,
            "measurementOrder": (
                "baseline and candidate warmed once, then alternated by file-and-iteration parity"
            ),
            "errorPolicy": "best-effort",
            "sharedLeaseTelemetry": {
                "status": "unavailable",
                "dependency": "#269",
                "evidence": (
                    "actual CompoundFile::open exact/limit-minus-one/cancellation/release tests; "
                    "process RSS is measured here"
                ),
            },
            "baseline": baseline,
            "candidate": candidate,
            "commonSuccess": {
                "files": common,
                "byteIdenticalFiles": common_hash_matches,
                "baselineMeanMillis": round(baseline_mean, 3),
                "candidateMeanMillis": round(candidate_mean, 3),
                "regressionFraction": round(regression, 6),
            },
        },
        checks,
    )


def gate(candidate: dict[str, object], baseline: dict[str, object]) -> tuple[list[dict[str, object]], bool]:
    checks: list[dict[str, object]] = []

    def add(name: str, passed: bool, detail: str) -> None:
        checks.append({"name": name, "passed": passed, "detail": detail})

    size = int(candidate["coreExecutableBytes"])
    baseline_size = int(baseline["coreExecutableBytes"])
    add(
        "core-executable-size",
        size <= baseline_size + 8 * MIB,
        f"candidate={size}, baseline={baseline_size}, allowedGrowth={8 * MIB}",
    )
    cold = float(candidate["coldStart"]["medianMillis"])
    baseline_cold = float(baseline["coldStart"]["medianMillis"])
    cold_limit = max(baseline_cold * 2, baseline_cold + 150)
    add("cold-start", cold <= cold_limit, f"candidate={cold:.3f}ms, limit={cold_limit:.3f}ms")
    for format_name in FORMATS:
        metric = candidate["individualConversions"][format_name]
        elapsed = float(metric["maximumMillis"])
        peak = metric["peakRssBytes"]
        add(
            f"{format_name}-latency",
            elapsed <= 2_000,
            f"maximum={elapsed:.3f}ms, limit=2000ms",
        )
        add(
            f"{format_name}-peak-rss",
            peak is not None and int(peak) <= 512 * MIB,
            f"peak={peak}, limit={512 * MIB}",
        )
    serial = float(candidate["warmSerialBatch"]["maximumMillis"])
    parallel = float(candidate["concurrentBatch"]["maximumMillis"])
    add(
        "concurrent-throughput",
        parallel <= serial * 2,
        f"parallel={parallel:.3f}ms, serial={serial:.3f}ms, limit={serial * 2:.3f}ms",
    )
    return checks, all(check["passed"] for check in checks)


def main() -> int:
    args = parse_args()
    if args.iterations < 3 or not 2 <= args.parallelism <= len(FORMATS):
        raise SystemExit("iterations must be >= 3 and parallelism must be between 2 and 3")
    xls_arguments = (args.xls_corpus, args.xls_authority, args.xls_xlrd_path)
    if any(value is not None for value in xls_arguments) and any(
        value is None for value in xls_arguments
    ):
        raise SystemExit(
            "--xls-corpus, --xls-authority, and --xls-xlrd-path must be supplied together"
        )
    if not 0 <= args.xls_regression_limit < 1:
        raise SystemExit("--xls-regression-limit must be in the range 0..1")
    baseline = args.baseline_cli.resolve(strict=True)
    candidate = args.candidate_cli.resolve(strict=True)
    fixtures = args.fixtures.resolve(strict=True)
    output = args.output.resolve(strict=False)
    if output.exists() or not output.parent.is_dir():
        raise SystemExit("output must be a new file in an existing directory")
    for format_name in FORMATS:
        if not (fixtures / f"normal.{format_name}").is_file():
            raise SystemExit(f"missing normal.{format_name}")
    xls_corpus = args.xls_corpus.resolve(strict=True) if args.xls_corpus else None
    xls_authority = args.xls_authority.resolve(strict=True) if args.xls_authority else None
    xls_xlrd_path = args.xls_xlrd_path.resolve(strict=True) if args.xls_xlrd_path else None
    authority = (
        load_xls_authority(xls_authority, xls_corpus)
        if xls_authority is not None and xls_corpus is not None
        else None
    )
    oracle_tool = pathlib.Path(__file__).with_name("legacy-xls-oracle.py").resolve(strict=True)
    oracle = (
        load_xls_oracle(
            oracle_tool,
            xls_xlrd_path,
            xls_corpus,
            xls_authority,
        )
        if xls_xlrd_path is not None
        and xls_corpus is not None
        and xls_authority is not None
        else None
    )
    with tempfile.TemporaryDirectory(prefix="into-md-office-performance-") as temporary:
        root = pathlib.Path(temporary)
        baseline_metrics = baseline_report(baseline, fixtures, root / "baseline", args.iterations)
        candidate_metrics, failures = executable_report(
            candidate, fixtures, root / "candidate", args.iterations, args.parallelism
        )
        xls_metrics, xls_checks = (
            xls_corpus_report(
                baseline,
                candidate,
                xls_corpus,
                authority,
                root / "xls-corpus",
                args.iterations,
                args.xls_regression_limit,
                oracle,
            )
            if xls_corpus is not None and authority is not None and oracle is not None
            else (None, [])
        )
    checks, checks_passed = gate(candidate_metrics, baseline_metrics)
    checks.extend(xls_checks)
    checks_passed = checks_passed and all(check["passed"] for check in xls_checks)
    report = {
        "schemaVersion": 2,
        "corpus": "repository-authored-office-97-2003",
        "baseline": baseline_metrics,
        "candidate": candidate_metrics,
        "xlsCorpus": xls_metrics,
        "gates": checks,
        "failures": failures,
        "passed": checks_passed and not failures,
    }
    temporary_output = output.with_name(f".{output.name}.tmp")
    temporary_output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    temporary_output.replace(output)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
