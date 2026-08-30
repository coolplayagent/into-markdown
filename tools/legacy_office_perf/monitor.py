"""Process monitoring and filesystem residue accounting for performance runs."""

from __future__ import annotations

import ctypes
import hashlib
import json
import os
import pathlib
import subprocess
import tempfile
import time
from dataclasses import dataclass


@dataclass(frozen=True)
class Observation:
    elapsed_ms: float
    peak_rss_bytes: int | None
    returncode: int
    stdout_sha256: str
    stderr_sha256: str


@dataclass(frozen=True)
class XlsObservation:
    elapsed_ms: float
    peak_rss_bytes: int | None
    returncode: int
    output_bytes: int
    output_sha256: str | None
    output_path: pathlib.Path
    peak_temporary_bytes: int
    temporary_bytes_after: int
    residual_paths: tuple[str, ...]
    report: dict[str, object] | None
    stderr: str


def private_environment(home: pathlib.Path) -> dict[str, str]:
    environment = {
        "HOME": str(home),
        "USERPROFILE": str(home),
        "XDG_CONFIG_HOME": str(home / "xdg-config"),
        "XDG_DATA_HOME": str(home / "xdg-data"),
        "TMPDIR": str(home / "tmp"),
        "TEMP": str(home / "tmp"),
        "TMP": str(home / "tmp"),
        "NO_COLOR": "1",
        "PATH": "",
    }
    for name in ("SystemRoot", "WINDIR"):
        if name in os.environ:
            environment[name] = os.environ[name]
    for directory in (home / "xdg-config", home / "xdg-data", home / "tmp"):
        directory.mkdir(parents=True, exist_ok=True)
    return environment


def linux_rss(pid: int) -> int | None:
    try:
        for line in pathlib.Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    except (FileNotFoundError, ProcessLookupError, ValueError):
        return None
    return None


def macos_rss(pid: int) -> int | None:
    try:
        result = subprocess.run(
            ["/bin/ps", "-o", "rss=", "-p", str(pid)],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=1,
        )
        return int(result.stdout.strip()) * 1024 if result.stdout.strip() else None
    except (OSError, subprocess.SubprocessError, ValueError):
        return None


def windows_rss(process: subprocess.Popen[bytes]) -> int | None:
    if sys.platform != "win32":
        return None

    class Counters(ctypes.Structure):
        _fields_ = [
            ("cb", ctypes.c_ulong),
            ("PageFaultCount", ctypes.c_ulong),
            ("PeakWorkingSetSize", ctypes.c_size_t),
            ("WorkingSetSize", ctypes.c_size_t),
            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
            ("PagefileUsage", ctypes.c_size_t),
            ("PeakPagefileUsage", ctypes.c_size_t),
        ]

    counters = Counters()
    counters.cb = ctypes.sizeof(counters)
    try:
        ok = ctypes.windll.psapi.GetProcessMemoryInfo(
            ctypes.c_void_p(process._handle), ctypes.byref(counters), counters.cb
        )
    except (AttributeError, OSError):
        return None
    return int(counters.PeakWorkingSetSize) if ok else None


def resident_bytes(process: subprocess.Popen[bytes]) -> int | None:
    if sys.platform.startswith("linux"):
        return linux_rss(process.pid)
    if sys.platform == "darwin":
        return macos_rss(process.pid)
    return windows_rss(process)


