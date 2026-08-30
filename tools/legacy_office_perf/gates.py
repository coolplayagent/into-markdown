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

