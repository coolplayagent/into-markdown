"""Mach-O architecture, deployment target and loader-boundary audit."""

from __future__ import annotations

import pathlib
import re

from common import ReleaseError, run

FORBIDDEN = ("/Users/", "/opt/homebrew/", "/usr/local/", "/usr/src/into-markdown")


def audit(root: pathlib.Path, profile: str, maximum_macos: str) -> None:
    paths = [
        root / "bin/into-md",
        root / "bin/onnxruntime-worker",
        root / "bin/installed-smoke",
        root / "bin/archive-check",
        root / "bin/legacy-office-runtime/legacy-office-worker",
        root / "bin/onnxruntime/lib/libonnxruntime.dylib",
        root / "lib/pdfium/libpdfium.dylib",
    ]
    for path in sorted(set(paths)):
        audit_macho(path, maximum_macos)


def audit_macho(path: pathlib.Path, maximum_macos: str) -> None:
    description = run(["/usr/bin/file", "-b", str(path)])
    if "Mach-O" not in description or "arm64" not in description or "x86_64" in description:
        raise ReleaseError(f"release binary is not thin arm64 Mach-O: {path.name}")
    build = run(["/usr/bin/vtool", "-show-build", str(path)])
    matches = re.findall(r"\bminos\s+(\d+(?:\.\d+){1,2})", build)
    if not matches or any(version_tuple(value) > version_tuple(maximum_macos) for value in matches):
        raise ReleaseError(f"Mach-O requires a newer macOS than the release contract: {path.name}")
    commands = run(["/usr/bin/otool", "-l", str(path)])
    dependencies = run(["/usr/bin/otool", "-L", str(path)])
    combined = commands + dependencies
    if any(value in combined for value in FORBIDDEN):
        raise ReleaseError(f"Mach-O contains a forbidden loader path: {path.name}")
    for line in dependencies.splitlines()[1:]:
        identity = line.strip().split(" (compatibility", 1)[0]
        if identity.startswith("/") and not identity.startswith(("/usr/lib/", "/System/Library/")):
            raise ReleaseError(f"Mach-O has a non-system absolute dependency: {path.name}")
    run(["/usr/bin/codesign", "--verify", "--strict", str(path)])


def version_tuple(value: str) -> tuple[int, int, int]:
    fields = [int(field) for field in value.split(".")]
    return tuple((fields + [0, 0])[:3])
