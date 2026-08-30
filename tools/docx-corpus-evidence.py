#!/usr/bin/env python3
"""Build and measure a conversion-independent DOCX compatibility corpus.

Classification is deliberately completed before any converter is invoked. A
conversion failure is an observation and can never mutate an item's package
classification.
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import pathlib
import posixpath
import statistics
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ET
import zipfile
from dataclasses import dataclass
from typing import Any
from urllib.parse import unquote


SCHEMA_VERSION = 1
CORPUS_SIZE = 250
MIB = 1024 * 1024
LIMITS = {
    "maxInputBytes": 512 * MIB,
    "maxDecompressedBytes": 1024 * MIB,
    "maxArchiveEntries": 100_000,
    "maxArchiveEntryBytes": 256 * MIB,
    "maxArchiveCompressionRatio": 100,
    "maxNestingDepth": 256,
    "maxTableRows": 100_000,
    "maxTableColumns": 16_384,
    "maxTableCells": 1_000_000,
}
PACKAGE_REL = "http://schemas.openxmlformats.org/package/2006/relationships"
CONTENT_TYPES_NS = "http://schemas.openxmlformats.org/package/2006/content-types"
OFFICE_REL = (
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
)
STRICT_OFFICE_REL = "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument"
OFFICE_REL_TYPES = {OFFICE_REL, STRICT_OFFICE_REL}
WORD_MAIN_TYPES = {
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
    "application/vnd.ms-word.document.macroEnabled.main+xml",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.template.main+xml",
    "application/vnd.ms-word.template.macroEnabledTemplate.main+xml",
}
WORD_NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
STRICT_WORD_NS = "http://purl.oclc.org/ooxml/wordprocessingml/main"
WORD_NAMESPACES = {WORD_NS, STRICT_WORD_NS}
ALT_CHUNK_REL = (
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/aFChunk"
)
STRICT_ALT_CHUNK_REL = "http://purl.oclc.org/ooxml/officeDocument/relationships/aFChunk"
ALT_CHUNK_REL_TYPES = {ALT_CHUNK_REL, STRICT_ALT_CHUNK_REL}


@dataclass(frozen=True)
class Observation:
    elapsed_ms: float
    peak_rss_bytes: int | None
    returncode: int
    payload: dict[str, Any] | None
    stderr_sha256: str


class PolicyRejected(ValueError):
    """The package is parseable enough to establish a pre-conversion policy rejection."""


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus-root", type=pathlib.Path, required=True)
    parser.add_argument("--manifest", type=pathlib.Path, required=True)
    parser.add_argument("--write-manifest", action="store_true")
    parser.add_argument("--baseline-probe", type=pathlib.Path)
    parser.add_argument(
        "--baseline-report",
        type=pathlib.Path,
        help="reuse the baseline section from a verified earlier paired report",
    )
    parser.add_argument("--candidate-probe", type=pathlib.Path)
    parser.add_argument("--report", type=pathlib.Path)
    parser.add_argument("--timeout-seconds", type=float, default=30.0)
    parser.add_argument("--iterations", type=int, default=3)
    return parser.parse_args()


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_member(name: str) -> str | None:
    if not name or "\\" in name or name.startswith(("/", "\\")):
        return None
    parts = name.split("/")
    if any(part in ("", ".", "..") for part in parts):
        return None
    if ":" in parts[0] or any("\x00" in part for part in parts):
        return None
    return "/".join(parts)


def canonical_relationship_target(target: str) -> str | None:
    if target.startswith("//"):
        return None
    decoded = unquote(target)
    if decoded.startswith("/"):
        decoded = decoded[1:]
    return canonical_member(decoded)


def xml_metrics(data: bytes) -> tuple[int, int, int, int, int]:
    lowered = data.lower()
    if b"<!doctype" in lowered or b"<!entity" in lowered:
        raise PolicyRejected("dtdOrEntityDeclaration")
    depth = rows = cells = columns = word_text = 0
    parser = ET.XMLPullParser(events=("start", "end"))
    parser.feed(data)
    current = 0
    for event, element in parser.read_events():
        if event == "start":
            current += 1
            depth = max(depth, current)
            namespace, _, local = element.tag.removeprefix("{").partition("}")
            if namespace in WORD_NAMESPACES and local == "tr":
                rows += 1
            elif namespace in WORD_NAMESPACES and local == "tc":
                cells += 1
            elif namespace in WORD_NAMESPACES and local == "gridSpan":
                value = next(
                    (
                        candidate
                        for key, candidate in element.attrib.items()
                        if key in {f"{{{word_namespace}}}val" for word_namespace in WORD_NAMESPACES}
                    ),
                    "1",
                )
                columns = max(columns, int(value))
        else:
            namespace, _, local = element.tag.removeprefix("{").partition("}")
            if namespace in WORD_NAMESPACES and local in {"t", "delText", "instrText"}:
                word_text += len(element.text or "")
            current -= 1
            element.clear()
    if current != 0:
        raise ValueError("unbalancedXml")
    return depth, rows, cells, columns, word_text


def relationship_owner(part: str) -> str | None:
    if part == "_rels/.rels":
        return ""
    directory, filename = posixpath.split(part)
    if posixpath.basename(directory) != "_rels" or not filename.endswith(".rels"):
        return None
    owner_directory = posixpath.dirname(directory)
    owner_filename = filename[: -len(".rels")]
    return posixpath.join(owner_directory, owner_filename)


def resolve_relationship_target(owner: str, target: str) -> str | None:
    if target.startswith("/"):
        return canonical_relationship_target(target)
    joined = posixpath.normpath(posixpath.join(posixpath.dirname(owner), unquote(target)))
    return canonical_member(joined)


def content_types(root: ET.Element) -> tuple[dict[str, str], dict[str, str]]:
    if root.tag != f"{{{CONTENT_TYPES_NS}}}Types":
        raise ValueError("invalidContentTypesRootQName")
    defaults: dict[str, str] = {}
    overrides: dict[str, str] = {}
    for child in root:
        if child.tag == f"{{{CONTENT_TYPES_NS}}}Default":
            key = child.attrib.get("Extension", "").lower()
            if not key or key in defaults:
                raise ValueError("duplicateOrEmptyContentTypeDefault")
            defaults[key] = child.attrib.get("ContentType", "")
        elif child.tag == f"{{{CONTENT_TYPES_NS}}}Override":
            key = child.attrib.get("PartName", "").lstrip("/")
            if not key or key in overrides:
                raise ValueError("duplicateOrEmptyContentTypeOverride")
            overrides[key] = child.attrib.get("ContentType", "")
        else:
            raise ValueError("invalidContentTypeChildQName")
    return defaults, overrides


def validate_relationship_root(root: ET.Element) -> None:
    if root.tag != f"{{{PACKAGE_REL}}}Relationships":
        raise ValueError("invalidRelationshipsRootQName")


def classify(path: pathlib.Path) -> dict[str, Any]:
    evidence: dict[str, Any] = {
        "category": "invalidPackage",
        "reason": "uninspected",
        "archiveEntries": None,
        "decompressedBytes": None,
        "maximumXmlDepth": None,
        "tableRows": None,
        "tableCells": None,
        "maximumGridSpan": None,
        "externalRelationships": 0,
        "activeParts": 0,
        "altChunkRelationships": 0,
        "altChunkInternalBytes": 0,
        "wordTextCharacters": 0,
    }
    size = path.stat().st_size
    if size > LIMITS["maxInputBytes"]:
        evidence.update(category="defaultStructuralHardLimit", reason="maxInputBytes")
        return evidence
    with path.open("rb") as source:
        prefix = source.read(8)
    if prefix == b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1":
        evidence.update(category="policyRejected", reason="compoundContainerNotAllowed")
        return evidence
    if not prefix.startswith((b"PK\x03\x04", b"PK\x05\x06", b"PK\x07\x08")):
        evidence.update(category="invalidPackage", reason="nonZipContainer")
        return evidence
    try:
        with zipfile.ZipFile(path) as archive:
            entries = archive.infolist()
            evidence["archiveEntries"] = len(entries)
            if len(entries) > LIMITS["maxArchiveEntries"]:
                evidence.update(
                    category="defaultStructuralHardLimit", reason="maxArchiveEntries"
                )
                return evidence
            seen: set[str] = set()
            entries_by_name: dict[str, zipfile.ZipInfo] = {}
            expanded = 0
            policy_reason: str | None = None
            hard_reason: str | None = None
            for entry in entries:
                if entry.is_dir():
                    continue
                name = canonical_member(entry.filename)
                if name is None:
                    policy_reason = policy_reason or "unsafePartName"
                    continue
                folded = name.casefold()
                if folded in seen:
                    policy_reason = policy_reason or "duplicateCanonicalPart"
                seen.add(folded)
                entries_by_name[folded] = entry
                if entry.flag_bits & 1:
                    policy_reason = policy_reason or "encryptedMember"
                unix_mode = (entry.external_attr >> 16) & 0o170000
                if unix_mode == 0o120000:
                    policy_reason = policy_reason or "symbolicLinkMember"
                expanded += entry.file_size
                if entry.file_size > LIMITS["maxArchiveEntryBytes"]:
                    hard_reason = hard_reason or "maxArchiveEntryBytes"
                ratio = entry.file_size / max(entry.compress_size, 1)
                if ratio > LIMITS["maxArchiveCompressionRatio"]:
                    policy_reason = policy_reason or "maxArchiveCompressionRatio"
            evidence["decompressedBytes"] = expanded
            if policy_reason:
                evidence.update(category="policyRejected", reason=policy_reason)
                return evidence
            if expanded > LIMITS["maxDecompressedBytes"]:
                hard_reason = hard_reason or "maxDecompressedBytes"
            if hard_reason:
                evidence.update(category="defaultStructuralHardLimit", reason=hard_reason)
                return evidence
            required = {"[content_types].xml", "_rels/.rels"}
            if not required.issubset(seen):
                evidence["reason"] = "requiredPackagePartMissing"
                return evidence
            types_bytes = archive.read("[Content_Types].xml")
            root = ET.fromstring(types_bytes)
            defaults, overrides = content_types(root)
            rel_root = ET.fromstring(archive.read("_rels/.rels"))
            validate_relationship_root(rel_root)
            main_targets: list[str] = []
            for relation in rel_root:
                if relation.tag != f"{{{PACKAGE_REL}}}Relationship":
                    continue
                if relation.attrib.get("Type") in OFFICE_REL_TYPES:
                    if relation.attrib.get("TargetMode", "").lower() == "external":
                        evidence["reason"] = "externalOfficeDocument"
                        return evidence
                    target = canonical_relationship_target(relation.attrib.get("Target", ""))
                    if target:
                        main_targets.append(target)
            if len(main_targets) != 1:
                evidence["reason"] = "ambiguousOfficeDocument"
                return evidence
            main = main_targets[0]
            extension = pathlib.PurePosixPath(main).suffix.lstrip(".").lower()
            main_type = overrides.get(main, defaults.get(extension))
            if main_type not in WORD_MAIN_TYPES or main.casefold() not in seen:
                evidence["reason"] = "nonWordMainPart"
                return evidence
            maximum_depth = rows = cells = maximum_span = word_text = 0
            for entry in entries:
                canonical = canonical_member(entry.filename)
                if canonical is None:
                    continue
                lower = canonical.lower()
                media_type = overrides.get(canonical)
                extension = pathlib.PurePosixPath(canonical).suffix.lstrip(".").lower()
                media_type = media_type or defaults.get(extension, "")
                if any(token in media_type.lower() for token in ("vba", "activex", "macro")):
                    evidence["activeParts"] += 1
                if not (
                    lower.endswith((".xml", ".rels"))
                    or media_type.lower() == "application/xhtml+xml"
                ):
                    continue
                data = archive.read(entry)
                depth, part_rows, part_cells, part_span, part_text = xml_metrics(data)
                maximum_depth = max(maximum_depth, depth)
                rows += part_rows
                cells += part_cells
                maximum_span = max(maximum_span, part_span)
                word_text += part_text
                if lower.endswith(".rels"):
                    owner = relationship_owner(canonical)
                    relation_root = ET.fromstring(data)
                    validate_relationship_root(relation_root)
                    for relation in relation_root:
                        if relation.tag != f"{{{PACKAGE_REL}}}Relationship":
                            continue
                        external = relation.attrib.get("TargetMode", "").lower() == "external"
                        if external:
                            evidence["externalRelationships"] += 1
                        if relation.attrib.get("Type") not in ALT_CHUNK_REL_TYPES:
                            continue
                        evidence["altChunkRelationships"] += 1
                        if not external and owner is not None:
                            target = resolve_relationship_target(
                                owner, relation.attrib.get("Target", "")
                            )
                            target_entry = entries_by_name.get((target or "").casefold())
                            if target_entry:
                                evidence["altChunkInternalBytes"] += target_entry.file_size
                if canonical.casefold() == main.casefold():
                    main_root = ET.fromstring(data)
                    if main_root.tag not in {
                        f"{{{word_namespace}}}document" for word_namespace in WORD_NAMESPACES
                    }:
                        raise ValueError("invalidWordMainRootQName")
                    bodies = [
                        child
                        for child in main_root
                        if child.tag
                        in {f"{{{word_namespace}}}body" for word_namespace in WORD_NAMESPACES}
                    ]
                    if len(bodies) != 1:
                        raise ValueError("wordMainBodyCount")
            evidence.update(
                maximumXmlDepth=maximum_depth,
                tableRows=rows,
                tableCells=cells,
                maximumGridSpan=maximum_span,
                wordTextCharacters=word_text,
            )
            if maximum_depth > LIMITS["maxNestingDepth"]:
                evidence.update(
                    category="defaultStructuralHardLimit", reason="maxNestingDepth"
                )
            elif rows > LIMITS["maxTableRows"]:
                evidence.update(category="defaultStructuralHardLimit", reason="maxTableRows")
            elif cells > LIMITS["maxTableCells"]:
                evidence.update(category="defaultStructuralHardLimit", reason="maxTableCells")
            elif maximum_span > LIMITS["maxTableColumns"]:
                evidence.update(
                    category="defaultStructuralHardLimit", reason="maxTableColumns"
                )
            else:
                evidence.update(category="validPolicyAllowed", reason="packagePreflightPassed")
    except PolicyRejected as error:
        evidence.update(category="policyRejected", reason=str(error))
    except zipfile.BadZipFile:
        evidence.update(category="invalidPackage", reason="badZipFile")
    except ET.ParseError:
        evidence.update(category="invalidPackage", reason="xmlParseError")
    except KeyError:
        evidence.update(category="invalidPackage", reason="missingZipEntry")
    except ValueError as error:
        known = {
            "unbalancedXml",
            "invalidContentTypesRootQName",
            "duplicateOrEmptyContentTypeDefault",
            "duplicateOrEmptyContentTypeOverride",
            "invalidContentTypeChildQName",
            "invalidRelationshipsRootQName",
            "invalidWordMainRootQName",
            "wordMainBodyCount",
        }
        reason = str(error) if str(error) in known else "invalidXmlValue"
        evidence.update(category="invalidPackage", reason=reason)
    except OSError:
        evidence.update(category="invalidPackage", reason="packageIoError")
    return evidence


def discover(root: pathlib.Path) -> list[pathlib.Path]:
    candidates = sorted(
        (path for path in root.rglob("*.docx") if path.is_file()),
        key=lambda path: path.relative_to(root).as_posix().casefold(),
    )
    selected = candidates[:CORPUS_SIZE]
    if len(selected) != CORPUS_SIZE:
        raise SystemExit(f"need {CORPUS_SIZE} DOCX paths, found {len(selected)}")
    return selected


def write_manifest(root: pathlib.Path, destination: pathlib.Path) -> None:
    files = discover(root)
    items = []
    payloads: dict[str, list[str]] = {}
    for path in files:
        digest = sha256(path)
        relative = path.relative_to(root).as_posix()
        payloads.setdefault(digest, []).append(relative)
        items.append(
            {
                "path": relative,
                "bytes": path.stat().st_size,
                "sha256": digest,
                "classification": classify(path),
            }
        )
    counts: dict[str, int] = {}
    for item in items:
        category = item["classification"]["category"]
        counts[category] = counts.get(category, 0) + 1
    manifest = {
        "schemaVersion": SCHEMA_VERSION,
        "selection": {
            "algorithm": "case-folded-relative-path-order, first 250 regular DOCX paths",
            "rawFiles": len(items),
            "uniquePayloads": len(payloads),
            "duplicatePayloadGroups": [
                {"sha256": digest, "paths": paths}
                for digest, paths in sorted(payloads.items())
                if len(paths) > 1
            ],
        },
        "defaultLimits": LIMITS,
        "classificationCounts": counts,
        "items": items,
    }
    atomic_json(destination, manifest)


def load_manifest(root: pathlib.Path, path: pathlib.Path) -> dict[str, Any]:
    manifest = json.loads(path.read_text(encoding="utf-8"))
    if manifest.get("schemaVersion") != SCHEMA_VERSION:
        raise SystemExit("unsupported manifest schema")
    items = manifest.get("items", [])
    if len(items) != CORPUS_SIZE:
        raise SystemExit(f"manifest must contain exactly {CORPUS_SIZE} items")
    for item in items:
        source = (root / item["path"]).resolve(strict=True)
        if not source.is_relative_to(root):
            raise SystemExit(f"manifest path escapes corpus root: {item['path']}")
        if sha256(source) != item["sha256"] or source.stat().st_size != item["bytes"]:
            raise SystemExit(f"manifest authority mismatch: {item['path']}")
        current = classify(source)
        if current != item["classification"]:
            raise SystemExit(f"pre-conversion classification drift: {item['path']}")
    return manifest


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
    ok = ctypes.windll.psapi.GetProcessMemoryInfo(
        ctypes.c_void_p(process._handle), ctypes.byref(counters), counters.cb
    )
    return int(counters.PeakWorkingSetSize) if ok else None


def resident_bytes(process: subprocess.Popen[bytes]) -> int | None:
    if sys.platform == "win32":
        return windows_rss(process)
    status = pathlib.Path(f"/proc/{process.pid}/status")
    if status.exists():
        for line in status.read_text(encoding="ascii").splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    return None


def observe(probe: pathlib.Path, source: pathlib.Path, timeout: float) -> Observation:
    with tempfile.TemporaryDirectory(prefix="into-md-docx-probe-") as temporary:
        started = time.perf_counter()
        process = subprocess.Popen(
            [str(probe), str(source), temporary],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        peak = 0
        deadline = time.monotonic() + timeout
        while process.poll() is None:
            peak = max(peak, resident_bytes(process) or 0)
            if time.monotonic() >= deadline:
                process.kill()
                process.wait()
                raise RuntimeError(f"probe timed out: {source}")
            time.sleep(0.002)
        stdout, stderr = process.communicate()
        peak = max(peak, resident_bytes(process) or 0)
    payload = None
    if stdout:
        try:
            payload = json.loads(stdout)
        except json.JSONDecodeError:
            payload = None
    return Observation(
        elapsed_ms=(time.perf_counter() - started) * 1000,
        peak_rss_bytes=peak or None,
        returncode=process.returncode,
        payload=payload,
        stderr_sha256=hashlib.sha256(stderr).hexdigest(),
    )


def run_probe(
    probe: pathlib.Path,
    root: pathlib.Path,
    manifest: dict[str, Any],
    timeout: float,
    iterations: int,
) -> dict[str, Any]:
    observations = []
    for item in manifest["items"]:
        source = root / item["path"]
        measured = [observe(probe, source, timeout) for _ in range(iterations)]
        observations.append(observation_item(item, measured, iterations))
    return summarize_probe(probe, observations)


def observation_item(
    item: dict[str, Any], measured: list[Observation], iterations: int
) -> dict[str, Any]:
    exit_codes = {run.returncode for run in measured}
    payload_signatures = {
        json.dumps(run.payload, sort_keys=True, separators=(",", ":")) for run in measured
    }
    if len(exit_codes) != 1 or len(payload_signatures) != 1:
        raise RuntimeError(f"probe is not deterministic: {item['path']}")
    elapsed = [run.elapsed_ms for run in measured]
    peaks = [run.peak_rss_bytes for run in measured if run.peak_rss_bytes is not None]
    selected = measured[0]
    return {
        "path": item["path"],
        "classification": item["classification"]["category"],
        "runs": iterations,
        "elapsedMillis": round(statistics.median(elapsed), 3),
        "runElapsedMillis": [round(value, 3) for value in elapsed],
        "peakRssBytes": max(peaks) if peaks else None,
        "exitCode": selected.returncode,
        "probe": selected.payload,
        "stderrSha256": sorted({run.stderr_sha256 for run in measured}),
    }


def summarize_probe(probe: pathlib.Path, observations: list[dict[str, Any]]) -> dict[str, Any]:
    successes = [item for item in observations if item["exitCode"] == 0]
    elapsed = [item["elapsedMillis"] for item in successes]
    return {
        "probeSha256": sha256(probe),
        "items": observations,
        "summary": {
            "raw": len(observations),
            "successful": len(successes),
            "failed": len(observations) - len(successes),
            "meanSuccessfulMillis": round(statistics.fmean(elapsed), 3) if elapsed else None,
            "medianSuccessfulMillis": round(statistics.median(elapsed), 3) if elapsed else None,
            "nonEmptyMarkdown": sum(
                1
                for item in successes
                if item["probe"] and item["probe"].get("markdownBytes", 0) > 0
            ),
        },
    }


def run_paired_probes(
    baseline_probe: pathlib.Path,
    candidate_probe: pathlib.Path,
    root: pathlib.Path,
    manifest: dict[str, Any],
    timeout: float,
    iterations: int,
) -> tuple[dict[str, Any], dict[str, Any]]:
    baseline_items = []
    candidate_items = []
    for item in manifest["items"]:
        source = root / item["path"]
        baseline_runs = []
        candidate_runs = []
        for iteration in range(iterations):
            if iteration % 2 == 0:
                baseline_runs.append(observe(baseline_probe, source, timeout))
                candidate_runs.append(observe(candidate_probe, source, timeout))
            else:
                candidate_runs.append(observe(candidate_probe, source, timeout))
                baseline_runs.append(observe(baseline_probe, source, timeout))
        baseline_items.append(observation_item(item, baseline_runs, iterations))
        candidate_items.append(observation_item(item, candidate_runs, iterations))
    return (
        summarize_probe(baseline_probe, baseline_items),
        summarize_probe(candidate_probe, candidate_items),
    )


def comparison(baseline: dict[str, Any], candidate: dict[str, Any]) -> dict[str, Any]:
    before = {item["path"]: item for item in baseline["items"] if item["exitCode"] == 0}
    after = {item["path"]: item for item in candidate["items"] if item["exitCode"] == 0}
    common = sorted(before.keys() & after.keys())
    baseline_mean = statistics.fmean(before[path]["elapsedMillis"] for path in common)
    candidate_mean = statistics.fmean(after[path]["elapsedMillis"] for path in common)
    fallback = (candidate_mean / baseline_mean - 1.0) if baseline_mean else None
    return {
        "commonSuccessful": len(common),
        "baselineMeanMillis": round(baseline_mean, 3),
        "candidateMeanMillis": round(candidate_mean, 3),
        "meanFallbackFraction": round(fallback, 6) if fallback is not None else None,
        "underFiftyPercentGate": fallback is not None and fallback < 0.5,
    }


def atomic_json(path: pathlib.Path, value: dict[str, Any]) -> None:
    if not path.parent.is_dir():
        raise SystemExit(f"output parent does not exist: {path.parent}")
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )
    temporary.replace(path)


def main() -> int:
    args = arguments()
    if args.iterations < 1:
        raise SystemExit("--iterations must be positive")
    root = args.corpus_root.resolve(strict=True)
    manifest_path = args.manifest.resolve(strict=False)
    if args.write_manifest:
        write_manifest(root, manifest_path)
    manifest = load_manifest(root, manifest_path)
    if args.baseline_probe and args.baseline_report:
        raise SystemExit("--baseline-probe and --baseline-report are mutually exclusive")
    if not args.baseline_probe and not args.baseline_report:
        return 0
    if not args.report:
        raise SystemExit("--report is required when a probe is supplied")
    paired_candidate = None
    if args.baseline_probe and args.candidate_probe:
        baseline, paired_candidate = run_paired_probes(
            args.baseline_probe.resolve(strict=True),
            args.candidate_probe.resolve(strict=True),
            root,
            manifest,
            args.timeout_seconds,
            args.iterations,
        )
    elif args.baseline_report:
        previous = json.loads(args.baseline_report.resolve(strict=True).read_text(encoding="utf-8"))
        if previous.get("manifestSha256") != sha256(manifest_path):
            raise SystemExit("baseline report manifest authority differs")
        baseline = previous.get("baseline")
        if not isinstance(baseline, dict) or len(baseline.get("items", [])) != CORPUS_SIZE:
            raise SystemExit("baseline report does not contain a complete baseline")
    else:
        baseline = run_probe(
            args.baseline_probe.resolve(strict=True),
            root,
            manifest,
            args.timeout_seconds,
            args.iterations,
        )
    candidate = paired_candidate
    compared = None
    if args.candidate_probe and candidate is None:
        candidate = run_probe(
            args.candidate_probe.resolve(strict=True),
            root,
            manifest,
            args.timeout_seconds,
            args.iterations,
        )
    if candidate is not None:
        compared = comparison(baseline, candidate)
    report = {
        "schemaVersion": SCHEMA_VERSION,
        "manifestSha256": sha256(manifest_path),
        "classificationCounts": manifest["classificationCounts"],
        "baseline": baseline,
        "candidate": candidate,
        "comparison": compared,
    }
    atomic_json(args.report.resolve(strict=False), report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
