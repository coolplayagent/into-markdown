#!/usr/bin/env python3
"""Validate, flatten, and inventory all target-native release artifacts."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import shutil
import sys

sys.path.append(str(pathlib.Path(__file__).resolve().parent))
from release_version import VersionError, validate_version


ROOT = pathlib.Path(__file__).resolve().parents[1]
TARGET_CORES = {
    "x86_64-unknown-linux-gnu": "into-md-linux-x86_64-core.tar.gz",
    "aarch64-unknown-linux-gnu": "into-md-linux-arm64-core.tar.gz",
    "x86_64-pc-windows-msvc": "into-md-windows-x86_64-core.zip",
    "aarch64-pc-windows-msvc": "into-md-windows-arm64-core.zip",
    "aarch64-apple-darwin": "into-md-macos-arm64-core.dmg",
}
TARGET_HOSTS = {
    "x86_64-unknown-linux-gnu": ("linux", "x86_64"),
    "aarch64-unknown-linux-gnu": ("linux", "aarch64"),
    "x86_64-pc-windows-msvc": ("windows", "x86_64"),
    "aarch64-pc-windows-msvc": ("windows", "aarch64"),
    "aarch64-apple-darwin": ("macos", "aarch64"),
}
PLUGIN_IDS = ("official.ocr.ppocrv6", "official.media.whisper")
GENERIC_REPORTS = frozenset(
    {"platform-audit.json", "installed-smoke.json", "platform-acceptance.json"}
)
SHA256 = re.compile(r"[0-9a-f]{64}")
METADATA_SUFFIXES = (".spdx.json", ".sources.json", ".THIRD_PARTY_NOTICES.md")


def digest(path: pathlib.Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def artifact_target(name: str) -> str | None:
    if name.startswith("modular-"):
        return name.removeprefix("modular-")
    if name == "into-md-macos-arm64-modular":
        return "aarch64-apple-darwin"
    if name == "into-markdown-agent-skill":
        return None
    raise RuntimeError(f"unexpected workflow artifact directory: {name}")


def copy_artifacts(source: pathlib.Path, output: pathlib.Path) -> None:
    if output.exists():
        raise RuntimeError("release staging output already exists")
    output.mkdir(parents=True)
    for artifact in sorted(source.iterdir()):
        if not artifact.is_dir() or artifact.is_symlink():
            raise RuntimeError(f"unsafe workflow artifact entry: {artifact.name}")
        target = artifact_target(artifact.name)
        if target is not None and target not in TARGET_CORES:
            raise RuntimeError(f"unsupported workflow artifact target: {target}")
        for candidate in sorted(artifact.rglob("*")):
            if candidate.is_symlink():
                raise RuntimeError(f"release artifact contains a link: {candidate}")
            if not candidate.is_file():
                continue
            name = candidate.name
            if target is not None and name in GENERIC_REPORTS:
                name = f"{target}-{name}"
            destination = output / name
            if destination.exists():
                raise RuntimeError(f"duplicate flat release asset: {name}")
            shutil.copyfile(candidate, destination)


def verify_sidecar(output: pathlib.Path, name: str) -> None:
    artifact = output / name
    sidecar = output / f"{name}.sha256"
    if not artifact.is_file() or not sidecar.is_file():
        raise RuntimeError(f"release artifact or SHA-256 sidecar is missing: {name}")
    fields = sidecar.read_text(encoding="ascii").strip().split()
    if len(fields) != 2 or fields[1] != name or SHA256.fullmatch(fields[0]) is None:
        raise RuntimeError(f"release SHA-256 sidecar is invalid: {sidecar.name}")
    if fields[0] != digest(artifact):
        raise RuntimeError(f"release SHA-256 sidecar disagrees: {sidecar.name}")


def load_json(path: pathlib.Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"release JSON is invalid: {path.name}") from error
    if not isinstance(value, dict):
        raise RuntimeError(f"release JSON root is not an object: {path.name}")
    return value


def verify_target_evidence(
    output: pathlib.Path, target: str, version: str, revision: str, signing_mode: str
) -> None:
    policy_path = output / f"{target}-signing-policy.json"
    policy = load_json(policy_path)
    if (
        policy.get("schemaVersion") != 1
        or policy.get("target") != target
        or policy.get("sourceRevision") != revision
        or policy.get("mode") != signing_mode
        or policy.get("externalPublisherIdentityVerified") != (signing_mode == "signed")
    ):
        raise RuntimeError(f"release signing policy disagrees: {policy_path.name}")

    audit = load_json(output / f"{target}-platform-audit.json")
    acceptance = load_json(output / f"{target}-platform-acceptance.json")
    smoke = load_json(output / f"{target}-installed-smoke.json")
    if audit.get("target") != target or audit.get("passed") is not True:
        raise RuntimeError(f"platform audit did not pass: {target}")
    if acceptance.get("target") != target or acceptance.get("conclusion") != "passed":
        raise RuntimeError(f"platform acceptance did not pass: {target}")
    expected_platform, expected_architecture = TARGET_HOSTS[target]
    if (
        smoke.get("platform") != expected_platform
        or smoke.get("architecture") != expected_architecture
        or smoke.get("passed") is not True
    ):
        raise RuntimeError(f"installed smoke did not pass: {target}")

    release_sets = []
    for path in output.glob("*.json"):
        value = load_json(path)
        if (
            value.get("target") == target
            and value.get("version") == version
            and value.get("source_revision") == revision
            and isinstance(value.get("artifacts"), list)
        ):
            release_sets.append(path)
    if len(release_sets) != 1:
        raise RuntimeError(f"target must have exactly one matching release-set: {target}")
    release_set = load_json(release_sets[0])
    expected_artifacts = {
        "core": TARGET_CORES[target],
        "ocr-plugin": f"official.ocr.ppocrv6-{target}.imp",
        "media-plugin": f"official.media.whisper-{target}.imp",
    }
    artifacts = release_set.get("artifacts")
    if not isinstance(artifacts, list) or len(artifacts) != len(expected_artifacts):
        raise RuntimeError(f"release-set artifact inventory is incomplete: {target}")
    actual_artifacts: dict[str, str] = {}
    for item in artifacts:
        if not isinstance(item, dict):
            raise RuntimeError(f"release-set artifact entry is invalid: {target}")
        kind = item.get("artifact")
        name = item.get("file_name")
        if kind not in expected_artifacts or name != expected_artifacts[kind]:
            raise RuntimeError(f"release-set artifact identity disagrees: {target}")
        artifact = output / name
        metadata = {
            "sbom_sha256": output / f"{name}.spdx.json",
            "sources_sha256": output / f"{name}.sources.json",
            "notices_sha256": output / f"{name}.THIRD_PARTY_NOTICES.md",
        }
        if not artifact.is_file() or any(not path.is_file() for path in metadata.values()):
            raise RuntimeError(f"release-set artifact evidence is missing: {name}")
        if (
            item.get("bytes") != artifact.stat().st_size
            or item.get("sha256") != digest(artifact)
            or not isinstance(item.get("components"), list)
            or not item["components"]
            or any(item.get(field) != digest(path) for field, path in metadata.items())
        ):
            raise RuntimeError(f"release-set artifact evidence disagrees: {name}")
        actual_artifacts[kind] = name
    if actual_artifacts != expected_artifacts:
        raise RuntimeError(f"release-set artifact inventory is not exact: {target}")
    ordered = list(expected_artifacts.values())
    if (
        release_set.get("profiles")
        != {"core": ordered[:1], "complete-offline": ordered}
        or release_set.get("complete_offline_minus_core", {}).get("artifacts") != ordered[1:]
    ):
        raise RuntimeError(f"release-set profiles disagree: {target}")


def expected_payload(signing_mode: str) -> set[str]:
    names = {"into-markdown-skill.zip", "into-markdown-skill.zip.sha256"}
    for target, core in TARGET_CORES.items():
        artifacts = [core, *(f"{plugin_id}-{target}.imp" for plugin_id in PLUGIN_IDS)]
        for artifact in artifacts:
            names.update({artifact, f"{artifact}.sha256"})
            names.update(f"{artifact}{suffix}" for suffix in METADATA_SUFFIXES)
        base = f"into-markdown-{target}-release-set"
        names.update(
            {
                f"{base}.json",
                f"{base}.spdx.json",
                f"{target}-signing-policy.json",
                *(f"{target}-{report}" for report in GENERIC_REPORTS),
            }
        )
        if signing_mode == "signed" and target.endswith("linux-gnu"):
            names.update(f"{artifact}.asc" for artifact in artifacts)
    return names


def verify_exact_payload(output: pathlib.Path, signing_mode: str) -> None:
    expected = expected_payload(signing_mode)
    actual = {path.name for path in output.iterdir() if path.is_file()}
    if actual != expected:
        missing = sorted(expected - actual)
        unexpected = sorted(actual - expected)
        raise RuntimeError(
            f"release payload is not exact: missing={missing}, unexpected={unexpected}"
        )


def finalize(
    source: pathlib.Path,
    output: pathlib.Path,
    tag: str,
    version: str,
    revision: str,
    signing_mode: str,
) -> dict:
    if tag != f"v{version}":
        raise RuntimeError("release tag must be the exact v-prefixed release version")
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise RuntimeError("release source revision must be a full lowercase Git SHA")
    if signing_mode not in {"signed", "unsigned"}:
        raise RuntimeError("release signing mode is invalid")
    try:
        validate_version(version, ROOT)
    except VersionError as error:
        raise RuntimeError(str(error)) from error

    copy_artifacts(source, output)
    verify_sidecar(output, "into-markdown-skill.zip")
    for target, core in TARGET_CORES.items():
        verify_sidecar(output, core)
        for plugin_id in PLUGIN_IDS:
            plugin = f"{plugin_id}-{target}.imp"
            verify_sidecar(output, plugin)
        verify_target_evidence(output, target, version, revision, signing_mode)
        if signing_mode == "signed" and target.endswith("linux-gnu"):
            for name in (core, *(f"{plugin_id}-{target}.imp" for plugin_id in PLUGIN_IDS)):
                if not (output / f"{name}.asc").is_file():
                    raise RuntimeError(f"signed Linux artifact lacks detached signature: {name}")

    verify_exact_payload(output, signing_mode)

    assets = [
        {
            "name": path.name,
            "bytes": path.stat().st_size,
            "sha256": digest(path),
        }
        for path in sorted(output.iterdir())
        if path.is_file()
    ]
    manifest = {
        "schemaVersion": 1,
        "tag": tag,
        "version": version,
        "sourceRevision": revision,
        "signingMode": signing_mode,
        "targets": sorted(TARGET_CORES),
        "assets": assets,
    }
    manifest_path = output / "release-manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    checksummed = [*assets, {"name": manifest_path.name, "sha256": digest(manifest_path)}]
    (output / "SHA256SUMS").write_text(
        "".join(f"{item['sha256']}  {item['name']}\n" for item in checksummed),
        encoding="ascii",
    )
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--signing-mode", choices=("signed", "unsigned"), required=True)
    arguments = parser.parse_args()
    finalize(
        arguments.source.resolve(),
        arguments.output.resolve(),
        arguments.tag,
        arguments.version,
        arguments.source_revision,
        arguments.signing_mode,
    )


if __name__ == "__main__":
    main()
