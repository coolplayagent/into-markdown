"""Extract and authorize a complete native LibreOffice runtime."""

from __future__ import annotations

import ctypes
import hashlib
import os
import pathlib
import re
import shutil
import subprocess
import tarfile
import tempfile

from common import ReleaseError, regular_files, resolve_msvc_tool, run, sha256, write_json

SYSTEM_LINUX = {
    "ld-linux-aarch64.so.1",
    "ld-linux-x86-64.so.2",
    "libc.so.6",
    "libdl.so.2",
    "libgcc_s.so.1",
    "libm.so.6",
    "libpthread.so.0",
    "librt.so.1",
    "libstdc++.so.6",
}
SYSTEM_WINDOWS = {
    "advapi32.dll",
    "bcrypt.dll",
    "bcryptprimitives.dll",
    "comctl32.dll",
    "comdlg32.dll",
    "crypt32.dll",
    "d2d1.dll",
    "d3d9.dll",
    "dbghelp.dll",
    "dwmapi.dll",
    "fontsub.dll",
    "gdi32.dll",
    "gdiplus.dll",
    "httpapi.dll",
    "imm32.dll",
    "iphlpapi.dll",
    "kernel32.dll",
    "mfplat.dll",
    "mfplay.dll",
    "mfreadwrite.dll",
    "mpr.dll",
    "msvcrt.dll",
    "ncrypt.dll",
    "netapi32.dll",
    "ntdll.dll",
    "ole32.dll",
    "oleaut32.dll",
    "oledlg.dll",
    "propsys.dll",
    "rpcrt4.dll",
    "secur32.dll",
    "setupapi.dll",
    "shell32.dll",
    "shlwapi.dll",
    "ucrtbase.dll",
    "user32.dll",
    "userenv.dll",
    "usp10.dll",
    "version.dll",
    "winhttp.dll",
    "winmm.dll",
    "winspool.drv",
    "wer.dll",
    "wsock32.dll",
    "ws2_32.dll",
}
FORBIDDEN_CAPABILITIES = [
    "documentsLibrary",
    "enterpriseAuthentication",
    "internetClient",
    "internetClientServer",
    "musicLibrary",
    "picturesLibrary",
    "privateNetworkClientServer",
    "removableStorage",
    "sharedUserCertificates",
    "videosLibrary",
]
LEGACY_PLUGIN_ID = "official.legacy-office.libreoffice"


