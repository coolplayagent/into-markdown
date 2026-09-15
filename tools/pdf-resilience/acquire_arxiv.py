#!/usr/bin/env python3
"""Explicitly acquire the versioned arXiv originals listed in a frozen manifest."""
from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import time
import urllib.request


def acquire(manifest, destination):
    papers = json.loads(manifest.read_text())["papers"]
    destination.mkdir(parents=True, exist_ok=True)
    errors = []
    for paper in papers:
        name = f'{paper["id"]}v{paper["version"]}.pdf'
        expected_url = f'https://arxiv.org/pdf/{paper["id"]}v{paper["version"]}'
        if paper["url"] != expected_url or "/" in name or ".." in name:
            raise ValueError("manifest contains an invalid arXiv source")
        target = destination / name
        try:
            if target.exists():
                payload = target.read_bytes()
            else:
                request = urllib.request.Request(expected_url, headers={
                    "User-Agent": "into-markdown-pdf-resilience/1 (explicit local regression)"})
                with urllib.request.urlopen(request, timeout=90) as response:
                    if not response.url.startswith("https://arxiv.org/pdf/"):
                        raise ValueError("unexpected source redirect")
                    payload = response.read(paper["bytes"] + 1)
                time.sleep(3)
            if len(payload) != paper["bytes"] or hashlib.sha256(payload).hexdigest() != paper["sha256"]:
                raise ValueError("source bytes differ from the frozen authority")
            if not target.exists():
                with target.open("xb") as output:
                    output.write(payload)
        except Exception as error:
            errors.append(dict(id=paper["id"], error=str(error)))
        print(paper["id"], "failed" if errors and errors[-1]["id"] == paper["id"] else "verified", flush=True)
    (destination / "download-report.json").write_text(json.dumps({
        "expected": len(papers), "verified": len(papers) - len(errors), "downloadErrors": errors,
    }, indent=2) + "\n")
    return bool(errors)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=pathlib.Path, required=True)
    parser.add_argument("--destination", type=pathlib.Path, required=True)
    parser.add_argument("--allow-network", action="store_true", required=True)
    args = parser.parse_args()
    raise SystemExit(acquire(args.manifest, args.destination))
