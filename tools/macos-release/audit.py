"""Mach-O architecture, deployment target and loader-boundary audit."""

from __future__ import annotations

import pathlib
import re
import hashlib

from common import ReleaseError, run

FORBIDDEN_LOADER = ("/Users/", "/home/", "/opt/homebrew/", "/usr/local/", "C:\\Users\\")
FORBIDDEN_BYTES = (
    b"/Users/",
    b"/home/",
    b"/private/tmp/into-md-",
    b"C:\\Users\\",
    b"c:\\users\\",
)
PINNED_ONNX_SHA256 = "c04fe65021445904a3cae047272cad05e648282c75bf1f9eb7b3440120ae13dc"
PINNED_ONNX_BUILD_ROOT = b"/Users/cloudtest/vss/_work/"
PINNED_PDFIUM_SHA256 = "33c98063af28c0b7cbf8227f4422bf5c15942df2455cf7f0a5dce3dc601d52b0"
PINNED_PDFIUM_BUILD_ROOT = b"/Users/runner/work/pdfium-binaries/"


def audit(root: pathlib.Path, profile: str, maximum_macos: str) -> None:
    for path in sorted(candidate for candidate in root.rglob("*") if candidate.is_file()):
        description = run(["/usr/bin/file", "-b", str(path)])
        if "Mach-O" in description:
            audit_macho(path, maximum_macos, description)


def audit_macho(path: pathlib.Path, maximum_macos: str, description: str | None = None) -> None:
    description = description or run(["/usr/bin/file", "-b", str(path)])
    if "Mach-O" not in description or "arm64" not in description or "x86_64" in description:
        raise ReleaseError(f"release binary is not thin arm64 Mach-O: {path.name}")
    build = run(["/usr/bin/vtool", "-show-build", str(path)])
    matches = re.findall(r"\bminos\s+(\d+(?:\.\d+){1,2})", build)
    if not matches or any(version_tuple(value) > version_tuple(maximum_macos) for value in matches):
        raise ReleaseError(f"Mach-O requires a newer macOS than the release contract: {path.name}")
    commands = run(["/usr/bin/otool", "-l", str(path)])
    dependencies = run(["/usr/bin/otool", "-L", str(path)])
    combined = commands + dependencies
    if any(value in combined for value in FORBIDDEN_LOADER):
        raise ReleaseError(f"Mach-O contains a forbidden loader path: {path.name}")
    audit_embedded_paths(path)
    for line in dependencies.splitlines()[1:]:
        identity = line.strip().split(" (compatibility", 1)[0]
        if identity.startswith("/") and not identity.startswith(("/usr/lib/", "/System/Library/")):
            raise ReleaseError(f"Mach-O has a non-system absolute dependency: {path.name}")
    run(["/usr/bin/codesign", "--verify", "--strict", str(path)])


def audit_embedded_paths(path: pathlib.Path) -> None:
    data = path.read_bytes()
    if (
        path.name == "libonnxruntime.dylib"
        and hashlib.sha256(data).hexdigest() == PINNED_ONNX_SHA256
    ):
        data = data.replace(PINNED_ONNX_BUILD_ROOT, b"")
    if (
        path.name == "libpdfium.dylib"
        and hashlib.sha256(data).hexdigest() == PINNED_PDFIUM_SHA256
    ):
        data = data.replace(PINNED_PDFIUM_BUILD_ROOT, b"")
    if any(value in data for value in FORBIDDEN_BYTES):
        raise ReleaseError(f"Mach-O embeds a developer home path: {path.name}")


def version_tuple(value: str) -> tuple[int, int, int]:
    fields = [int(field) for field in value.split(".")]
    return tuple((fields + [0, 0])[:3])