def generate(
    runtime: pathlib.Path,
    worker: pathlib.Path,
    artifact: pathlib.Path,
    artifact_authority: dict,
    target: str,
) -> pathlib.Path:
    runtime.mkdir(parents=True, exist_ok=False)
    suffix = ".exe" if target == "x86_64-pc-windows-msvc" else ""
    target_worker = runtime / f"legacy-office-worker{suffix}"
    copy_regular(worker, target_worker, executable=True)
    install_root = runtime / "libreoffice"
    if target.endswith("linux-gnu"):
        extract_linux(artifact, install_root)
        kit = unique(install_root.rglob("libmergedlo.so"), "LibreOfficeKit ELF library")
        binary_format = "elf"
        architecture = "aarch64" if target.startswith("aarch64") else "x86_64"
        system_names = dependency_closure_linux(target_worker, kit, runtime)
        system = [
            {"identity": name, "path": linux_system_path(name)}
            for name in sorted(system_names)
        ]
        app_container = None
    elif target == "x86_64-pc-windows-msvc":
        extract_windows(artifact, install_root)
        kit = unique(
            (path for path in install_root.rglob("mergedlo.dll")),
            "LibreOfficeKit PE library",
        )
        binary_format = "pe"
        architecture = "x86_64"
        system_names = dependency_closure_windows(target_worker, kit, runtime)
        system = [
            {"identity": name, "path": rf"C:\Windows\System32\{name.lower()}"}
            for name in sorted(system_names, key=str.lower)
        ]
        suffix = hashlib.sha256(LEGACY_PLUGIN_ID.encode("ascii")).hexdigest()[:24]
        profile = f"into-markdown.plugin.{suffix}"
        app_container = {
            "profileName": profile,
            "sid": derive_app_container_sid(profile),
            "capabilities": [],
            "forbiddenCapabilities": FORBIDDEN_CAPABILITIES,
        }
    else:
        raise ReleaseError(f"unsupported legacy Office target: {target}")
    verify_export(kit, binary_format)
    license_path = find_license(install_root)
    inventory = []
    for path in regular_files(runtime):
        relative = path.relative_to(runtime).as_posix()
        if path == target_worker:
            role = "worker"
        elif path == kit:
            role = "kitLibrary"
        elif path == license_path:
            role = "license"
        elif path.suffix.lower() in {".ini", ".rc", ".xcu", ".xcd", ".xml", ".cfg"}:
            role = "configuration"
        else:
            role = "runtime"
        inventory.append(
            {
                "path": relative,
                "bytes": path.stat().st_size,
                "sha256": sha256(path),
                "role": role,
            }
        )
    license_relative = license_path.relative_to(runtime).as_posix()
    sandbox = {
        "systemLibraries": system,
        "network": "deny",
        "childProcesses": "deny",
        "compatibilityChild": None,
        "appContainer": app_container,
    }
    value = {
        "schemaVersion": 1,
        "product": "LibreOffice",
        "version": "26.2.5",
        "sourceUrl": artifact_authority["url"],
        "targets": {
            target: {
                "artifactUrl": artifact_authority["url"],
                "artifactBytes": artifact_authority["bytes"],
                "artifactSha256": artifact_authority["sha256"],
                "installRoot": install_root.relative_to(runtime).as_posix(),
                "kitLibrary": kit.relative_to(runtime).as_posix(),
                "worker": target_worker.relative_to(runtime).as_posix(),
                "files": inventory,
                "licenses": [
                    {
                        "id": "libreoffice-license-1",
                        "spdx": None,
                        "noticePath": license_relative,
                        "noticeSha256": sha256(license_path),
                    }
                ],
                "abi": {
                    "binaryFormat": binary_format,
                    "architecture": architecture,
                    "libraryIdentity": kit.name,
                    "requiredExport": "libreofficekit_hook_2",
                },
                "limits": {
                    "addressSpaceOverheadBytes": 2147483648,
                    "fileSizeLimitBytes": 536870912,
                    "openFileLimit": 1024,
                    "processLimit": 1,
                },
                "sandbox": sandbox,
                "container": None,
            }
        },
    }
    destination = runtime / "authority.json"
    write_json(destination, value)
    return destination


def extract_linux(artifact: pathlib.Path, destination: pathlib.Path) -> None:
    with tempfile.TemporaryDirectory(prefix="into-md-lo-linux-") as name:
        outer = pathlib.Path(name) / "outer"
        outer.mkdir()
        safe_extract_tar(artifact, outer)
        payload = pathlib.Path(name) / "payload"
        payload.mkdir()
        packages = sorted(outer.rglob("*.deb"))
        if not packages:
            raise ReleaseError("LibreOffice Linux archive contains no Debian packages")
        for package in packages:
            run(["dpkg-deb", "-x", package, payload])
        candidates = [path for path in (payload / "opt").glob("libreoffice*") if path.is_dir()]
        source = unique(candidates, "LibreOffice Linux install root")
        copy_tree_materialized(source, destination)


def extract_windows(artifact: pathlib.Path, destination: pathlib.Path) -> None:
    with tempfile.TemporaryDirectory(prefix="into-md-lo-windows-") as name:
        administrative = pathlib.Path(name) / "administrative"
        administrative.mkdir()
        completed = subprocess.run(
            [
                "msiexec.exe",
                "/a",
                str(artifact),
                "/qn",
                f"TARGETDIR={administrative}",
            ],
            check=False,
        )
        if completed.returncode != 0:
            raise ReleaseError(
                f"LibreOffice administrative extraction failed ({completed.returncode})"
            )
        candidates = [
            path.parent.parent
            for path in administrative.rglob("mergedlo.dll")
            if path.parent.name.lower() == "program"
        ]
        source = unique(candidates, "LibreOffice Windows install root")
        generated_package = administrative / artifact.name
        if not generated_package.is_file():
            raise ReleaseError("LibreOffice administrative MSI output is absent")
        with generated_package.open("rb") as package:
            magic = package.read(8)
        if magic != bytes.fromhex("d0cf11e0a1b11ae1"):
            raise ReleaseError("LibreOffice administrative MSI output is absent")
        copy_tree_materialized(
            source,
            destination,
            windows_administrative_exclusions(source, generated_package),
        )


