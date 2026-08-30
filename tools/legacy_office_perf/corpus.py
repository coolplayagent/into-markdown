"""Alternating XLS corpus runner, summaries, and blocking checks."""

from __future__ import annotations

import pathlib
import statistics

from .constants import MIB
from .monitor import XlsObservation, file_sha256, observe_xls
from .oracle import verify_xls_oracle


def summarize_xls_file(
    item: dict[str, object],
    observations: list[XlsObservation],
    oracle: dict[str, object] | None,
) -> dict[str, object]:
    successful = [
        observation
        for observation in observations
        if observation.returncode == 0 and observation.output_bytes > 0
    ]
    output_hashes = sorted(
        {observation.output_sha256 for observation in successful if observation.output_sha256}
    )
    structured_failures = []
    for observation in observations:
        report_items = observation.report.get("items") if observation.report else None
        report_item = report_items[0] if isinstance(report_items, list) and len(report_items) == 1 else None
        structured_failures.append(
            isinstance(report_item, dict)
            and report_item.get("status") == "failed"
            and report_item.get("errorCode") == "resourceLimit"
            and report_item.get("reasonCode") == "documentNodes"
            and isinstance(report_item.get("limit"), dict)
            and report_item["limit"].get("name") == "documentNodes"
            and observation.output_bytes == 0
            and not observation.residual_paths
            and observation.temporary_bytes_after == 0
        )
    hard_limit = not successful and len(observations) > 0 and all(structured_failures)
    quality = (
        verify_xls_oracle(successful[0].output_path, oracle)
        if successful and oracle is not None
        else None
    )
    return {
        "file": item["file"],
        "valid": item["valid"],
        "container": item["container"],
        "runs": len(observations),
        "successfulRuns": len(successful),
        "nonEmptyRuns": sum(observation.output_bytes > 0 for observation in observations),
        "deterministic": len(successful) == len(observations) and len(output_hashes) == 1,
        "hardLimit": hard_limit,
        "structuredDocumentNodesRuns": sum(structured_failures),
        "qualityOracle": quality,
        "meanMillis": round(statistics.mean(o.elapsed_ms for o in observations), 3),
        "maximumMillis": round(max(o.elapsed_ms for o in observations), 3),
        "peakRssBytes": max((o.peak_rss_bytes or 0) for o in observations) or None,
        "peakTemporaryBytes": max(o.peak_temporary_bytes for o in observations),
        "temporaryBytesAfter": max(o.temporary_bytes_after for o in observations),
        "residualPaths": sorted({path for o in observations for path in o.residual_paths}),
        "exitCodes": sorted({o.returncode for o in observations}),
        "outputSha256": output_hashes,
        "failureEvidence": sorted(
            {o.stderr.strip() for o in observations if o.returncode != 0 and o.stderr.strip()}
        ),
    }


def summarize_xls_corpus(
    cli: pathlib.Path,
    authority: list[dict[str, object]],
    observations_by_file: dict[str, list[XlsObservation]],
    oracle: dict[str, dict[str, object]] | None,
) -> dict[str, object]:
    items = []
    for item in authority:
        name = str(item["file"])
        observations = observations_by_file[name]
        items.append(summarize_xls_file(item, observations, oracle.get(name) if oracle else None))
    passed = {item["file"] for item in items if item["deterministic"]}
    valid = {str(item["file"]) for item in authority if item["valid"] is True}
    hard_limits = {item["file"] for item in items if item["valid"] and item["hardLimit"]}
    return {
        "binary": str(cli),
        "binarySha256": file_sha256(cli),
        "rawPass": {"passed": len(passed), "total": len(items)},
        "validPass": {"passed": len(passed & valid), "total": len(valid)},
        "validHardLimits": sorted(hard_limits),
        "maximumPeakRssBytes": max((item["peakRssBytes"] or 0) for item in items) or None,
        "maximumPeakTemporaryBytes": max(item["peakTemporaryBytes"] for item in items),
        "maximumTemporaryBytesAfter": max(item["temporaryBytesAfter"] for item in items),
        "items": items,
    }