def observe(
    cli: pathlib.Path,
    arguments: list[str],
    current_dir: pathlib.Path,
    home: pathlib.Path,
) -> Observation:
    started = time.perf_counter()
    process = subprocess.Popen(
        [str(cli), *arguments],
        cwd=current_dir,
        env=private_environment(home),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    peak = 0
    deadline = time.monotonic() + 30
    while True:
        sample = resident_bytes(process)
        peak = max(peak, sample or 0)
        if process.poll() is not None:
            break
        if time.monotonic() >= deadline:
            process.kill()
            process.wait()
            raise RuntimeError("benchmark command exceeded 30 seconds")
        time.sleep(0.002)
    stdout, stderr = process.communicate()
    sample = resident_bytes(process)
    peak = max(peak, sample or 0)
    return Observation(
        elapsed_ms=(time.perf_counter() - started) * 1000,
        peak_rss_bytes=peak or None,
        returncode=process.returncode,
        stdout_sha256=hashlib.sha256(stdout).hexdigest(),
        stderr_sha256=hashlib.sha256(stderr).hexdigest(),
    )


def file_sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def directory_file_bytes(root: pathlib.Path) -> int:
    total = 0
    for path in root.rglob("*"):
        try:
            if path.is_file() and not path.is_symlink():
                total += path.stat().st_size
        except FileNotFoundError:
            continue
    return total


def transient_output_bytes(root: pathlib.Path, final_output: pathlib.Path) -> int:
    total = 0
    for path in root.rglob("*"):
        try:
            if path != final_output and path.is_file() and not path.is_symlink():
                total += path.stat().st_size
        except FileNotFoundError:
            continue
    return total


def output_residuals(root: pathlib.Path, allowed: pathlib.Path | None) -> tuple[str, ...]:
    if not root.exists():
        return ()
    return tuple(
        sorted(
            path.relative_to(root).as_posix()
            for path in root.rglob("*")
            if path != allowed and (path.is_file() or path.is_symlink())
        )
    )


def observe_xls(
    cli: pathlib.Path,
    source: pathlib.Path,
    current_dir: pathlib.Path,
    run_root: pathlib.Path,
) -> XlsObservation:
    home = run_root / "home"
    run_root.mkdir(parents=True)
    output_root = run_root / "output"
    output_root.mkdir(parents=True)
    output = output_root / "output.ir.json"
    report_path = run_root / "report.json"
    started = time.perf_counter()
    process = subprocess.Popen(
        [
            str(cli),
            "--no-config",
            str(source),
            "--format",
            "xls",
            "--error-policy",
            "best-effort",
            "--emit",
            "ir-json",
            "--asset-mode",
            "embed",
            "--quiet",
            "--output",
            str(output),
            "--conflict",
            "error",
            "--report",
            str(report_path),
        ],
        cwd=current_dir,
        env=private_environment(home),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    peak = 0
    peak_temporary = 0
    deadline = time.monotonic() + 120
    while True:
        sample = resident_bytes(process)
        peak = max(peak, sample or 0)
        peak_temporary = max(
            peak_temporary,
            directory_file_bytes(home / "tmp")
            + transient_output_bytes(output_root, output),
        )
        if process.poll() is not None:
            break
        if time.monotonic() >= deadline:
            process.kill()
            process.wait()
            raise RuntimeError(f"XLS benchmark command exceeded 120 seconds: {source.name}")
        time.sleep(0.002)
    _, stderr = process.communicate()
    sample = resident_bytes(process)
    peak = max(peak, sample or 0)
    peak_temporary = max(
        peak_temporary,
        directory_file_bytes(home / "tmp")
        + transient_output_bytes(output_root, output),
    )
    output_bytes = output.stat().st_size if output.is_file() else 0
    temporary = home / "tmp"
    report = json.loads(report_path.read_text(encoding="utf-8")) if report_path.is_file() else None
    return XlsObservation(
        elapsed_ms=(time.perf_counter() - started) * 1000,
        peak_rss_bytes=peak or None,
        returncode=process.returncode,
        output_bytes=output_bytes,
        output_sha256=file_sha256(output) if output_bytes else None,
        output_path=output,
        peak_temporary_bytes=peak_temporary,
        temporary_bytes_after=directory_file_bytes(temporary),
        residual_paths=output_residuals(
            output_root, output if process.returncode == 0 and output.is_file() else None
        ),
        report=report,
        stderr=stderr.decode("utf-8", errors="replace")[-2048:],
    )
