#!/usr/bin/env python3
"""Audit an assembled Linux or Windows Core and its official capability packages."""

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
BUNDLED_OCR = pathlib.PurePosixPath(
    "share/into-markdown/plugins/packages/official.ocr.ppocrv6.imp"
)
OFFICIAL_OCR = "official.ocr.ppocrv6"
OFFICIAL_SPEECH = "official.media.whisper"


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


def require_package_identity(
    package: pathlib.Path, plugin_id: str, target: str, audit: Audit
) -> None:
    try:
        with zipfile.ZipFile(package) as archive:
            names = archive.namelist()
            if names.count("plugin.json") != 1:
                raise ValueError("plugin.json must occur exactly once")
            manifest = json.loads(archive.read("plugin.json"))
    except (OSError, ValueError, KeyError, json.JSONDecodeError, zipfile.BadZipFile) as error:
        audit.require(package.name, "plugin-manifest", False, str(error))
        return
    identity_matches = (
        manifest.get("id") == plugin_id
        and manifest.get("supportedTargets") == [target]
        and set(manifest.get("entrypoints", {})) == {target}
    )
    audit.require(
        package.name,
        "plugin-identity",
        identity_matches,
        f"expected {plugin_id} for {target}",
    )


def resolve_release_packages(
    target: str, core_root: pathlib.Path, plugins: pathlib.Path, audit: Audit
) -> tuple[pathlib.Path, pathlib.Path]:
    bundled = core_root.joinpath(*BUNDLED_OCR.parts)
    audit.require(
        bundled,
        "bundled-ocr-package",
        bundled.is_file() and not bundled.is_symlink(),
        "Core must contain one safe official OCR package",
    )
    catalog_path = core_root / "share/into-markdown/plugins/official-publisher.json"
    try:
        catalog = json.loads(catalog_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        audit.require(catalog_path, "official-catalog", False, str(error))
        raise AssertionError("unreachable") from error
    records = catalog.get("packages", {})
    ocr = records.get(OFFICIAL_OCR, {}) if isinstance(records, dict) else {}
    speech = records.get(OFFICIAL_SPEECH, {}) if isinstance(records, dict) else {}
    signing_key_sha256 = catalog.get("signingKeySha256", "")
    audit.require(
        catalog_path,
        "bundled-ocr-authority",
        catalog.get("schemaVersion") == 2
        and set(records) == {OFFICIAL_OCR, OFFICIAL_SPEECH}
        and isinstance(catalog.get("signingKeyId"), str)
        and bool(catalog.get("signingKeyId"))
        and re.fullmatch(r"[0-9a-f]{64}", signing_key_sha256) is not None
        and ocr.get("file") == BUNDLED_OCR.name
        and ocr.get("sha256") == sha256(bundled)
        and "url" not in ocr,
        "catalog must bind the bundled OCR filename and digest",
    )

    packages = sorted(plugins.glob("*.imp"))
    candidates = {
        plugin_id: [
            plugins / f"{plugin_id}.imp",
            plugins / f"{plugin_id}-{target}.imp",
        ]
        for plugin_id in (OFFICIAL_OCR, OFFICIAL_SPEECH)
    }
    selected = {
        plugin_id: [item for item in values if item.is_file() and not item.is_symlink()]
        for plugin_id, values in candidates.items()
    }
    selected_paths = [item for values in selected.values() for item in values]
    audit.require(
        plugins,
        "release-package-build-set",
        all(len(values) == 1 for values in selected.values())
        and {item.resolve() for item in packages}
        == {item.resolve() for item in selected_paths},
        "build output must contain exactly one bundled OCR source and one external speech IMP; "
        f"found {[item.name for item in packages]}",
    )
    built_ocr = selected[OFFICIAL_OCR][0]
    external = selected[OFFICIAL_SPEECH][0]
    audit.require(
        bundled,
        "bundled-ocr-build-binding",
        sha256(bundled) == sha256(built_ocr),
        "Core bundled OCR bytes must equal the audited release build output",
    )
    audit.require(
        catalog_path,
        "speech-package-authority",
        speech.get("sha256") == sha256(external)
        and isinstance(speech.get("url"), str)
        and speech["url"].startswith("https://"),
        "catalog must bind the external speech package digest and URL",
    )
    require_package_identity(bundled, OFFICIAL_OCR, target, audit)
    require_package_identity(external, OFFICIAL_SPEECH, target, audit)
    return bundled, external


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


def clr_il_only(output: str) -> bool:
    return bool(
        re.search(r"^\s+IL Only\s*$", output, re.MULTILINE)
        and re.search(
            r"^\s+0\s+\[\s*0\s*\]\s+RVA\s+\[size\]\s+of\s+ManagedNativeHeader Directory\s*$",
            output,
            re.MULTILINE | re.IGNORECASE,
        )
    )


def requires_x86_64_extension_level(notes: str) -> bool:
    """Return true when ELF declares an ISA requirement above x86-64 baseline."""
    needed = "\n".join(
        line for line in notes.splitlines() if "x86 ISA needed:" in line
    )
    return re.search(r"\bx86-64-v[234]\b", needed, re.IGNORECASE) is not None


def audit_tree(root: pathlib.Path, audit: Audit, linux: bool) -> None:
    audit.require(root, "regular-root", root.is_dir() and not root.is_symlink(), "root must be a real directory")
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        audit.require(relative, "no-link-or-reparse", not path.is_symlink(), "links are forbidden in release content")
        if linux:
            mode = stat.S_IMODE(path.lstat().st_mode)
            audit.require(relative, "safe-mode", mode & 0o6022 == 0, f"mode {mode:o} must not be setuid/setgid or group/world-writable")


def audit_rust_facade(root: pathlib.Path, audit: Audit) -> None:
    package = root / "lib/into-markdown-rust.zip"
    audit.require(
        package.relative_to(root),
        "rust-facade-archive",
        package.is_file() and not package.is_symlink(),
        "Core must contain one regular offline Rust facade archive",
    )
    try:
        with zipfile.ZipFile(package) as archive:
            entries = archive.infolist()
            names = [entry.filename for entry in entries]
            folded = [name.casefold() for name in names]
            safe = all(
                name
                and "\\" not in name
                and not pathlib.PurePosixPath(name).is_absolute()
                and all(part not in {"", ".", ".."} for part in name.split("/"))
                and not entry.is_dir()
                and stat.S_IFMT(entry.external_attr >> 16) in {0, stat.S_IFREG}
                for name, entry in zip(names, entries, strict=True)
            )
            deterministic = (
                names == sorted(names)
                and len(folded) == len(set(folded))
                and all(entry.date_time == (1980, 1, 1, 0, 0, 0) for entry in entries)
                and all(entry.compress_type == zipfile.ZIP_STORED for entry in entries)
            )
            complete = (
                names.count("Cargo.toml") == 1
                and names.count("Cargo.lock") == 1
                and any(name.startswith("vendor/") for name in names)
            )
    except (OSError, zipfile.BadZipFile) as error:
        audit.require(package.relative_to(root), "rust-facade-readable", False, str(error))
        return
    audit.require(
        package.relative_to(root),
        "rust-facade-safe-members",
        safe,
        "members must be unique regular files with safe portable paths",
    )
    audit.require(
        package.relative_to(root),
        "rust-facade-deterministic",
        deterministic,
        "members must use sorted names, fixed timestamps, and stored bytes",
    )
    audit.require(
        package.relative_to(root),
        "rust-facade-offline-complete",
        complete,
        "archive must contain Cargo.toml, Cargo.lock, and vendor content",
    )


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
        if expected_machine == "Advanced Micro Devices X86-64":
            notes = command([readelf, "-nW", str(binary)])
            audit.require(
                relative,
                "x86-64-isa-baseline",
                not requires_x86_64_extension_level(notes),
                "ELF must not require x86-64-v2/v3/v4",
            )


def audit_windows(
    root: pathlib.Path,
    expected_machine: str,
    audit: Audit,
    signing_mode: str,
) -> None:
    dumpbin = resolve_msvc_tool("dumpbin.exe")
    signtool = resolve_windows_sdk_tool("signtool.exe")
    audit.require("dumpbin", "tool-present", True, str(dumpbin))
    audit.require("signtool", "tool-present", True, str(signtool))
    binaries = []
    for binary in candidates(root, b"MZ"):
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
            if signing_mode == "unsigned":
                audit.require(
                    relative,
                    "authenticode-unsigned-distribution",
                    signature.returncode != 0,
                    "unsigned distribution must not carry an unexpected Authenticode identity",
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
    parser.add_argument(
        "--windows-signing-mode",
        choices=("signed", "unsigned"),
        default="signed",
        help="Expected Authenticode policy for Windows project binaries.",
    )
    parser.add_argument(
        "--allow-unsigned-test-artifacts",
        action="store_true",
        help=argparse.SUPPRESS,
    )
    return parser.parse_args()


def run(
    target: str,
    core_root: pathlib.Path,
    plugins: pathlib.Path,
    *,
    windows_signing_mode: str = "signed",
    allow_unsigned_test_artifacts: bool = False,
) -> dict[str, object]:
    if windows_signing_mode not in {"signed", "unsigned"}:
        raise ValueError(f"unsupported Windows signing mode: {windows_signing_mode}")
    if allow_unsigned_test_artifacts:
        windows_signing_mode = "unsigned"
    platform, machine = TARGETS[target]
    audit = Audit(target)
    artifacts: list[dict[str, str]] = []
    roots: list[tuple[str, pathlib.Path]] = [("core", core_root)]
    temporary = tempfile.TemporaryDirectory(prefix="into-md-platform-audit-")
    try:
        extracted = pathlib.Path(temporary.name)
        bundled_ocr, external_speech = resolve_release_packages(
            target, core_root, plugins, audit
        )
        audit_rust_facade(core_root, audit)
        for disposition, package in (
            ("bundled-core-capability", bundled_ocr),
            ("external-plugin", external_speech),
        ):
            artifacts.append(
                {"name": package.name, "sha256": sha256(package), "disposition": disposition}
            )
            destination = extracted / package.stem
            destination.mkdir()
            safe_zip_extract(package, destination, audit)
            roots.append((package.name, destination))
        for label, root in roots:
            audit_tree(root, audit, platform == "linux")
            if platform == "linux":
                audit_linux(root, machine, audit)
            else:
                audit_windows(root, machine, audit, windows_signing_mode)
            audit.record(label, "platform-audit", True, "passed")
    finally:
        temporary.cleanup()
    return {
        "schemaVersion": 1,
        "target": target,
        "distributionSigning": {
            "mode": windows_signing_mode if platform == "windows" else "not-audited",
            "publisherIdentityVerified": windows_signing_mode == "signed" if platform == "windows" else None,
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
            windows_signing_mode=arguments.windows_signing_mode,
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
