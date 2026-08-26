"""Shared deterministic helpers for Linux and Windows modular releases."""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import stat
import subprocess
from collections.abc import Iterable

ROOT = pathlib.Path(__file__).resolve().parents[2]
AUTHORITY = pathlib.Path(__file__).with_name("authority.json")


class ReleaseError(RuntimeError):
    """Stable packaging failure."""


def authority() -> dict:
    value = json.loads(AUTHORITY.read_text(encoding="utf-8"))
    if value.get("schemaVersion") != 1 or set(value.get("targets", {})) != {
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    }:
        raise ReleaseError("platform release authority schema or targets are invalid")
    return value


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def regular_files(root: pathlib.Path) -> list[pathlib.Path]:
    result: list[pathlib.Path] = []
    for directory, directories, files in os.walk(root, followlinks=False):
        base = pathlib.Path(directory)
        for name in sorted(directories + files):
            path = base / name
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode):
                raise ReleaseError(f"symbolic link is forbidden: {path.relative_to(root)}")
            if not (stat.S_ISDIR(mode) or stat.S_ISREG(mode)):
                raise ReleaseError(
                    f"non-regular archive entry is forbidden: {path.relative_to(root)}"
                )
        result.extend(base / name for name in sorted(files))
    return sorted(result, key=lambda path: path.relative_to(root).as_posix())


def run(
    arguments: Iterable[str],
    *,
    cwd: pathlib.Path | None = None,
    env: dict[str, str] | None = None,
) -> str:
    command = [str(argument) for argument in arguments]
    completed = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        errors="replace",
    )
    if completed.returncode:
        # Cargo and native linkers commonly finish with a generic summary such as
        # "build failed, waiting for other jobs to finish". Preserve a bounded
        # diagnostic tail so a hosted release failure exposes the actual compiler
        # or linker error without flooding Actions logs with the entire build.
        detail = completed.stderr.strip().splitlines()[-40:] or ["no diagnostic"]
        rendered = "\n".join(detail)
        raise ReleaseError(
            f"command failed ({command[0]}, exit {completed.returncode}):\n{rendered}"
        )
    return completed.stdout


def write_json(path: pathlib.Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def resolve_msvc_tool(name: str) -> pathlib.Path:
    if os.name != "nt" or pathlib.Path(name).name != name:
        raise ReleaseError("MSVC tool resolution requires a simple tool name on Windows")
    version = authority()["targets"]["x86_64-pc-windows-msvc"]["buildBaseline"][
        "msvcTools"
    ]
    configured = os.environ.get("VCToolsInstallDir")
    if configured:
        tools = pathlib.Path(configured)
        if tools.name != version:
            raise ReleaseError(f"active MSVC tools disagree with fixed version {version}")
    else:
        program_files = os.environ.get("ProgramFiles(x86)")
        if not program_files:
            raise ReleaseError("ProgramFiles(x86) is unavailable")
        vswhere = pathlib.Path(program_files) / "Microsoft Visual Studio/Installer/vswhere.exe"
        if not vswhere.is_file() or vswhere.is_symlink():
            raise ReleaseError("trusted vswhere.exe is unavailable")
        installations = run(
            [
                vswhere,
                "-latest",
                "-products",
                "*",
                "-requires",
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "-property",
                "installationPath",
            ]
        ).splitlines()
        if len(installations) != 1:
            raise ReleaseError("a unique Visual Studio C++ installation is unavailable")
        tools = pathlib.Path(installations[0]) / f"VC/Tools/MSVC/{version}"
    candidate = tools / f"bin/HostX64/x64/{name}"
    if not candidate.is_file() or candidate.is_symlink():
        raise ReleaseError(f"fixed MSVC tool is unavailable: {candidate}")
    return candidate.resolve(strict=True)


def resolve_windows_sdk_tool(name: str) -> pathlib.Path:
    if os.name != "nt" or pathlib.Path(name).name != name:
        raise ReleaseError("Windows SDK tool resolution requires a simple tool name on Windows")
    version = authority()["targets"]["x86_64-pc-windows-msvc"]["buildBaseline"][
        "windowsSdk"
    ]
    configured = os.environ.get("WindowsSdkDir")
    if configured:
        kits = pathlib.Path(configured)
    else:
        program_files = os.environ.get("ProgramFiles(x86)")
        if not program_files:
            raise ReleaseError("ProgramFiles(x86) is unavailable")
        kits = pathlib.Path(program_files) / "Windows Kits/10"
    candidate = kits / f"bin/{version}/x64/{name}"
    if not candidate.is_file() or candidate.is_symlink():
        raise ReleaseError(f"fixed Windows SDK tool is unavailable: {candidate}")
    return candidate.resolve(strict=True)
