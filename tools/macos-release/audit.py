"""Mach-O architecture, deployment target and loader-boundary audit."""

from __future__ import annotations

import pathlib
import re
import hashlib
import json
import tempfile
import zipfile

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
        if path.suffix == ".imp":
            audit_plugin_package(path, maximum_macos)
            continue
        description = run(["/usr/bin/file", "-b", str(path)])
        if "Mach-O" in description:
            audit_macho(path, maximum_macos, description)


def audit_plugin_package(path: pathlib.Path, maximum_macos: str) -> None:
    try:
        with zipfile.ZipFile(path) as package:
            manifest = json.loads(package.read("plugin.json"))
            if (
                manifest["protocol"] != "process-v1"
                or manifest["supportedTargets"] != ["aarch64-apple-darwin"]
                or set(manifest["entrypoints"]) != {"aarch64-apple-darwin"}
            ):
                raise ReleaseError(f"plugin lacks one macOS ARM64 target: {path.name}")
            declared = {item["path"]: item for item in manifest["files"]}
            expected = {*declared, "plugin.json"}
            if len(package.namelist()) != len(expected) or set(package.namelist()) != expected:
                raise ReleaseError(f"plugin inventory is not exact: {path.name}")
            if "provider.json" not in declared:
                raise ReleaseError(f"plugin lacks its provider descriptor: {path.name}")
            provider = json.loads(package.read("provider.json"))
            if provider["id"] != manifest["id"] or provider["version"] != manifest["version"]:
                raise ReleaseError(f"plugin provider identity differs: {path.name}")
            for name, authority in declared.items():
                must_execute = name.startswith("bin/") or name == "ffmpeg/ffmpeg"
                if authority.get("executable") is not must_execute:
                    raise ReleaseError(
                        f"plugin executable authority differs: {path.name}:{name}"
                    )
                info = package.getinfo(name)
                if info.is_dir() or info.file_size != authority["bytes"]:
                    raise ReleaseError(f"plugin file size changed: {path.name}:{name}")
                data = package.read(name)
                if hashlib.sha256(data).hexdigest() != authority["sha256"]:
                    raise ReleaseError(f"plugin file digest changed: {path.name}:{name}")
                with tempfile.NamedTemporaryFile(prefix="into-md-plugin-audit-") as extracted:
                    extracted.write(data)
                    extracted.flush()
                    candidate = pathlib.Path(extracted.name)
                    description = run(["/usr/bin/file", "-b", str(candidate)])
                    if "Mach-O" in description:
                        audit_macho(candidate, maximum_macos, description)
    except (KeyError, json.JSONDecodeError, OSError, zipfile.BadZipFile) as error:
        raise ReleaseError(f"plugin package audit failed: {path.name}: {error}") from error


def audit_macho(path: pathlib.Path, maximum_macos: str, description: str | None = None) -> None:
    description = description or run(["/usr/bin/file", "-b", str(path)])
    if "Mach-O" not in description or "arm64" not in description or "x86_64" in description:
        raise ReleaseError(f"release binary is not thin arm64 Mach-O: {path.name}")
    build = run(["/usr/bin/vtool", "-show-build", str(path)])
    matches = re.findall(r"\bminos\s+(\d+(?:\.\d+){1,2})", build)
    if not matches or any(version_tuple(value) > version_tuple(maximum_macos) for value in matches):
        raise ReleaseError(f"Mach-O requires a newer macOS than the release contract: {path.name}")
    commands = strip_tool_header(run(["/usr/bin/otool", "-l", str(path)]))
    dependencies = strip_tool_header(run(["/usr/bin/otool", "-L", str(path)]))
    combined = commands + dependencies
    if forbidden := next((value for value in FORBIDDEN_LOADER if value in combined), None):
        raise ReleaseError(
            f"Mach-O contains forbidden loader path {forbidden!r}: {path}"
        )
    audit_embedded_paths(path)
    for identity in dependency_identities(dependencies):
        if identity.startswith("/") and not identity.startswith(("/usr/lib/", "/System/Library/")):
            raise ReleaseError(f"Mach-O has a non-system absolute dependency: {path.name}")
    run(["/usr/bin/codesign", "--verify", "--strict", str(path)])


def strip_tool_header(output: str) -> str:
    """Remove otool's input filename without hiding any load-command content."""
    return "\n".join(output.splitlines()[1:])


def dependency_identities(output: str) -> list[str]:
    """Return every dependency from already header-stripped otool output."""
    return [
        line.strip().split(" (compatibility", 1)[0]
        for line in output.splitlines()
        if line.strip()
    ]


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