def xls_corpus_report(
    baseline_cli: pathlib.Path,
    candidate_cli: pathlib.Path,
    corpus: pathlib.Path,
    authority: list[dict[str, object]],
    root: pathlib.Path,
    iterations: int,
    regression_limit: float,
    oracle: dict[str, dict[str, object]],
) -> tuple[dict[str, object], list[dict[str, object]]]:
    warm_item = next(item for item in authority if item["valid"] is True)
    warm_source = corpus / str(warm_item["file"])
    observe_xls(baseline_cli, warm_source, corpus, root / "warmup" / "baseline")
    observe_xls(candidate_cli, warm_source, corpus, root / "warmup" / "candidate")
    baseline_observations = {str(item["file"]): [] for item in authority}
    candidate_observations = {str(item["file"]): [] for item in authority}
    for file_index, item in enumerate(authority):
        name = str(item["file"])
        for iteration in range(iterations):
            order = (
                (
                    ("baseline", baseline_cli, baseline_observations),
                    ("candidate", candidate_cli, candidate_observations),
                )
                if (file_index + iteration) % 2 == 0
                else (
                    ("candidate", candidate_cli, candidate_observations),
                    ("baseline", baseline_cli, baseline_observations),
                )
            )
            for label, cli, destination in order:
                destination[name].append(
                    observe_xls(
                        cli,
                        corpus / name,
                        corpus,
                        root / "runs" / name / str(iteration) / label,
                    )
                )
    baseline = summarize_xls_corpus(
        baseline_cli, authority, baseline_observations, None
    )
    candidate = summarize_xls_corpus(
        candidate_cli, authority, candidate_observations, oracle
    )
    baseline_items = {item["file"]: item for item in baseline["items"]}
    candidate_items = {item["file"]: item for item in candidate["items"]}
    common = sorted(
        name
        for name in baseline_items.keys() & candidate_items.keys()
        if baseline_items[name]["deterministic"] and candidate_items[name]["deterministic"]
    )
    common_hash_matches = [
        name
        for name in common
        if baseline_items[name]["outputSha256"] == candidate_items[name]["outputSha256"]
    ]
    baseline_mean = statistics.mean(baseline_items[name]["meanMillis"] for name in common)
    candidate_mean = statistics.mean(candidate_items[name]["meanMillis"] for name in common)
    regression = candidate_mean / baseline_mean - 1
    valid_passed = int(candidate["validPass"]["passed"])
    valid_total = int(candidate["validPass"]["total"])
    hard_limits = len(candidate["validHardLimits"])
    oracle_verified = [
        item["file"]
        for item in candidate["items"]
        if item["valid"]
        and item["deterministic"]
        and item["qualityOracle"] is not None
        and item["qualityOracle"]["verified"]
    ]
    invalid_successes = [
        item["file"] for item in candidate["items"] if not item["valid"] and item["deterministic"]
    ]
    residuals = [item["file"] for item in candidate["items"] if item["residualPaths"]]
    checks = [
        {
            "name": "xls-valid-accounted",
            "passed": valid_passed == valid_total - 1
            and hard_limits == 1
            and valid_passed + hard_limits == valid_total,
            "detail": f"passed={valid_passed}, hardLimits={hard_limits}, total={valid_total}",
        },
        {
            "name": "xls-quality-oracle",
            "passed": len(oracle_verified) == valid_passed == 55,
            "detail": (
                f"verified={len(oracle_verified)}, successfulValid={valid_passed}, required=55"
            ),
        },
        {
            "name": "xls-invalid-inputs-fail-closed",
            "passed": not invalid_successes,
            "detail": f"invalidSuccesses={invalid_successes}",
        },
        {
            "name": "xls-common-success-byte-stability",
            "passed": len(common) == int(baseline["validPass"]["passed"])
            and len(common_hash_matches) == len(common),
            "detail": (
                f"common={len(common)}, byteIdentical={len(common_hash_matches)}, "
                f"baselineValid={baseline['validPass']['passed']}"
            ),
        },
        {
            "name": "xls-valid-coverage-not-regressed",
            "passed": valid_passed >= int(baseline["validPass"]["passed"]),
            "detail": (
                f"candidate={valid_passed}, baseline={baseline['validPass']['passed']}, "
                f"total={valid_total}"
            ),
        },
        {
            "name": "xls-common-success-mean-regression",
            "passed": regression < regression_limit,
            "detail": (
                f"common={len(common)}, baseline={baseline_mean:.3f}ms, "
                f"candidate={candidate_mean:.3f}ms, regression={regression:.3%}, "
                f"limit={regression_limit:.3%}"
            ),
        },
        {
            "name": "xls-peak-rss",
            "passed": candidate["maximumPeakRssBytes"] is not None
            and int(candidate["maximumPeakRssBytes"]) <= 2560 * MIB,
            "detail": (
                f"peak={candidate['maximumPeakRssBytes']}, "
                f"limit={2560 * MIB} (2 GiB shared budget + 512 MiB margin)"
            ),
        },
        {
            "name": "xls-peak-temporary-storage",
            "passed": int(candidate["maximumPeakTemporaryBytes"]) <= 2 * 1024 * MIB,
            "detail": (
                f"peak={candidate['maximumPeakTemporaryBytes']}, limit={2 * 1024 * MIB}"
            ),
        },
        {
            "name": "xls-temporary-cleanup",
            "passed": candidate["maximumTemporaryBytesAfter"] == 0 and not residuals,
            "detail": (
                f"maximumTemporaryBytesAfter={candidate['maximumTemporaryBytesAfter']}, "
                f"residualFiles={residuals}"
            ),
        },
    ]
    return (
        {
            "authority": "xlrd-2.0.2-eager-cell-read",
            "iterations": iterations,
            "measurementOrder": (
                "baseline and candidate warmed once, then alternated by file-and-iteration parity"
            ),
            "errorPolicy": "best-effort",
            "sharedLeaseTelemetry": {
                "status": "unavailable",
                "dependency": "#269",
                "evidence": (
                    "actual CompoundFile::open exact/limit-minus-one/cancellation/release tests; "
                    "process RSS is measured here"
                ),
            },
            "baseline": baseline,
            "candidate": candidate,
            "commonSuccess": {
                "files": common,
                "byteIdenticalFiles": common_hash_matches,
                "baselineMeanMillis": round(baseline_mean, 3),
                "candidateMeanMillis": round(candidate_mean, 3),
                "regressionFraction": round(regression, 6),
            },
        },
        checks,
    )

