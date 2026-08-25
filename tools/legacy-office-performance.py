#!/usr/bin/env python3
"""Compare a baseline and candidate Core CLI on the Office 97-2003 corpus."""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import pathlib
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass


FORMATS = ("doc", "ppt", "xls")
MIB = 1024 * 1024


@dataclass(frozen=True)
class Observation:
    elapsed_ms: float
    peak_rss_bytes: int | None
    returncode: int
    stdout_sha256: str
    stderr_sha256: str


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-cli", type=pathlib.Path, required=True)
    parser.add_argument("--candidate-cli", type=pathlib.Path, required=True)
    parser.add_argument("--fixtures", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--parallelism", type=int, default=3)
    return parser.parse_args()


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


def summarized(observations: list[Observation]) -> dict[str, object]:
    elapsed = [item.elapsed_ms for item in observations]
    peaks = [item.peak_rss_bytes for item in observations if item.peak_rss_bytes is not None]
    return {
        "runs": len(observations),
        "medianMillis": round(statistics.median(elapsed), 3),
        "maximumMillis": round(max(elapsed), 3),
        "peakRssBytes": max(peaks) if peaks else None,
        "exitCodes": sorted({item.returncode for item in observations}),
        "stdoutSha256": sorted({item.stdout_sha256 for item in observations}),
        "stderrSha256": sorted({item.stderr_sha256 for item in observations}),
    }


def individual_conversions(
    cli: pathlib.Path,
    fixtures: pathlib.Path,
    home: pathlib.Path,
    iterations: int,
) -> tuple[dict[str, object], list[str]]:
    report: dict[str, object] = {}
    failures: list[str] = []
    for format_name in FORMATS:
        observations = [
            observe(
                cli,
                [
                    "--no-config",
                    str(fixtures / f"normal.{format_name}"),
                    "--format",
                    format_name,
                    "--asset-mode",
                    "embed",
                    "--quiet",
                ],
                fixtures,
                home / f"{format_name}-{iteration}",
            )
            for iteration in range(iterations)
        ]
        summary = summarized(observations)
        report[format_name] = summary
        if summary["exitCodes"] != [0]:
            failures.append(f"{format_name} conversion did not succeed")
        if len(summary["stdoutSha256"]) != 1:
            failures.append(f"{format_name} output was not byte-deterministic")
    return report, failures


def batch_conversion(
    cli: pathlib.Path,
    fixtures: pathlib.Path,
    root: pathlib.Path,
    jobs: int,
) -> Observation:
    output = root / f"output-{jobs}"
    output.mkdir(parents=True)
    arguments = ["--no-config", *[str(fixtures / f"normal.{name}") for name in FORMATS]]
    arguments.extend(["--output-dir", str(output), "--jobs", str(jobs), "--quiet"])
    return observe(cli, arguments, fixtures, root / f"home-{jobs}")


def executable_report(
    cli: pathlib.Path,
    fixtures: pathlib.Path,
    root: pathlib.Path,
    iterations: int,
    parallelism: int,
) -> tuple[dict[str, object], list[str]]:
    cold = [observe(cli, ["--version"], fixtures, root / f"cold-{i}") for i in range(iterations)]
    conversions, failures = individual_conversions(cli, fixtures, root / "individual", iterations)
    serial = batch_conversion(cli, fixtures, root / "serial", 1)
    parallel = batch_conversion(cli, fixtures, root / "parallel", parallelism)
    if serial.returncode != 0:
        failures.append("serial batch conversion failed")
    if parallel.returncode != 0:
        failures.append("parallel batch conversion failed")
    return (
        {
            "coreExecutableBytes": cli.stat().st_size,
            "coldStart": summarized(cold),
            "individualConversions": conversions,
            "warmSerialBatch": summarized([serial]),
            "concurrentBatch": {
                **summarized([parallel]),
                "jobs": parallelism,
                "documentsPerSecond": round(len(FORMATS) / (parallel.elapsed_ms / 1000), 3),
            },
        },
        failures,
    )


def baseline_report(
    cli: pathlib.Path,
    fixtures: pathlib.Path,
    root: pathlib.Path,
    iterations: int,
) -> dict[str, object]:
    cold = [observe(cli, ["--version"], fixtures, root / f"cold-{i}") for i in range(iterations)]
    formats: dict[str, object] = {}
    for format_name in FORMATS:
        result = observe(
            cli,
            [
                "--no-config",
                str(fixtures / f"normal.{format_name}"),
                "--format",
                format_name,
                "--asset-mode",
                "embed",
                "--quiet",
            ],
            fixtures,
            root / format_name,
        )
        formats[format_name] = summarized([result])
    available = all(metric["exitCodes"] == [0] for metric in formats.values())
    note = (
        "The baseline Core converted the corpus directly; availability is recorded but latency is not compared across different parser implementations."
        if available
        else "The baseline Core required an optional runtime, so conversion latency is recorded only as availability evidence and is not compared to native conversion."
    )
    return {
        "coreExecutableBytes": cli.stat().st_size,
        "coldStart": summarized(cold),
        "legacyConversionAvailability": formats,
        "note": note,
    }


def gate(candidate: dict[str, object], baseline: dict[str, object]) -> tuple[list[dict[str, object]], bool]:
    checks: list[dict[str, object]] = []

    def add(name: str, passed: bool, detail: str) -> None:
        checks.append({"name": name, "passed": passed, "detail": detail})

    size = int(candidate["coreExecutableBytes"])
    baseline_size = int(baseline["coreExecutableBytes"])
    add(
        "core-executable-size",
        size <= baseline_size + 8 * MIB,
        f"candidate={size}, baseline={baseline_size}, allowedGrowth={8 * MIB}",
    )
    cold = float(candidate["coldStart"]["medianMillis"])
    baseline_cold = float(baseline["coldStart"]["medianMillis"])
    cold_limit = max(baseline_cold * 2, baseline_cold + 150)
    add("cold-start", cold <= cold_limit, f"candidate={cold:.3f}ms, limit={cold_limit:.3f}ms")
    for format_name in FORMATS:
        metric = candidate["individualConversions"][format_name]
        elapsed = float(metric["maximumMillis"])
        peak = metric["peakRssBytes"]
        add(
            f"{format_name}-latency",
            elapsed <= 2_000,
            f"maximum={elapsed:.3f}ms, limit=2000ms",
        )
        add(
            f"{format_name}-peak-rss",
            peak is not None and int(peak) <= 512 * MIB,
            f"peak={peak}, limit={512 * MIB}",
        )
    serial = float(candidate["warmSerialBatch"]["maximumMillis"])
    parallel = float(candidate["concurrentBatch"]["maximumMillis"])
    add(
        "concurrent-throughput",
        parallel <= serial * 2,
        f"parallel={parallel:.3f}ms, serial={serial:.3f}ms, limit={serial * 2:.3f}ms",
    )
    return checks, all(check["passed"] for check in checks)


def main() -> int:
    args = parse_args()
    if args.iterations < 3 or not 2 <= args.parallelism <= len(FORMATS):
        raise SystemExit("iterations must be >= 3 and parallelism must be between 2 and 3")
    baseline = args.baseline_cli.resolve(strict=True)
    candidate = args.candidate_cli.resolve(strict=True)
    fixtures = args.fixtures.resolve(strict=True)
    output = args.output.resolve(strict=False)
    if output.exists() or not output.parent.is_dir():
        raise SystemExit("output must be a new file in an existing directory")
    for format_name in FORMATS:
        if not (fixtures / f"normal.{format_name}").is_file():
            raise SystemExit(f"missing normal.{format_name}")
    with tempfile.TemporaryDirectory(prefix="into-md-office-performance-") as temporary:
        root = pathlib.Path(temporary)
        baseline_metrics = baseline_report(baseline, fixtures, root / "baseline", args.iterations)
        candidate_metrics, failures = executable_report(
            candidate, fixtures, root / "candidate", args.iterations, args.parallelism
        )
    checks, checks_passed = gate(candidate_metrics, baseline_metrics)
    report = {
        "schemaVersion": 1,
        "corpus": "repository-authored-office-97-2003",
        "baseline": baseline_metrics,
        "candidate": candidate_metrics,
        "gates": checks,
        "failures": failures,
        "passed": checks_passed and not failures,
    }
    temporary_output = output.with_name(f".{output.name}.tmp")
    temporary_output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    temporary_output.replace(output)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
