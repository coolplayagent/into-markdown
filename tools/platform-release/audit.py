#!/usr/bin/env python3
"""Audit an assembled Linux or Windows Core and its two signed IMP packages."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import shutil
import stat
import subprocess
import tempfile
import zipfile
from dataclasses import dataclass, field

from common import resolve_msvc_tool, resolve_windows_sdk_tool


TARGETS = {
    "x86_64-unknown-linux-gnu": ("linux", "Advanced Micro Devices X86-64"),
    "aarch64-unknown-linux-gnu": ("linux", "AArch64"),
    "x86_64-pc-windows-msvc": ("windows", "8664 machine (x64)"),
}
PROJECT_WINDOWS_BINARIES = re.compile(
    r"(?:into-md(?:-(?:installer|ocr-provider|media-provider))?"
    r"|installed-smoke|archive-check|onnxruntime-worker)\.exe$",
    re.IGNORECASE,
)
GLIBC = re.compile(r"GLIBC_(\d+)\.(\d+)")
RPATH = re.compile(r"\((?:RPATH|RUNPATH)\).*?\[(.*?)\]")
NEEDED = re.compile(r"\(NEEDED\).*?\[(.*?)\]")


class AuditFailure(RuntimeError):
    pass


@dataclass
class Finding:
    path: str
    check: str
    passed: bool
    detail: str


@dataclass
class Audit:
    target: str
    findings: list[Finding] = field(default_factory=list)

    def record(self, path: pathlib.Path | str, check: str, passed: bool, detail: str) -> None:
        self.findings.append(Finding(str(path).replace("\\", "/"), check, passed, detail))

    def require(self, path: pathlib.Path | str, check: str, passed: bool, detail: str) -> None:
        self.record(path, check, passed, detail)
        if not passed:
            raise AuditFailure(f"{path}: {check}: {detail}")


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def safe_zip_extract(package: pathlib.Path, destination: pathlib.Path, audit: Audit) -> None:
    with zipfile.ZipFile(package) as archive:
        seen: set[str] = set()
        for item in archive.infolist():
            normalized = pathlib.PurePosixPath(item.filename)
            mode = item.external_attr >> 16
            safe = (
                not normalized.is_absolute()
                and ".." not in normalized.parts
                and "" not in normalized.parts
                and not stat.S_ISLNK(mode)
                and item.filename not in seen
            )
            audit.require(package.name, "safe-zip-entry", safe, item.filename)
            seen.add(item.filename)
            target = destination.joinpath(*normalized.parts)
            if item.is_dir():
                target.mkdir(parents=True, exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                with archive.open(item) as source, target.open("xb") as output:
                    shutil.copyfileobj(source, output)


def command(arguments: list[str]) -> str:
    process = subprocess.run(arguments, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if process.returncode:
        raise AuditFailure(f"command failed ({' '.join(arguments)}): {process.stdout.strip()}")
    return process.stdout


def candidates(root: pathlib.Path, magic: bytes) -> list[pathlib.Path]:
    result = []
    for path in sorted(root.rglob("*")):
        if path.is_file() and not path.is_symlink():
            with path.open("rb") as source:
                if source.read(len(magic)) == magic:
                    result.append(path)
    return result


def distributed_source_fixture(relative: pathlib.Path) -> bool:
    return relative.parts[:3] == ("lib", "into-markdown-rust", "vendor")


def clr_il_only(output: str) -> bool:
    return bool(
        re.search(r"^\s+IL Only\s*$", output, re.MULTILINE)
        and re.search(
            r"^\s+0\s+\[\s*0\s*\]\s+RVA\s+\[size\]\s+of\s+ManagedNativeHeader Directory\s*$",
            output,
            re.MULTILINE | re.IGNORECASE,
        )
    )


def audit_tree(root: pathlib.Path, audit: Audit, linux: bool) -> None:
    audit.require(root, "regular-root", root.is_dir() and not root.is_symlink(), "root must be a real directory")
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        audit.require(relative, "no-link-or-reparse", not path.is_symlink(), "links are forbidden in release content")
        if linux:
            mode = stat.S_IMODE(path.lstat().st_mode)
            audit.require(relative, "safe-mode", mode & 0o6022 == 0, f"mode {mode:o} must not be setuid/setgid or group/world-writable")


def audit_linux(root: pathlib.Path, expected_machine: str, audit: Audit) -> None:
    readelf = shutil.which("readelf")
    audit.require("readelf", "tool-present", readelf is not None, "GNU readelf is required")
    binaries = candidates(root, b"\x7fELF")
    audit.require(root, "elf-present", bool(binaries), "no ELF binary found")
    for binary in binaries:
        relative = binary.relative_to(root)
        headers = command([readelf, "-h", str(binary)])
        machine = next((line.split(":", 1)[1].strip() for line in headers.splitlines() if "Machine:" in line), "")
        audit.require(relative, "elf-machine", machine == expected_machine, machine)
        dynamic = command([readelf, "-dW", str(binary)])
        for value in RPATH.findall(dynamic):
            entries = value.split(":")
            safe = all(entry and not entry.startswith("/") and (entry == "$ORIGIN" or entry.startswith("$ORIGIN/")) for entry in entries)
            audit.require(relative, "rpath", safe, value)
        for library in NEEDED.findall(dynamic):
            audit.require(relative, "needed-name", "/" not in library and "\\" not in library, library)
        versions = command([readelf, "--version-info", "-W", str(binary)])
        required = sorted({(int(major), int(minor)) for major, minor in GLIBC.findall(versions)})
        ceiling = max(required, default=(0, 0))
        audit.require(relative, "glibc-ceiling", ceiling <= (2, 28), f"highest required GLIBC is {ceiling[0]}.{ceiling[1]}")
        interpreter = command([readelf, "-lW", str(binary)])
        paths = re.findall(r"Requesting program interpreter:\s*([^\]]+)", interpreter)
        if paths:
            allowed = {"/lib64/ld-linux-x86-64.so.2", "/lib/ld-linux-aarch64.so.1"}
            audit.require(relative, "elf-interpreter", paths[0] in allowed, paths[0])


def audit_windows(
    root: pathlib.Path,
    expected_machine: str,
    audit: Audit,
    allow_unsigned_test_artifacts: bool,
) -> None:
    dumpbin = resolve_msvc_tool("dumpbin.exe")
    signtool = resolve_windows_sdk_tool("signtool.exe")
    audit.require("dumpbin", "tool-present", True, str(dumpbin))
    audit.require("signtool", "tool-present", True, str(signtool))
    binaries = []
    for binary in candidates(root, b"MZ"):
        relative = binary.relative_to(root)
        if distributed_source_fixture(relative):
            audit.record(
                relative,
                "non-runtime-source-fixture",
                True,
                "preserved upstream Rust source fixture; excluded from installed PE runtime",
            )
        else:
            binaries.append(binary)
    audit.require(root, "pe-present", bool(binaries), "no PE binary found")
    for binary in binaries:
        relative = binary.relative_to(root)
        headers = command([dumpbin, "/nologo", "/headers", str(binary)])
        machine_lines = [line.strip() for line in headers.splitlines() if "machine (" in line.lower()]
        native_machine = any(
            expected_machine.lower() in line.lower() for line in machine_lines
        )
        managed = False
        if not native_machine:
            clr = subprocess.run(
                [dumpbin, "/nologo", "/clrheader", str(binary)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            managed = clr.returncode == 0 and clr_il_only(clr.stdout)
            if managed:
                audit.record(
                    relative,
                    "pe-managed-il-only",
                    True,
                    "architecture-neutral CLR IL with no managed native image",
                )
        audit.require(
            relative,
            "pe-machine",
            native_machine or managed,
            "; ".join(machine_lines),
        )
        imports = command([dumpbin, "/nologo", "/imports", str(binary)])
        imported = re.findall(r"^\s+([A-Za-z0-9_.+-]+\.dll)\s*$", imports, re.MULTILINE | re.IGNORECASE)
        audit.require(relative, "import-paths", all("/" not in name and "\\" not in name for name in imported), ", ".join(imported))
        if PROJECT_WINDOWS_BINARIES.search(binary.name):
            signature = subprocess.run(
                [signtool, "verify", "/pa", "/all", str(binary)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            if allow_unsigned_test_artifacts and signature.returncode != 0:
                audit.record(
                    relative,
                    "authenticode-local-test-exemption",
                    True,
                    "unsigned local test artifact; production Authenticode was not evaluated",
                )
            else:
                audit.require(
                    relative,
                    "authenticode",
                    signature.returncode == 0,
                    signature.stdout.strip()[-1000:],
                )


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", choices=sorted(TARGETS), required=True)
    parser.add_argument("--core-root", type=pathlib.Path, required=True)
    parser.add_argument("--plugins", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    parser.add_argument("--allow-unsigned-test-artifacts", action="store_true")
    return parser.parse_args()


def run(
    target: str,
    core_root: pathlib.Path,
    plugins: pathlib.Path,
    *,
    allow_unsigned_test_artifacts: bool = False,
) -> dict[str, object]:
    platform, machine = TARGETS[target]
    audit = Audit(target)
    artifacts: list[dict[str, str]] = []
    roots: list[tuple[str, pathlib.Path]] = [("core", core_root)]
    packages = sorted(plugins.glob("*.imp"))
    audit.require(plugins, "plugin-count", len(packages) == 2, f"expected 2 IMP packages, found {len(packages)}")
    temporary = tempfile.TemporaryDirectory(prefix="into-md-platform-audit-")
    try:
        extracted = pathlib.Path(temporary.name)
        for package in packages:
            artifacts.append({"name": package.name, "sha256": sha256(package)})
            destination = extracted / package.stem
            destination.mkdir()
            safe_zip_extract(package, destination, audit)
            roots.append((package.name, destination))
        for label, root in roots:
            audit_tree(root, audit, platform == "linux")
            if platform == "linux":
                audit_linux(root, machine, audit)
            else:
                audit_windows(root, machine, audit, allow_unsigned_test_artifacts)
            audit.record(label, "platform-audit", True, "passed")
    finally:
        temporary.cleanup()
    return {
        "schemaVersion": 1,
        "target": target,
        "testMode": {
            "allowUnsignedArtifacts": allow_unsigned_test_artifacts,
            "formalReleaseEligible": not allow_unsigned_test_artifacts,
        },
        "artifacts": artifacts,
        "findings": [finding.__dict__ for finding in audit.findings],
        "passed": all(finding.passed for finding in audit.findings),
    }


def main() -> int:
    arguments = parse_arguments()
    report: dict[str, object]
    try:
        report = run(
            arguments.target,
            arguments.core_root.resolve(strict=True),
            arguments.plugins.resolve(strict=True),
            allow_unsigned_test_artifacts=arguments.allow_unsigned_test_artifacts,
        )
    except Exception as error:
        report = {"schemaVersion": 1, "target": arguments.target, "error": str(error), "passed": False}
    arguments.report.parent.mkdir(parents=True, exist_ok=True)
    arguments.report.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if not report["passed"]:
        raise SystemExit(report.get("error", "platform audit failed"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