def safe_extract_tar(archive: pathlib.Path, destination: pathlib.Path) -> None:
    with tarfile.open(archive, "r:*") as source:
        for member in source:
            path = pathlib.PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts or member.issym() or member.islnk():
                raise ReleaseError("LibreOffice archive contains an unsafe outer entry")
        source.extractall(destination, filter="data")


def copy_tree_materialized(
    source: pathlib.Path,
    destination: pathlib.Path,
    excluded: set[pathlib.Path] | None = None,
) -> None:
    excluded = excluded or set()
    destination.mkdir()
    for entry in sorted(source.iterdir(), key=lambda path: path.name.lower()):
        target = destination / entry.name
        resolved = entry.resolve(strict=True)
        if resolved in excluded:
            continue
        if entry.is_dir():
            copy_tree_materialized(resolved, target, excluded)
        elif entry.is_file():
            copy_regular(resolved, target, executable=os.access(entry, os.X_OK))
        else:
            raise ReleaseError("LibreOffice runtime contains an unsupported entry")


def windows_administrative_exclusions(
    root: pathlib.Path, generated_package: pathlib.Path
) -> set[pathlib.Path]:
    """Exclude MSI deployment payloads and package-manager launcher templates."""
    program = root / "program"
    python = unique(program.glob("python-core-*"), "LibreOffice bundled Python root")
    distlib = python / "lib/pip/_vendor/distlib"
    setuptools = python / "lib/setuptools"
    launcher_names = {
        "t32.exe",
        "t64-arm.exe",
        "t64.exe",
        "w32.exe",
        "w64-arm.exe",
        "w64.exe",
        "cli-32.exe",
        "cli-64.exe",
        "cli-arm64.exe",
        "cli.exe",
        "gui-32.exe",
        "gui-64.exe",
        "gui-arm64.exe",
        "gui.exe",
    }
    launchers = {path.name: path for path in [*distlib.glob("*.exe"), *setuptools.glob("*.exe")]}
    if set(launchers) != launcher_names:
        raise ReleaseError("LibreOffice Python launcher template inventory changed")
    required = [
        generated_package,
        root / "System",
        program / "spsupp_x86.dll",
        program / "twain32shim.exe",
        *launchers.values(),
    ]
    if any(not path.exists() for path in required):
        raise ReleaseError("LibreOffice administrative exclusion inventory is incomplete")
    return {path.resolve(strict=True) for path in required}


