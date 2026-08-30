"""Repository fixture and executable-level benchmark runner."""

from __future__ import annotations

import pathlib
import statistics

from .constants import FORMATS
from .monitor import Observation, observe


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

