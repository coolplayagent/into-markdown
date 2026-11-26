"""Exercise an installed Windows archive through dsh's own ACL runner (opt-in)."""

import argparse
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import posixpath
import re
import shutil
import subprocess
import sys
import unicodedata
import uuid
from urllib.parse import unquote
import xml.etree.ElementTree as ET
import zipfile

from ocr_smoke import FIXTURE_ID, ROOT, recognized_text, verify_result


def require(condition, message):
    if not condition:
        raise ValueError(message)


def character_errors(reference, actual):
    def normalize(text):
        return "".join(unicodedata.normalize("NFC", text).split())

    reference, actual = normalize(reference), normalize(actual)
    previous = list(range(len(actual) + 1))
    for row, left in enumerate(reference, 1):
        current = [row]
        for column, right in enumerate(actual, 1):
            current.append(min(previous[column] + 1, current[-1] + 1,
                               previous[column - 1] + (left != right)))
        previous = current
    return previous[-1], len(reference)


def slide_image_digests(archive):
    drawings = "{http://schemas.openxmlformats.org/drawingml/2006/main}"
    office = "{http://schemas.openxmlformats.org/officeDocument/2006/relationships}"
    parts = set()
    for name in archive.namelist():
        if not re.fullmatch(r"ppt/slides/slide[0-9]+\.xml", name):
            continue
        parent, slide = posixpath.split(name)
        used = {node.get(office + "embed") for node in ET.fromstring(archive.read(name)).iter(drawings + "blip")}
        rels = f"{parent}/_rels/{slide}.rels"
        if rels in archive.namelist():
            for rel in ET.fromstring(archive.read(rels)):
                if rel.get("Id") in used and rel.get("TargetMode") != "External":
                    target = posixpath.join(parent, rel.attrib["Target"])
                    parts.add(posixpath.normpath(target).lstrip("/"))
    return {hashlib.sha256(archive.read(part)).digest() for part in parts}


def bounded_edits(left, right, limit):
    """Exact character distance within the configured tolerance, linear for exact matches."""
    if left == right:
        return 0
    if not limit or abs(len(left) - len(right)) > limit:
        return limit + 1
    previous = {i: i for i in range(min(len(right), limit) + 1)}
    for row, character in enumerate(left, 1):
        current = {0: row} if row <= limit else {}
        for column in range(max(1, row - limit), min(len(right), row + limit) + 1):
            current[column] = min(previous.get(column, limit + 1) + 1,
                                  current.get(column - 1, limit + 1) + 1,
                                  previous.get(column - 1, limit + 1) + (character != right[column - 1]))
        if min(current.values(), default=limit + 1) > limit:
            return limit + 1
        previous = current
    return previous.get(len(right), limit + 1)


