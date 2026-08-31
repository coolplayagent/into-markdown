#!/usr/bin/env python3
"""Verify pinned public text/HTML/source inputs against an installed or built CLI.

Network access occurs only with --fetch. Source files stay in the caller's cache;
the repository records immutable provenance and observations, not upstream code.
"""

import argparse
import hashlib
import json
from pathlib import Path
import platform
import re
import subprocess
import tempfile
import urllib.request


def digest(data):
    return hashlib.sha256(data).hexdigest()


def invoke(command, cwd):
    result = subprocess.run(command, cwd=cwd, capture_output=True, timeout=60, check=False)
    try:
        payload = json.loads(result.stdout)
    except (ValueError, UnicodeDecodeError):
        payload = None
    return result, payload


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--fetch", action="store_true")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--source-revision")
    parser.add_argument("--report", type=Path)
    parser.add_argument("--enforce", action="store_true")
    args = parser.parse_args()
    manifest_bytes = args.manifest.read_bytes()
    samples = json.loads(manifest_bytes)["samples"]
    for sample in samples:
        relative = Path(sample["kind"]) / sample["path"]
        if relative.is_absolute() or ".." in relative.parts:
            raise ValueError("unsafe corpus path")
        path = args.cache / relative
        if args.fetch and not path.exists():
            expected_url = f'https://raw.githubusercontent.com/{sample["repository"]}/{sample["revision"]}/{sample["path"]}'
            if sample["url"] != expected_url:
                raise ValueError("source URL disagrees with pinned provenance")
            with urllib.request.urlopen(expected_url, timeout=30) as response:
                data = response.read(sample["bytes"] + 1)
            if len(data) != sample["bytes"] or digest(data) != sample["sha256"]:
                raise ValueError(f"download hash mismatch: {relative}")
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        data = path.read_bytes()
        if len(data) != sample["bytes"] or digest(data) != sample["sha256"]:
            raise ValueError(f"cached hash mismatch: {relative}")
    if args.binary is None:
        print(f"Verified {len(samples)} pinned source files")
        return
    if args.report is None or args.source_revision is None:
        parser.error("--binary requires --report and --source-revision")
    binary = args.binary.resolve()
    records = []
    with tempfile.TemporaryDirectory(prefix="into-md-339-") as work:
        for sample in samples:
            source = (args.cache / sample["kind"] / sample["path"]).resolve()
            detect_command = [str(binary), "--no-config", "formats", "detect", "--json", str(source)]
            detection, candidates = invoke(detect_command, work)
            for policy in ["best-effort", "strict"]:
                command = [str(binary), "--no-config", "--error-policy", policy, "--ocr", "off", "--emit", "result-json", "--assets-dir", str(Path(work) / "assets"), str(source)]
                result, payload = invoke(command, work)
                stderr = result.stderr.decode("utf-8", errors="replace")
                markdown = payload.get("markdown", "") if isinstance(payload, dict) else ""
                expected = sample["expectedOutcome"]
                if expected == "converted":
                    passed = result.returncode == 0 and bool(markdown.strip()) and all(value in re.sub(r"\\([!\"#$%&'()*+,\-./:;<=>?@\[\]\\^_`{|}~])", r"\1", markdown) for value in sample.get("expectedContains", []))
                elif expected == "unsafeText":
                    passed = result.returncode != 0 and "noConverter" in stderr
                elif expected == "emptyHtml":
                    passed = result.returncode != 0 and "HTML contains no visible document content" in stderr
                else:
                    passed = result.returncode != 0 and "unsupported" in stderr and detection.returncode != 0
                record = {
                    "kind": sample["kind"], "path": sample["path"], "sha256": sample["sha256"],
                    "policy": policy, "expectedOutcome": expected, "passed": passed,
                    "detectCommand": detect_command, "detectExit": detection.returncode, "detection": candidates,
                    "command": command, "exit": result.returncode, "stderr": stderr,
                    "markdownBytes": len(markdown.encode()), "markdownSha256": digest(markdown.encode()),
                    "providers": sorted({item.get("provenance", {}).get("provider", "") for item in (payload or {}).get("document", {}).get("blocks", [])}),
                    "diagnostics": (payload or {}).get("diagnostics", []),
                }
                records.append(record)
                print(f'{sample["kind"]} {source.name} {policy}: exit={result.returncode} expected={passed}', flush=True)
    report = {"schemaVersion": 1, "platform": platform.platform(), "binarySha256": digest(binary.read_bytes()),
              "sourceRevision": args.source_revision, "manifestSha256": digest(manifest_bytes),
              "binaryVersion": subprocess.check_output([str(binary), "--version"], text=True).strip(),
              "records": records, "passed": sum(row["passed"] for row in records), "total": len(records)}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    if args.enforce and report["passed"] != report["total"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
