#!/usr/bin/env python3
"""CLI for the explicit fixture-input downloader."""

from __future__ import annotations

import argparse
from pathlib import Path

try:
    from fixtures.download_lib import FixtureDownloadError, download_artifact, load_artifact
except ModuleNotFoundError:  # Direct `python fixtures/download.py` execution.
    from download_lib import FixtureDownloadError, download_artifact, load_artifact


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--artifact", required=True)
    parser.add_argument("--output-directory", type=Path, required=True)
    args = parser.parse_args()
    try:
        artifact = load_artifact(args.manifest, args.artifact)
        target = download_artifact(artifact, args.output_directory)
    except FixtureDownloadError as exc:
        parser.error(str(exc))
    print(target)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