def copy_regular(source: pathlib.Path, destination: pathlib.Path, executable: bool) -> None:
    if not source.is_file():
        raise ReleaseError(f"release input is not a file: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(0o755 if executable else 0o644)


def find_license(root: pathlib.Path) -> pathlib.Path:
    preferred = [
        path
        for path in regular_files(root)
        if path.name.lower() in {"license", "license.html", "copying", "notice"}
    ]
    return unique(preferred[:1], "LibreOffice license material")


def unique(values, label: str):
    result = list(dict.fromkeys(values))
    if len(result) != 1:
        raise ReleaseError(f"{label} is absent or ambiguous")
    return result[0]


def verify_export(kit: pathlib.Path, binary_format: str) -> None:
    command = (
        ["nm", "-D", str(kit)]
        if binary_format == "elf"
        else [resolve_msvc_tool("dumpbin.exe"), "/exports", str(kit)]
    )
    output = run(command)
    if "libreofficekit_hook_2" not in output:
        raise ReleaseError("LibreOfficeKit required export is absent")


def dependency_closure_linux(worker: pathlib.Path, kit: pathlib.Path, root: pathlib.Path) -> set[str]:
    inventory = inventory_by_identity(root, case_sensitive=True)
    pending = [worker, kit]
    visited = set()
    system = set()
    while pending:
        owner = pending.pop()
        if owner in visited:
            continue
        visited.add(owner)
        output = run(["readelf", "-d", str(owner)])
        needed = re.findall(r"Shared library: \[([^]]+)]", output)
        for identity in needed:
            if identity in SYSTEM_LINUX:
                system.add(identity)
            elif identity in inventory:
                pending.append(unique(inventory[identity], f"LibreOffice ELF dependency {identity}"))
            else:
                raise ReleaseError(f"undeclared LibreOffice ELF dependency: {identity}")
    return system


def linux_system_path(identity: str) -> str:
    output = run(["ldconfig", "-p"])
    matches = re.findall(rf"=>\s+(\S*/{re.escape(identity)})$", output, re.MULTILINE)
    resolved = sorted({str(pathlib.Path(path).resolve(strict=True)) for path in matches})
    named = [path for path in resolved if pathlib.Path(path).name == identity]
    return unique(named, f"Linux system library {identity}")


def dependency_closure_windows(worker: pathlib.Path, kit: pathlib.Path, root: pathlib.Path) -> set[str]:
    dumpbin = resolve_msvc_tool("dumpbin.exe")
    inventory = inventory_by_identity(root, case_sensitive=False)
    pending = [worker, kit]
    visited = set()
    system = set()
    undeclared = set()
    while pending:
        owner = pending.pop()
        if owner in visited:
            continue
        visited.add(owner)
        output = run([dumpbin, "/dependents", str(owner)])
        needed = dumpbin_dependencies(output)
        for identity in needed:
            lowered = identity.lower()
            if lowered in inventory:
                pending.extend(inventory[lowered])
            elif lowered in SYSTEM_WINDOWS or lowered.startswith("api-ms-win-"):
                system.add(lowered)
            else:
                undeclared.add(identity)
    if undeclared:
        names = ", ".join(sorted(undeclared, key=str.lower))
        raise ReleaseError(f"undeclared LibreOffice PE dependencies: {names}")
    return system


def dumpbin_dependencies(output: str) -> list[str]:
    """Parse only the dependency table from ``dumpbin /dependents`` output."""
    header = "Image has the following dependencies:"
    lines = output.splitlines()
    try:
        start = next(index for index, line in enumerate(lines) if line.strip() == header)
    except StopIteration as error:
        normalized = [line.strip() for line in lines]
        if any(line.startswith("Dump of file ") for line in normalized) and any(
            line.startswith("File Type: ") for line in normalized
        ):
            return []
        raise ReleaseError("dumpbin dependency section is absent") from error

    dependency = re.compile(r"[A-Za-z0-9_.+-]+\.(?:dll|drv)\Z", re.IGNORECASE)
    result: list[str] = []
    for line in lines[start + 1 :]:
        value = line.strip()
        if not value and not result:
            continue
        if dependency.fullmatch(value):
            result.append(value)
            continue
        if result:
            break
    return result


def inventory_by_identity(
    root: pathlib.Path, *, case_sensitive: bool
) -> dict[str, list[pathlib.Path]]:
    """Index runtime files without silently choosing between duplicate library names."""
    inventory: dict[str, list[pathlib.Path]] = {}
    for path in regular_files(root):
        identity = path.name if case_sensitive else path.name.lower()
        inventory.setdefault(identity, []).append(path)
    return inventory


def derive_app_container_sid(profile_name: str) -> str:
    if os.name != "nt":
        raise ReleaseError("AppContainer SID derivation requires Windows")
    userenv = ctypes.WinDLL("userenv", use_last_error=True)
    advapi = ctypes.WinDLL("advapi32", use_last_error=True)
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    sid = ctypes.c_void_p()
    derive = userenv.DeriveAppContainerSidFromAppContainerName
    derive.argtypes = [ctypes.c_wchar_p, ctypes.POINTER(ctypes.c_void_p)]
    derive.restype = ctypes.c_long
    if derive(profile_name, ctypes.byref(sid)) < 0 or not sid.value:
        raise ReleaseError("AppContainer SID derivation failed")
    text = ctypes.c_wchar_p()
    convert = advapi.ConvertSidToStringSidW
    convert.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_wchar_p)]
    convert.restype = ctypes.c_int
    local_free = kernel.LocalFree
    local_free.argtypes = [ctypes.c_void_p]
    local_free.restype = ctypes.c_void_p
    free_sid = advapi.FreeSid
    free_sid.argtypes = [ctypes.c_void_p]
    free_sid.restype = ctypes.c_void_p
    try:
        if not convert(sid, ctypes.byref(text)) or not text.value:
            raise ReleaseError("AppContainer SID formatting failed")
        return text.value
    finally:
        if text.value:
            local_free(ctypes.cast(text, ctypes.c_void_p))
        free_sid(sid)
