#!/usr/bin/env python3
"""Replay hash-pinned arXiv originals through the installed CLI, without network."""
from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import os
import signal
import time
import urllib.parse
import zipfile
import io


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def strip_generated_html(text):
    """Remove rendering tags while retaining escaped text and comparisons."""
    return re.sub(
        r'(?<!\\)</?(?:a|sup|sub|br|table|thead|tbody|tr|td|th|div|span)'
        r'(?:\s+[^<>\n]*)?\s*/?>', '', text)


def nested_original_checks(source, case):
    parts = sorted({(d.get("locator") or {}).get("part")
        for item in case.get("items", []) for d in item.get("diagnostics", [])
        if d.get("code") == "conversion.recovery.originalFile"} - {None, ""})
    assets = {a["sha256"] for a in case.get("assets", [])
              if a["path"] in case.get("originalAttachmentPaths", [])}
    checks = []
    for part in parts:
        try:
            payload, remaining = source.read_bytes(), part
            while remaining:
                with zipfile.ZipFile(io.BytesIO(payload)) as archive:
                    names = [name for name in archive.namelist()
                             if remaining == name or remaining.startswith(name + "/")]
                    if not names:
                        raise ValueError("original member is absent from archive")
                    name = max(names, key=len)
                    payload = archive.read(name)
                    remaining = remaining[len(name):].lstrip("/")
            sha = hashlib.sha256(payload).hexdigest()
            checks.append(dict(part=part, sha256=sha, matched=sha in assets))
        except (OSError, ValueError, KeyError, zipfile.BadZipFile) as error:
            checks.append(dict(part=part, matched=False, error=str(error)))
    return checks


def inspect(markdown, expected_pages):
    text = markdown.read_text(encoding="utf-8")
    pages = [int(n) for n in re.findall(r'<a id="pdf-page-(\d+)"></a>', text)]
    missing = sorted(set(range(1, expected_pages + 1)) - set(pages))
    references = re.findall(r'(!?)\[[^\]\n]*\]\(<([^>]+)>\)', text)
    broken = []
    broken_internal = []
    source_links = []
    artifacts = {}
    for image, target in references:
        parsed = urllib.parse.urlsplit(target)
        if not parsed.scheme and not parsed.path and parsed.fragment.startswith(('pdf-page-', 'epub-spine-')):
            if parsed.fragment not in set(re.findall(r'<a id="([^"]+)"></a>', text)):
                broken_internal.append(target)
        if parsed.scheme or not parsed.path:
            continue
        resolved = markdown.parent / urllib.parse.unquote(parsed.path)
        if not resolved.is_file():
            generated = any(part.endswith("_assets") for part in pathlib.PurePosixPath(parsed.path).parts) or pathlib.PurePosixPath(parsed.path).name.startswith("asset-")
            if image or generated or parsed.path.lower().endswith(".pdf"):
                broken.append(target)
            else:
                source_links.append(target)
        else:
            artifacts[parsed.path] = dict(path=parsed.path, bytes=resolved.stat().st_size, sha256=digest(resolved))
    original_paths = []
    for target in re.findall(r'!?\[Original (?:PDF|source)[^\]\n]*\]\(<([^>]+)>\)', text):
        parsed = urllib.parse.urlsplit(target)
        if not parsed.scheme:
            original_paths.append(parsed.path)
    evidence = []
    sections = re.split(r'<a id="pdf-page-(\d+)"></a>', text)
    for index in range(1, len(sections), 2):
        page, body = int(sections[index]), sections[index + 1]
        images = len(re.findall(r'!\[.*?\]\(', body))
        original = len(re.findall(r'\]\(<[^>]*\.pdf(?:#[^>]*)?>\)', body))
        body = re.sub(r'^## Page \d+\s*$', '', body, flags=re.M)
        body = re.sub(r'!\[.*?\]\(<[^>]+>\)', '', body)
        body = re.sub(r'^PDF [^\n]*retained [^\n]*$', '', body, flags=re.M)
        body = strip_generated_html(body)
        evidence.append(dict(page=page, textChars=len(re.sub(r'\s', '', body)),
                             images=images, originalPdfReferences=original))
    empty = [p['page'] for p in evidence if not (p['textChars'] or p['images'] or p['originalPdfReferences'])]
    return dict(pages=pages, missingPages=missing, emptyPages=empty,
                duplicatePages=len(pages) != len(set(pages)), pageEvidence=evidence, brokenAssets=broken,
                brokenInternalLinks=broken_internal, unresolvedSourceLinks=source_links,
                assets=list(artifacts.values()), originalAttachmentPaths=original_paths,
                markdownBytes=markdown.stat().st_size, markdownSha256=digest(markdown))


def certified_empty(case):
    items = case.get("items", [])
    return (case.get("expectedPages") == 0 and case.get("exitCode") == 0
            and bool(items) and all(item.get("status") == "success"
            and item.get("outcome") == "complete" and item.get("reasonCode") == "emptySource"
            and not item.get("errorCode") and not item.get("warnings")
            and any(d.get("code") == "emptySource" and d.get("severity") == "info"
                    for d in item.get("diagnostics", [])) for item in items))


def validate_originals(source, paper, case):
    source_recovery = any(d.get("code") == "conversion.recovery.originalFile" and not (d.get("locator") or {}).get("part")
                          for item in case.get("items", []) for d in item.get("diagnostics", []))
    originals = [a for a in case.get("assets", [])
                 if a["path"] in case.get("originalAttachmentPaths", [])
                 and (paper.get('format', 'pdf') == 'pdf' or source_recovery)]
    case["originalHashMismatch"] = [a["path"] for a in originals if a["sha256"] != paper["sha256"]]
    case["originalFileFallback"] = source_recovery and bool(originals) and not case["originalHashMismatch"]
    case["nestedOriginalChecks"] = nested_original_checks(source, case)
    global_original = any(d.get("code") == "pdf.recovery.originalPdf" and not (d.get("locator") or {}).get("page")
                          for item in case.get("items", []) for d in item.get("diagnostics", []))
    case["documentOriginalFallback"] = (case["originalFileFallback"]
        or (global_original and bool(originals) and not case["originalHashMismatch"]))
    return source_recovery


