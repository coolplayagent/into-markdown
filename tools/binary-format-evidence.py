#!/usr/bin/env python3
"""Check packaged binary routing, body/assets, and native capability availability."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import zipfile


def digest(data):
    return hashlib.sha256(data).hexdigest()


def convert(binary, source, work, environment, ocr="off"):
    with tempfile.TemporaryDirectory(prefix="conversion-", dir=work) as output:
        command = [str(binary), "--no-config", "--ocr", ocr, "--emit", "result-json",
                   "--assets-dir", str(Path(output) / "assets"), str(source)]
        process = subprocess.run(command, cwd=output, env=environment,
                                 capture_output=True, timeout=120, check=False)
        try:
            result = json.loads(process.stdout)
        except ValueError:
            result = None
        return process, result, command


def detect(binary, source, environment):
    command = [str(binary), "--no-config", "formats", "detect", "--json", str(source)]
    output = subprocess.check_output(command, env=environment, timeout=30)
    return json.loads(output)["candidates"][0]["format"]


def fixtures(root, work, zip_cache):
    sources = [
        (root / "tools/macos-release/fixtures/normal.ppt", "ppt"),
        (root / "tools/macos-release/fixtures/normal.doc", "doc"),
        (root / "tools/macos-release/fixtures/normal.xls", "xls"),
        (root / "fixtures/small/pptx/normal.pptx", "pptx"),
        (root / "fixtures/small/docx/normal.docx", "docx"),
        (root / "fixtures/small/xlsx/normal.xlsx", "xlsx"),
        (root / "fixtures/small/pdf/structures.pdf", "pdf"),
        (root / "fixtures/small/ocr/ocr-english-clear-1.png", "image"),
    ]
    archive = work / "generated.zip"
    with zipfile.ZipFile(archive, "w") as package:
        package.writestr("kept.txt", "ZIP kept text")
        package.writestr("renamed.py", sources[3][0].read_bytes())
        package.writestr("rejected.js", "const source = 1;")
    sources.append((archive, "zip"))
    manifest = json.loads((root / "tools/archive-compat/samples.json").read_text())
    original = next(sample for sample in manifest["samples"]
                    if sample["name"] == "test_read_format_zip_filename_utf8_jp.zip")
    real_zip = zip_cache / original["name"]
    assert digest(real_zip.read_bytes()) == original["sha256"]
    sources.append((real_zip, "zip"))
    return sources, original


def public_fixtures(manifest, cache):
    samples = json.loads(manifest.read_text())["samples"]
    sources = []
    for sample in samples:
        path = cache / sample["name"]
        data = path.read_bytes()
        assert len(data) == sample["bytes"] and digest(data) == sample["sha256"]
        sources.append((path, sample["format"], sample.get("errorContains")))
    return sources, samples


def renamed_cases(binary, sources, work, environment):
    records = []
    for source_record in sources:
        source, expected = source_record[:2]
        boundary = source_record[2] if len(source_record) > 2 else None
        data = source.read_bytes()
        reference_process, reference, _ = convert(binary, source, work, environment)
        if boundary:
            assert reference_process.returncode != 0 and boundary in reference_process.stderr.decode()
        else:
            assert reference_process.returncode == 0, reference_process.stderr.decode()
            assert reference["markdown"].strip(), source
        suffixes = [source.suffix, ".md", ".csv", ".js", ".bin", ""]
        if expected == "zip":
            suffixes += [".docx", ".epub", ".rar"]
        for suffix in suffixes:
            path = work / f"{expected}-renamed{suffix}"
            path.write_bytes(data)
            selected = detect(binary, path, environment)
            process, result, command = convert(binary, path, work, environment)
            result = result or {}
            if boundary:
                passed = process.returncode != 0 and selected == expected and boundary in process.stderr.decode()
            else:
                passed = (process.returncode == 0 and selected == expected
                          and result.get("markdown") == reference["markdown"]
                          and len(result.get("assets", [])) == len(reference.get("assets", [])))
            records.append({
                "source": str(source), "sourceSha256": digest(data), "name": path.name,
                "command": command, "format": selected, "exit": process.returncode,
                "passed": passed, "expectedOutcome": "existingParserBoundary" if boundary else "converted",
                "markdownSha256": digest(result.get("markdown", "").encode()),
                "assets": len(result.get("assets", [])),
                "diagnostics": result.get("diagnostics", []), "stderr": process.stderr.decode(),
            })
            print(path.name, selected, passed, flush=True)
    return records


def native_cases(binary, root, work, environment):
    image = work / "ocr-renamed.md"
    image.write_bytes((root / "fixtures/small/ocr/ocr-english-clear-1.png").read_bytes())
    process, result, command = convert(binary, image, work, environment, "always")
    text = (result or {}).get("markdown", "")
    records = [{
        "name": image.name, "command": command, "exit": process.returncode,
        "passed": process.returncode == 0 and "conversion quality" in text.lower(),
        "markdown": text, "stderr": process.stderr.decode(),
        "diagnostics": (result or {}).get("diagnostics", []),
    }]
    audio = work / "audio-renamed.md"
    audio.write_bytes((root / "fixtures/small/audio/normal.wav").read_bytes())
    process, _, command = convert(binary, audio, work, environment)
    error = process.stderr.decode()
    records.append({
        "name": audio.name, "command": command, "format": detect(binary, audio, environment),
        "exit": process.returncode,
        "passed": (process.returncode != 0 and "componentUnavailable" in error
                   and "media transcription requires" in error),
        "stderr": error,
    })
    return records


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--zip-cache", type=Path, required=True)
    parser.add_argument("--public-manifest", type=Path, required=True)
    parser.add_argument("--public-cache", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    binary = args.binary.resolve()
    args.work.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.pop("PDFIUM_LIBRARY", None)
    with tempfile.TemporaryDirectory(prefix="binary-evidence-", dir=args.work.resolve()) as temporary:
        work = Path(temporary)
        sources, original = fixtures(root, work, args.zip_cache.resolve())
        public_sources, public_samples = public_fixtures(args.public_manifest, args.public_cache.resolve())
        records = renamed_cases(binary, sources + public_sources, work, environment)
        records += native_cases(binary, root, work, environment)
    report = {
        "schemaVersion": 1, "sourceRevision": args.source_revision,
        "binarySha256": digest(binary.read_bytes()),
        "binaryVersion": subprocess.check_output([str(binary), "--version"], text=True).strip(),
        "profile": "packaged native runtime with default config disabled",
        "realZipSource": original, "publicBinarySources": public_samples, "records": records,
        "passed": sum(record["passed"] for record in records), "total": len(records),
    }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(report["passed"], report["total"])
    assert report["passed"] == report["total"]


if __name__ == "__main__":
    main()