class Acceptance:
    def __init__(self, binary, dsh, work):
        self.binary = binary.resolve(strict=True)
        dsh = dsh.resolve(strict=True)
        self.runner = [str(dsh), "--expose-internals", str(
            dsh.parent / "resources/app.asar/dsh/node_modules"
            / "@deepseek-ai/dsh-sandbox-windows-acl/lib/runner.js")]
        self.work = work
        self.failures = []

    def check(self, label, operation):
        try:
            operation()
            print(f"PASS {label}", flush=True)
        except (ValueError, KeyError, OSError, subprocess.TimeoutExpired) as exc:
            self.failures.append(label)
            print(f"FAIL {label}: {exc}", flush=True)

    def run(self, args, *, mode="workspace-write", status=0, stdin=b"", timeout=180,
            environment=None, binary=None):
        command = [str(binary or self.binary), *map(str, args)]
        env = os.environ.copy()
        if environment:
            env.update(environment)
        if mode is not None:
            command = [*self.runner, "--workspace", str(self.work), "--temp",
                       env["TEMP"], "--mode", mode, "--", *command]
            env["ELECTRON_RUN_AS_NODE"] = "1"
        result = subprocess.run(command, cwd=self.work, env=env, input=stdin,
                                capture_output=True, timeout=timeout)
        require(result.returncode == status, f"exit {result.returncode} (expected {status}): "
                + result.stderr.decode("utf-8", "replace")[-1000:])
        return result.stdout

    def commands(self):
        for args in [
            ["doctor", "--json"], ["doctor", "--deep", "--json"], ["formats", "--json"],
            ["capabilities", "list", "--json"], ["plugins", "--json"], ["providers", "--json"],
            ["config", "show", "--format", "json"], ["version", "--json"],
        ]:
            def inspect(args=args):
                result = json.loads(self.run(args))
                if args[0] == "doctor":
                    status = {item["id"]: item["status"] for item in result}
                    for key in ["configuration", "runtime.pdfium", "runtime.ocr"]:
                        require(status.get(key) == "ok", f"{key} reported {status.get(key)}")
            self.check(" ".join(args), inspect)
        for mode in ["workspace-write", "read-only"]:
            def verify_ocr(mode=mode):
                result = json.loads(self.run(["capabilities", "verify", "ocr", "--json"], mode=mode))
                require(result.get("status") == "ready", f"OCR verification reported {result.get('status')}")
                self.run(["setup", "ocr"], mode=mode)
            self.check("verify bundled OCR in " + mode, verify_ocr)

    def outputs(self):
        sample = self.work / "sample.txt"
        sample.write_text("workspace text\n", encoding="utf-8")
        dest = self.work / "sample.md"
        self.run([sample, "-o", dest])
        require(dest.read_text().strip() == "workspace text", "wrong single-file output")
        sample.write_text("updated workspace text\n", encoding="utf-8")
        self.run([sample, "-o", dest, "--conflict", "error"], status=4)
        require(dest.read_text().strip() == "workspace text", "conflicting output changed")
        self.run([sample, "-o", dest, "--conflict", "overwrite"])
        require(dest.read_text().strip() == "updated workspace text", "overwrite failed")
        forbidden = self.work / "readonly.md"
        self.run([sample, "-o", forbidden], mode="read-only", status=4)
        require(not forbidden.exists(), "read-only produced a file")
        require(self.run([sample], mode="read-only").strip() == b"updated workspace text",
                "read-only stdout conversion failed")
        require(self.run(["-", "--extension", "txt"], stdin=b"standard input").strip()
                == b"standard input", "stdin conversion failed")

    def read_only(self):
        pdf = ROOT / "fixtures/small/pdf/titled-table.pdf"
        args = [pdf, "--ocr", "off", "--asset-mode", "omit", "--error-policy", "strict"]
        text = self.run(args, mode=None)
        require(text and self.run(args, mode="read-only") == text, "read-only PDF text differs")
        image = (ROOT / "fixtures/small/ocr/ocr-english-clear-1.png").read_bytes()
        png = base64.b64encode(image).decode("ascii")
        notebook = self.work / "image.ipynb"
        output = {"output_type": "display_data", "data": {"image/png": png}, "metadata": {}}
        cell = {"id": "image", "cell_type": "code", "metadata": {}, "source": ["1"],
                "execution_count": 1, "outputs": [output]}
        notebook.write_text(json.dumps({"nbformat": 4, "nbformat_minor": 5, "metadata": {},
                                        "cells": [cell]}), encoding="utf-8")
        for emit in ["markdown", "result-json", "bundle"]:
            args = [notebook, "--ocr", "off", "--emit", emit]
            if emit != "bundle":
                args.extend(["--asset-mode", "embed"])
            data = self.run(args, mode="read-only")
            require(data == self.run(args, mode=None), f"read-only {emit} differs from host")
            if emit == "bundle":
                with zipfile.ZipFile(io.BytesIO(data)) as bundle:
                    require(bundle.testzip() is None and any(bundle.read(name) == image
                            for name in bundle.namelist()), "ZIP is missing exact PNG bytes")
            else:
                require(png.encode() in data, f"{emit} is missing the PNG")
        rejected = self.run([notebook, "--emit", "bundle", "--ocr", "off", "--max-memory-size",
                             "8KiB"], mode="read-only", status=5)
        require(not rejected, "over-budget conversion published partial stdout")

    def formats(self):
        inputs = self.work / "formats"
        inputs.mkdir()
        small = ROOT / "fixtures/small"
        fixtures = [p for p in sorted(small.glob("*/normal.*"))
                    if p.parent.name not in {"audio", "video"}]
        fixtures.append(small / "pdf/structures.pdf")
        for src in fixtures:
            shutil.copyfile(src, inputs / (src.parent.name + src.suffix))
        for src in (ROOT / "tools/macos-release/fixtures").rglob("normal.*"):
            shutil.copyfile(src, inputs / ("legacy-" + src.suffix[1:] + src.suffix))
        paths = sorted(inputs.iterdir())
        for mode, label in [(None, "host"), ("workspace-write", "sandbox")]:
            report_path = self.work / (label + "-formats.json")
            dest = self.work / (label + "-formats")
            self.run([inputs, "--recursive", "--output-dir", dest, "--asset-mode", "omit",
                      "--ocr", "off", "--jobs", "4", "--report", report_path], mode=mode)
            report = json.loads(report_path.read_bytes())
            require(report["failed"] == 0 and report["succeeded"] == len(paths),
                    f"{label} failed to convert all {len(paths)} core fixtures")
        for src in paths:
            name = src.stem + ".md"
            actual = (self.work / "sandbox-formats" / name).read_bytes()
            require(actual and actual == (self.work / "host-formats" / name).read_bytes(),
                    f"host/sandbox outputs differ for {src.name}")
            read_only = self.run([src, "--asset-mode", "omit", "--ocr", "off"], mode="read-only")
            require(read_only == actual, f"read-only stdout differs for {src.name}")
            piped = self.run(["-", "--extension", src.suffix[1:], "--asset-mode", "omit", "--ocr", "off"],
                             mode="read-only", stdin=src.read_bytes())
            require(piped == actual, f"read-only stdin differs for {src.name}")
        print(f"  {len(paths)} fixture outputs match in both dsh modes", flush=True)

    def ocr(self):
        manifest = json.loads((ROOT / "fixtures/manifest.json").read_bytes())
        fixture = next(f for f in manifest["fixtures"] if f["id"] == FIXTURE_ID)
        golden = next(g for g in manifest["ocr_quality"]["goldens"] if g["fixture_id"] == FIXTURE_ID)
        src = ROOT / "fixtures" / fixture["path"]
        require(hashlib.sha256(src.read_bytes()).hexdigest() == golden["fixture_sha256"],
                "fixture differs from quality manifest")
        cache = self.work / "unavailable-cache"
        cache.write_bytes(b"regular file")
        for name in ["first", "repeat"]:
            report = self.work / (name + "-ocr.json")
            result = json.loads(self.run([src, "--ocr", "always", "--error-policy", "strict",
                "--no-config", "--max-memory-size", "2GiB", "--timeout-ms", "120000",
                "--emit", "result-json", "--asset-mode", "omit", "--report", report],
                environment={"LOCALAPPDATA": str(cache)}))
            verify_result(result, json.loads(report.read_bytes()), golden["ground_truth_nfc"], ValueError)
        require(cache.read_bytes() == b"regular file", "conversion modified the cache sentinel")

        # Executables installed in the workspace inherit its low integrity label.
        # A direct launch can have low integrity without having restricting SIDs.
        installed = self.work / "into-md.exe"
        shutil.copyfile(self.binary, installed)
        for mode in (None, "workspace-write"):
            result = json.loads(self.run([src, "--ocr", "always", "--error-policy", "strict",
                "--no-config", "--emit", "result-json", "--asset-mode", "omit"],
                binary=installed, mode=mode))
            require(golden["ground_truth_nfc"] in recognized_text(result["document"]),
                    "workspace-installed executable lost OCR text")

    def extracted_assets(self):
        installed = self.work / "into-md.exe"
        source = self.work / "image.ipynb"
        original = (ROOT / "fixtures/small/ocr/ocr-english-clear-1.png").read_bytes()
        for emit in ("markdown", "ir-json", "result-json"):
            destination = self.work / ("extracted-" + emit)
            destination.mkdir()
            report = destination / "report.json"
            primary = destination / "output"
            self.run([source, "--ocr", "off", "--emit", emit, "--asset-mode", "extract",
                      "-o", primary, "--report", report], binary=installed, mode=None)
            require(primary.stat().st_size and json.loads(report.read_bytes())["succeeded"] == 1,
                    f"workspace-installed {emit} or report was not published")
            require([p.read_bytes() for p in destination.rglob("*.png")] == [original],
                    f"workspace-installed {emit} lost its PNG")

    def languages(self):
        manifest = json.loads((ROOT / "fixtures/manifest.json").read_bytes())
        fixtures = {f["id"]: ROOT / "fixtures" / f["path"] for f in manifest["fixtures"]}
        goldens = manifest["ocr_quality"]["goldens"]
        texts = {}
        for backend, mode in [("host", None), ("sandbox", "workspace-write")]:
            dest = self.work / (backend + "-language-results")
            report = self.work / (backend + "-language-report.json")
            self.run([*(fixtures[g["fixture_id"]] for g in goldens), "--output-dir", dest,
                      "--emit", "result-json", "--asset-mode", "omit", "--ocr", "always",
                      "--error-policy", "strict", "--jobs", "2", "--report", report],
                     timeout=600, mode=mode)
            stats = json.loads(report.read_bytes())
            require(stats["succeeded"] == len(goldens) and stats["failed"] == 0,
                    f"{backend} failed to convert some OCR fixtures")
            texts[backend] = {Path(item["input"]).stem: recognized_text(
                json.loads((self.work / item["output"]).read_bytes())["document"])
                for item in stats["items"]}
        require(texts["host"] == texts["sandbox"], "sandbox OCR text differs from native OCR")
        misses = []
        for golden in goldens:
            actual = texts["sandbox"][golden["fixture_id"]]
            require(actual, f"no OCR contribution for {golden['fixture_id']}")
            errors, length = character_errors(golden["ground_truth_nfc"], actual)
            if errors > length * golden["maximum_cer"]:
                misses.append(f"{golden['fixture_id']} ({errors}/{length})")
        print(f"  host parity {len(goldens)}/{len(goldens)}; above published error limit "
              + (", ".join(misses) or "none"), flush=True)

    def presentation(self, original, tolerance):
        # Separate workspaces with matching output names keep asset links comparable.
        src = self.work / "deck.pptx"
        shutil.copyfile(original, src)
        source_hash = hashlib.sha256(src.read_bytes()).digest()
        outcomes = []
        with zipfile.ZipFile(src) as archive:
            source_assets = slide_image_digests(archive)
        for mode, label in [(None, "host"), ("workspace-write", "sandbox")]:
            dest = self.work / (label + "-deck")
            dest.mkdir()
            md, report = dest / "deck.md", dest / "report.json"
            self.run([src, "--ocr", "auto", "-o", md, "--report", report],
                     mode=mode, timeout=900)
            result = json.loads(report.read_bytes())
            usage = result["resourceUsage"]
            stats = usage["ocrRuntime"]
            body = md.read_text(encoding="utf-8")
            require(result["failed"] == 0 and stats["imagesAttempted"] > 0
                    and stats["imagesCompleted"] == stats["imagesAttempted"]
                    and stats["imagesFailed"] == 0 and usage["ocr"]["recognizedChars"] > 0,
                    f"{label} OCR: {stats}, recognizedChars={usage['ocr']['recognizedChars']}")
            require(not re.search(r"<strong>\*\*[^<]*?\*\*</strong>", body),
                    f"{label} contains redundant bold markup")
            refs = {unquote(ref) for ref in re.findall(r"\]\(<([^>]+)>\)", body)
                    if unquote(ref).startswith("deck_assets/")}
            assets = {hashlib.sha256((dest / ref).read_bytes()).digest() for ref in refs}
            require(assets == source_assets, f"{label} did not link every original embedded asset")
            native = self.run([src, "--ocr", "off", "--asset-mode", "omit"], mode=mode)
            outcomes.append((body, native, stats["imagesCompleted"], stats["imagesWithText"]))
            print(f"  {label}: {stats['imagesCompleted']} completed, {stats['imagesWithText']} with text, "
                  f"{usage['ocr']['recognizedChars']} recognized characters, {len(refs)} assets", flush=True)
        host, sandbox = outcomes
        require(host[1:] == sandbox[1:], "native-text Markdown or OCR image coverage differs")
        edits = bounded_edits(host[0], sandbox[0], tolerance)
        require(edits <= tolerance, f"OCR-enriched Markdown exceeds the {tolerance}-edit tolerance")
        print(f"  {edits} character edits in {len(host[0])} characters", flush=True)
        require(hashlib.sha256(src.read_bytes()).digest() == source_hash, "source changed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--dsh", required=True, type=Path, help="installed DeepSeek Harness.exe")
    parser.add_argument("--workspace-parent", required=True, type=Path,
                        help="dsh-writable parent; results stay in a new subdirectory")
    parser.add_argument("--compare-pptx", type=Path,
                        help="optional image-heavy PPTX with distinct embedded media; kept local")
    parser.add_argument("--max-ocr-edits", type=int, default=0,
                        help="allowed character edits in OCR-enriched PPTX Markdown (default: exact)")
    args = parser.parse_args()
    if not 0 <= args.max_ocr_edits <= 256:
        parser.error("--max-ocr-edits must be between 0 and 256")
    work = args.workspace_parent.resolve(strict=True) / ("into-md-dsh-" + uuid.uuid4().hex[:12])
    work.mkdir()
    print(f"Evidence: {work}", flush=True)
    suite = Acceptance(args.binary, args.dsh, work)
    suite.commands()
    suite.check("output transactions and read-only stdout", suite.outputs)
    suite.check("read-only PDF and asset-bearing stdout", suite.read_only)
    suite.check("local format corpus", suite.formats)
    suite.check("first and repeated real OCR", suite.ocr)
    suite.check("workspace-installed file outputs, reports and extracted assets", suite.extracted_assets)
    suite.check("English, simplified/traditional Chinese and mixed OCR", suite.languages)
    if args.compare_pptx:
        suite.check("image-rich presentation", lambda: suite.presentation(args.compare_pptx, args.max_ocr_edits))
    return int(bool(suite.failures))


if __name__ == "__main__":
    sys.exit(main())