def run(args):
    manifest = json.loads(args.manifest.read_text())
    exe = args.into_md.resolve()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=True)
    version = json.loads(subprocess.check_output([str(exe), "--no-config", "version", "--json"]))
    report = dict(schemaVersion=1, manifestSha256=digest(args.manifest),
                  binarySha256=digest(exe), version=version,
                  watchdogSeconds=args.watchdog_seconds, modes=list(args.modes), cases=[])
    configs = getattr(args, "config", [])
    report["configurations"] = [{"path": str(p.resolve()), "sha256": digest(p)} for p in configs]
    prior_path = root / "report.json"
    if args.resume and prior_path.exists():
        prior = json.loads(prior_path.read_text())
        for key in ("manifestSha256", "binarySha256", "watchdogSeconds", "modes", "configurations"):
            if prior.get(key) != report[key]:
                raise ValueError(f"resume authority mismatch: {key}")
        report = prior
    finished = {(case["id"], case["ocr"]) for case in report["cases"]}
    for paper in manifest["papers"]:
        source = args.sources / (paper['sourcePath'] if 'sourcePath' in paper
                                 else f'{paper["id"]}v{paper["version"]}.pdf')
        if source.stat().st_size != paper["bytes"] or digest(source) != paper["sha256"]:
            raise ValueError(f"source authority mismatch: {source}")
        for mode in args.modes:
            if (paper["id"], mode) in finished:
                continue
            case_root = root / paper["id"] / mode
            case_root.mkdir(parents=True, exist_ok=True)
            output = case_root / "document.md"
            batch_report = case_root / "conversion.json"
            command = [str(exe), "--no-config", "--error-policy", "best-effort",
                       "--conflict", "error", "--log-format", "json",
                       "--report", str(batch_report), "--output", str(output), str(source)]
            for config in configs:
                command.extend(["--config", str(config.resolve())])
            if mode == "off":
                command += ["--ocr", "off"]
            execution = command
            rss_report = case_root / "process-resources.txt"
            if sys.platform == "darwin" and pathlib.Path("/usr/bin/time").exists():
                execution = ["/usr/bin/time", "-l", "-o", str(rss_report), *command]
            start = time.monotonic()
            timed_out = False
            with (case_root / "stderr.jsonl").open("wb") as stderr:
                try:
                    process = subprocess.Popen(execution, stdout=subprocess.DEVNULL, stderr=stderr,
                                               start_new_session=os.name == "posix")
                    code = process.wait(timeout=args.watchdog_seconds)
                except subprocess.TimeoutExpired:
                    timed_out, code = True, None
                    if os.name == "posix":
                        os.killpg(process.pid, signal.SIGKILL)
                    else:
                        process.kill()
                    process.wait()
            case = dict(id=paper["id"], ocr=mode, expectedPages=paper["pages"],
                        exitCode=code, watchdogExpired=timed_out,
                        seconds=round(time.monotonic() - start, 3), arguments=command[1:],
                        sourceSha256=paper["sha256"])
            if rss_report.exists():
                rss = re.search(r"(\d+)\s+maximum resident set size", rss_report.read_text())
                if rss:
                    case["peakRssBytes"] = int(rss[1])
            if batch_report.exists():
                conversion = json.loads(batch_report.read_text())
                case["items"] = conversion["items"]
                case["resourceUsage"] = conversion.get("resourceUsage")
            if output.exists():
                case.update(inspect(output, paper["pages"]))
            source_recovery = validate_originals(source, paper, case)
            case["uncoveredPages"] = ([] if case["documentOriginalFallback"] else case.get("missingPages", ["no output"]))
            case["certifiedEmptySource"] = certified_empty(case)
            case["passed"] = (code == 0 and (case.get("markdownBytes", 0) > 0 or case["certifiedEmptySource"])
                              and not case["uncoveredPages"]
                              and not case.get("emptyPages", ["no output"])
                              and not case.get("duplicatePages", True)
                              and not case.get("brokenAssets", ["no output"])
                              and not case.get("brokenInternalLinks", ["no output"])
                              and not case["originalHashMismatch"]
                              and all(check["matched"] for check in case["nestedOriginalChecks"])
                              and (not source_recovery or case["originalFileFallback"]))
            report["cases"].append(case)
            report["passed"] = sum(c["passed"] for c in report["cases"])
            report["failed"] = len(report["cases"]) - report["passed"]
            report["complete"] = len(report["cases"]) == len(manifest["papers"]) * len(args.modes)
            temporary = prior_path.with_suffix(".json.tmp")
            temporary.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
            temporary.replace(prior_path)
            print(f'{len(report["cases"])} {paper["id"]} {mode}: '
                  f'exit={code} passed={case["passed"]} {case["seconds"]}s', flush=True)
    return 0 if report["complete"] and not report["failed"] else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=pathlib.Path, required=True)
    parser.add_argument("--sources", type=pathlib.Path, required=True)
    parser.add_argument("--into-md", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--config", type=pathlib.Path, action="append", default=[])
    parser.add_argument("--modes", nargs="+", choices=["auto", "off"], default=["auto", "off"])
    parser.add_argument("--watchdog-seconds", type=int, default=660)
    raise SystemExit(run(parser.parse_args()))
