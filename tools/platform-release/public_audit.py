#!/usr/bin/env python3
"""Audit GitHub issue/PR metadata, Actions logs, and retained artifacts before publication."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import pathlib
import re
import subprocess
import time
import urllib.error
import urllib.request
import zipfile


def request(url: str, token: str) -> bytes:
    value = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    last: Exception | None = None
    for attempt in range(4):
        try:
            with urllib.request.urlopen(value, timeout=120) as response:
                return response.read()
        except urllib.error.HTTPError as error:
            if error.code < 500 or attempt == 3:
                raise
            last = error
        except urllib.error.URLError as error:
            if attempt == 3:
                raise
            last = error
        time.sleep(2**attempt)
    raise RuntimeError(f"GitHub request failed: {last}")


def pages(repository: str, endpoint: str, token: str) -> list[dict]:
    result: list[dict] = []
    page = 1
    while True:
        separator = "&" if "?" in endpoint else "?"
        payload = json.loads(request(f"https://api.github.com/repos/{repository}/{endpoint}{separator}per_page=100&page={page}", token))
        values = (
            payload.get("artifacts", payload.get("workflow_runs", []))
            if isinstance(payload, dict)
            else payload
        )
        if not values:
            return result
        result.extend(values)
        if len(values) < 100:
            return result
        page += 1


def extract(archive: pathlib.Path, destination: pathlib.Path) -> None:
    with zipfile.ZipFile(archive) as source:
        for item in source.infolist():
            path = pathlib.PurePosixPath(item.filename)
            if path.is_absolute() or ".." in path.parts:
                raise RuntimeError(f"unsafe GitHub ZIP entry: {item.filename}")
        source.extractall(destination)


def scan(gitleaks: pathlib.Path, source: pathlib.Path, report: pathlib.Path) -> None:
    configuration = pathlib.Path.cwd() / ".gitleaks.toml"
    process = subprocess.run(
        [str(gitleaks), "dir", "--config", str(configuration), "--redact", "--no-banner", "--report-format", "json", "--report-path", str(report), str(source)],
        text=True,
    )
    if process.returncode:
        raise RuntimeError(f"secret findings were written to {report}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository", required=True)
    parser.add_argument("--gitleaks", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    arguments = parser.parse_args()
    token = os.environ.get("GITHUB_TOKEN")
    if not token:
        raise SystemExit("GITHUB_TOKEN is required")
    arguments.output.mkdir(parents=True, exist_ok=False)
    metadata = arguments.output / "github-metadata"
    metadata.mkdir()
    issues = pages(arguments.repository, "issues?state=all", token)
    issue_text = json.dumps(issues, sort_keys=True)
    (metadata / "issues-and-pulls.json").write_text(issue_text, encoding="utf-8")
    attachments = sorted(set(re.findall(r"https://github\.com/user-attachments/assets/[0-9a-fA-F-]+", issue_text)))
    attachment_root = arguments.output / "issue-attachments"
    attachment_root.mkdir()
    for index, url in enumerate(attachments):
        (attachment_root / f"attachment-{index}").write_bytes(request(url, token))
    runs = pages(arguments.repository, "actions/runs", token)
    retained = arguments.output / "retained-github-data"
    retained.mkdir()
    def download_run(run: dict) -> None:
        destination = retained / f"run-{run['id']}"
        destination.mkdir()
        try:
            archive = destination / "logs.zip"
            archive.write_bytes(request(run["logs_url"], token))
            extract(archive, destination / "logs")
            archive.unlink()
        except urllib.error.HTTPError as error:
            if error.code not in {404, 410}:
                raise
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
        list(executor.map(download_run, runs))
    artifacts = pages(arguments.repository, "actions/artifacts", token)
    def download_artifact(artifact: dict) -> None:
        if artifact.get("expired"):
            return
        destination = retained / f"artifact-{artifact['id']}"
        destination.mkdir()
        archive = destination / "artifact.zip"
        archive.write_bytes(request(artifact["archive_download_url"], token))
        extract(archive, destination / "content")
        archive.unlink()
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
        list(executor.map(download_artifact, artifacts))
    scan(arguments.gitleaks, metadata, arguments.output / "metadata-findings.json")
    scan(arguments.gitleaks, attachment_root, arguments.output / "attachment-findings.json")
    scan(arguments.gitleaks, retained, arguments.output / "retained-data-findings.json")
    pii = sorted(
        value
        for value in set(re.findall(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b", issue_text))
        if not value.lower().endswith("@users.noreply.github.com")
        and not value.lower().endswith("@example.com")
        and not value.lower().endswith("@example.invalid")
    )
    (arguments.output / "pii-findings.json").write_text(json.dumps(pii, indent=2) + "\n", encoding="utf-8")
    if pii:
        raise RuntimeError("issue or PR metadata contains email addresses requiring manual disposition")
    summary = {
        "schemaVersion": 1,
        "repository": arguments.repository,
        "issuesAndPulls": len(issues),
        "issueAttachments": len(attachments),
        "workflowRuns": len(runs),
        "retainedArtifacts": sum(not item.get("expired", False) for item in artifacts),
        "passed": True,
    }
    (arguments.output / "public-readiness.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
