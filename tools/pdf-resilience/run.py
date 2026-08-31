#!/usr/bin/env python3
"""Black-box PDF regression checks for a source build or installed into-md."""
from __future__ import annotations

import argparse
import json
import os
import pathlib
import platform
import re
import subprocess
import time

from samples import SAMPLES, acquire, excerpt, sha256


def pdf(objects: list[bytes]) -> bytes:
    data = bytearray(b"%PDF-1.4\n")
    offsets = []
    for index, value in enumerate(objects, 1):
        offsets.append(len(data))
        data.extend(f"{index} 0 obj\n".encode() + value + b"\nendobj\n")
    xref = len(data)
    data.extend(f"xref\n0 {len(objects)+1}\n0000000000 65535 f \n".encode())
    for offset in offsets:
        data.extend(f"{offset:010} 00000 n \n".encode())
    data.extend(f"trailer\n<< /Size {len(objects)+1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode())
    return bytes(data)


def stream(data: bytes) -> bytes:
    return f"<< /Length {len(data)} >>\nstream\n".encode() + data + b"\nendstream"


def mixed_links() -> bytes:
    return pdf([
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R /Annots [6 0 R 7 0 R 8 0 R 9 0 R] >>",
        stream(b"BT /F1 12 Tf 10 200 Td (Retained public test body) Tj ET BT /F1 12 Tf 10 180 Td (Second text object) Tj ET"),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        b"<< /Type /Annot /Subtype /Link /Rect [10 100 80 120] /A << /S /URI /URI (https://example.test/valid) >> >>",
        b"<< /Type /Annot /Subtype /Link /Rect [10 120 80 100] /A << /S /URI /URI (https://example.test/reversed) >> >>",
        b"<< /Type /Annot /Subtype /Link /Rect [10 100 10 120] /A << /S /URI /URI (https://example.test/empty) >> >>",
        b"<< /Type /Annot /Subtype /Link /Rect [90 100 150 120] /Dest [3 0 R /Fit] >>",
    ])


def run_case(exe, root, env, name, inputs, expected=0, flags=()):
    output = root / f"{name}.md"
    command = [str(exe), "--no-config", *map(str, inputs), "--ocr", "off", "--log-format", "json", "--conflict", "overwrite", "--timeout-ms", "600000", *flags]
    command += ["--output", str(output)] if len(inputs) == 1 else ["--output-dir", str(root / name)]
    sentinel = b"Existing output must survive failed conversion\n"
    if expected != 0 and len(inputs) == 1:
        output.write_bytes(sentinel)
    started = time.monotonic()
    result = subprocess.run(command, env=env, capture_output=True, timeout=620)
    (root / f"{name}.stderr.jsonl").write_bytes(result.stderr)
    if result.returncode != expected:
        raise AssertionError(f"{name}: exit {result.returncode}, expected {expected}: {result.stderr[-2000:]!r}")
    if expected != 0 and len(inputs) == 1:
        assert output.read_bytes() == sentinel, "failure replaced the existing output"
        assert not (root / f"{name}_assets").exists()
    record = {"name": name, "exitCode": result.returncode, "seconds": round(time.monotonic() - started, 3), "arguments": command[1:]}
    if expected == 0 and len(inputs) == 1:
        text = output.read_text(encoding="utf-8")
        record.update(markdownBytes=output.stat().st_size, markdownSha256=sha256(output), pages=len(re.findall(r'<a id="pdf-page-\d+"></a>', text)))
    else:
        text = result.stderr.decode("utf-8")
    return record, text


def public_cases(exe, root, env, report):
    acquire(root)
    report["publicSamples"] = {name: {"url": url, "bytes": size, "sha256": digest} for name, (url, size, digest) in SAMPLES.items()}
    source = root / "Accenture_Humans_AI_Robots.pdf"
    record, text = run_case(exe, root, env, "accenture", [source], flags=["--max-memory-size", "8GiB"])
    assert record["pages"] == 29 and "Executive summary" in text and "https://" in text
    assert len(list((root / "accenture_assets").glob("*"))) > 0
    report["cases"].append(record)
    record, text = run_case(exe, root, env, "accenture-strict", [source], expected=3, flags=["--error-policy", "strict", "--asset-mode", "omit", "--max-memory-size", "8GiB"])
    assert "page 28" in text and "control character" in text
    report["cases"].append(record)
    selected = root / "CalculusVolume1-600.pdf"
    report["excerpt"] = excerpt(root / "CalculusVolume1-OP.pdf", selected)
    record, text = run_case(exe, root, env, "calculus600", [selected], flags=["--max-memory-size", "8GiB", "--max-pdf-layout-comparisons", "120000000", "--asset-mode", "omit"])
    assert record["pages"] == 600 and "Calculus" in text
    report["cases"].append(record)
    record, error = run_case(exe, root, env, "calculus-full-ir-boundary", [root / "CalculusVolume1-OP.pdf"], expected=5, flags=["--max-memory-size", "8GiB", "--asset-mode", "omit"])
    assert "documentInlines" in error
    report["cases"].append(record)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--into-md", type=pathlib.Path, required=True)
    parser.add_argument("--work-root", type=pathlib.Path, required=True)
    parser.add_argument("--pdfium-library", type=pathlib.Path)
    parser.add_argument("--public-samples", action="store_true", help="Download exact publisher PDFs and generate a local 600-page excerpt; requires pypdf 6.10.0")
    args = parser.parse_args()
    root, exe = args.work_root.resolve(), args.into_md.resolve()
    root.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    if args.pdfium_library:
        env["PDFIUM_LIBRARY"] = str(args.pdfium_library.resolve())
    version = subprocess.run([str(exe), "version", "--json", "--no-config"], env=env, capture_output=True, check=True)
    report = {"platform": platform.platform(), "version": json.loads(version.stdout), "binarySha256": sha256(exe), "cases": [], "status": "failed"}
    try:
        source = root / "中文 链接.pdf"
        source.write_bytes(mixed_links())
        record, text = run_case(exe, root, env, "mixed", [source])
        assert all(s in text for s in ["Retained public test body", "https://example.test/valid", "https://example.test/reversed", "#pdf-page-1"])
        assert "https://example.test/empty" not in text
        report["cases"].append(record)
        for name, flags, expected, reason in [
            ("strict", ["--error-policy", "strict"], 3, "annotation[2]"),
            ("page-budget", ["--max-pdf-page-objects", "1"], 5, "max_pdf_page_objects"),
            ("total-budget", ["--max-pdf-total-objects", "1"], 5, "max_pdf_total_objects"),
            ("exact-budget", ["--max-pdf-page-objects", "2", "--max-pdf-total-objects", "2"], 0, "Retained public test body"),
        ]:
            record, text = run_case(exe, root, env, name, [source], expected, flags)
            assert reason in text
            report["cases"].append(record)
        other = root / "第二个.pdf"
        other.write_bytes(mixed_links())
        record, _ = run_case(exe, root, env, "batch", [source, other])
        assert len(list((root / "batch").glob("*.md"))) == 2
        report["cases"].append(record)
        if args.public_samples:
            public_cases(exe, root, env, report)
        report["status"] = "passed"
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        (root / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"status": report["status"], "cases": len(report["cases"]), "report": str(root / "report.json")}))


if __name__ == "__main__":
    main()
