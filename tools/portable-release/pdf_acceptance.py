"""Exercise PDF recovery through the exact extracted release executable."""
from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import tempfile


def run(contents: dict[str, bytes], member: str) -> dict:
    with tempfile.TemporaryDirectory(prefix="into-md-pdf-acceptance-") as directory:
        root = pathlib.Path(directory).resolve()
        for relative, data in contents.items():
            path = root / "installed" / pathlib.PurePosixPath(relative)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        binary = root / "installed" / member
        binary.chmod(0o700)
        isolated = root / "isolated"
        isolated.mkdir()
        environment = dict(os.environ)
        environment.pop("PDFIUM_LIBRARY", None)
        for key in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_CACHE_HOME", "XDG_CONFIG_HOME", "TMP", "TEMP", "TMPDIR"):
            environment[key] = str(isolated)
        command = [sys.executable, str(pathlib.Path(__file__).resolve().parents[1] / "pdf-resilience/run.py"), "--into-md", str(binary), "--work-root", str(root / "results")]
        subprocess.run(command, env=environment, check=True, timeout=180, capture_output=True)
        return json.loads((root / "results/report.json").read_text(encoding="utf-8"))
