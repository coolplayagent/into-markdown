"""Cross-format executable, latency, throughput, and RSS gates."""

from __future__ import annotations

from .constants import FORMATS, MIB


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
        baseline_metric = baseline["individualConversions"][format_name]
        elapsed = float(metric["maximumMillis"])
        peak = metric["peakRssBytes"]
        baseline_median = float(baseline_metric["medianMillis"])
        candidate_median = float(metric["medianMillis"])
        latency_regression = candidate_median / baseline_median - 1
        baseline_peak = baseline_metric["peakRssBytes"]
        rss_regression = (
            int(peak) / int(baseline_peak) - 1
            if peak is not None and baseline_peak is not None and int(baseline_peak) > 0
            else float("inf")
        )
        candidate_temp = int(metric["peakTemporaryBytes"])
        baseline_temp = int(baseline_metric["peakTemporaryBytes"])
        temp_relative = (
            candidate_temp / baseline_temp - 1
            if baseline_temp > 0
            else (0.0 if candidate_temp == 0 else float("inf"))
        )
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
        add(
            f"{format_name}-relative-latency",
            latency_regression < 0.5,
            (
                f"baselineMedian={baseline_median:.3f}ms, "
                f"candidateMedian={candidate_median:.3f}ms, "
                f"regression={latency_regression:.3%}, limit=50.000%"
            ),
        )
        add(
            f"{format_name}-relative-rss",
            rss_regression < 0.5,
            (
                f"baselinePeak={baseline_peak}, candidatePeak={peak}, "
                f"regression={rss_regression:.3%}, limit=50.000%"
            ),
        )
        add(
            f"{format_name}-temporary-storage",
            candidate_temp <= 2 * 1024 * MIB
            and temp_relative < 0.5
            and int(metric["maximumTemporaryBytesAfter"]) == 0,
            (
                f"baselinePeak={baseline_temp}, candidatePeak={candidate_temp}, "
                f"regression={temp_relative:.3%}, absoluteLimit={2 * 1024 * MIB}, "
                f"after={metric['maximumTemporaryBytesAfter']}"
            ),
        )
    serial = float(candidate["warmSerialBatch"]["maximumMillis"])
    parallel = float(candidate["concurrentBatch"]["maximumMillis"])
    add(
        "concurrent-throughput",
        parallel <= serial * 2,
        f"parallel={parallel:.3f}ms, serial={serial:.3f}ms, limit={serial * 2:.3f}ms",
    )
    return checks, all(check["passed"] for check in checks)
