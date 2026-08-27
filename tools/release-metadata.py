#!/usr/bin/env python3
"""Generate final artifact sidecars and a target release-set from verified bytes."""

from __future__ import annotations

import argparse
import functools
import hashlib
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
import zipfile


ARTIFACTS = {
    "official.ocr.ppocrv6.imp": "ocr-plugin",
    "official.media.whisper.imp": "media-plugin",
}
OCR_MODEL_COMPONENTS = {
    "ppocrv6-tiny-detector-onnx-model",
    "ppocrv6-tiny-recognizer-character-table",
    "ppocrv6-tiny-recognizer-onnx-model",
}
PLUGIN_RUNTIME_COMPONENTS = {
    "ocr-plugin": (
        "onnxruntime-cpu",
        "ppocrv6-tiny-detector-onnx-model",
        "ppocrv6-tiny-recognizer-character-table",
        "ppocrv6-tiny-recognizer-onnx-model",
    ),
    "media-plugin": (
        "ffmpeg",
        "onnxruntime-cpu",
        "whisper-small",
        "silero-vad-half-onnx-model",
        "3dspeaker-eres2net-base-onnx-model",
    ),
}
ROOT = pathlib.Path(__file__).resolve().parents[1]


def plugin_artifact_kind(filename: str, target: str) -> str:
    for local_name, kind in ARTIFACTS.items():
        stem = pathlib.PurePosixPath(local_name).stem
        if filename in {local_name, f"{stem}-{target}.imp"}:
            return kind
    raise RuntimeError(f"plugin artifact filename is not authorized: {filename}")


def sha256_bytes(contents: bytes) -> str:
    return hashlib.sha256(contents).hexdigest()


def sha1_bytes(contents: bytes) -> str:
    return hashlib.sha1(contents, usedforsecurity=False).hexdigest()


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def sha1(path: pathlib.Path) -> str:
    digest = hashlib.sha1(usedforsecurity=False)
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def run_projection(tool: pathlib.Path, operation: str, value: dict) -> dict:
    with tempfile.TemporaryDirectory(prefix="into-md-release-metadata-") as name:
        request = pathlib.Path(name) / "request.json"
        request.write_text(json.dumps(value, sort_keys=True) + "\n", encoding="utf-8")
        process = subprocess.run(
            [str(tool), operation, str(request)],
            check=False,
            capture_output=True,
            text=True,
        )
    if process.returncode != 0:
        raise RuntimeError(process.stderr.strip() or f"release-projection {operation} failed")
    return json.loads(process.stdout)


def build_tool_executions(target: str) -> list[dict]:
    commands = [
        ("rust-toolchain", "rustc", ["--version"]),
        ("bazel", "bazel", ["--version"]),
        ("node", "node", ["--version"]),
        ("pnpm", "pnpm", ["--version"]),
        ("python", pathlib.Path(sys.executable).name, ["--version"]),
    ]
    if target == "aarch64-apple-darwin":
        commands.extend(
            [
                ("apple-xcode-toolchain", "clang", ["--version"]),
                ("apple-xcode-toolchain", "ld", ["-v"]),
                ("apple-xcode-toolchain", "codesign", ["--help"]),
                ("apple-xcode-toolchain", "hdiutil", ["help"]),
                ("apple-xcode-toolchain", "xcrun", ["--version"]),
            ]
        )
    elif target.endswith("linux-gnu"):
        commands.extend(
            [
                ("ubuntu-build-toolchain", "cc", ["--version"]),
                ("ubuntu-build-toolchain", "c++", ["--version"]),
                ("ubuntu-build-toolchain", "ld", ["--version"]),
                ("ubuntu-build-toolchain", "tar", ["--version"]),
                ("ubuntu-build-toolchain", "gpg", ["--version"]),
            ]
        )
    elif target == "x86_64-pc-windows-msvc":
        commands.extend(
            [
                ("windows-msvc-toolchain", "cl", ["/?"]),
                ("windows-msvc-toolchain", "link", ["/?"]),
                ("windows-msvc-toolchain", "signtool", ["/?"]),
            ]
        )
    else:
        raise RuntimeError(f"unsupported release target: {target}")

    result = []
    for authority_id, command, version_args in commands:
        located = pathlib.Path(sys.executable) if authority_id == "python" else None
        if located is None:
            found = shutil.which(command)
            if found is None:
                raise RuntimeError(f"build tool executable is unavailable: {command}")
            located = pathlib.Path(found)
        executable = located.resolve(strict=True)
        process = subprocess.run(
            [str(located), *version_args],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
        output = "\n".join((process.stdout, process.stderr))
        version = next((line.strip() for line in output.splitlines() if line.strip()), "")
        if not version:
            raise RuntimeError(f"build tool returned no version identity: {command}")
        result.append(
            {
                "authority_id": authority_id,
                "name": command,
                "version": version,
                "bytes": executable.stat().st_size,
                "sha256": sha256(executable),
            }
        )
    return result


def source_components(contents: bytes) -> list[str]:
    value = json.loads(contents)
    return [
        component["id"]
        for component in value["components"]
        if component["distributed"]
    ]


def scoped_plugin_components(
    source_component_ids: list[str], artifact: str
) -> list[str]:
    runtime_components = PLUGIN_RUNTIME_COMPONENTS[artifact]
    missing_runtime = set(runtime_components) - set(source_component_ids)
    if missing_runtime:
        raise RuntimeError(
            f"plugin source inventory omits runtime components: "
            f"{sorted(missing_runtime)}"
        )
    cargo_components = [
        item for item in source_component_ids if item.startswith("cargo:")
    ]
    return [*cargo_components, *runtime_components]


def core_projection(
    target: str,
    version: str,
    source_revision: str,
    artifact: pathlib.Path,
    root: pathlib.Path,
) -> dict:
    manifest_path = root / "archive-manifest.json"
    manifest_bytes = manifest_path.read_bytes()
    manifest = json.loads(manifest_bytes)
    if (
        manifest["target"] != target
        or manifest["version"] != version
        or manifest["source_revision"] != source_revision
    ):
        raise RuntimeError("Core archive manifest identity differs from metadata identity")
    expected = {item["path"]: item for item in manifest["files"]}
    actual = {}
    for path in sorted(item for item in root.rglob("*") if item.is_file()):
        relative = path.relative_to(root).as_posix()
        actual[relative] = {
            "path": relative,
            "bytes": path.stat().st_size,
            "sha1": sha1(path),
            "sha256": sha256(path),
        }
    manifest_entry = actual.pop("archive-manifest.json", None)
    if manifest_entry is None or set(actual) != set(expected):
        raise RuntimeError("Core staging tree differs from archive-manifest.json")
    files = []
    for path, observed in actual.items():
        declared = expected[path]
        if observed["bytes"] != declared["bytes"] or observed["sha256"] != declared["sha256"]:
            raise RuntimeError(f"Core member differs from archive manifest: {path}")
        declared["sha1"] = observed["sha1"]
        files.append(declared)
    manifest_entry["kind"] = "generated"
    files.append(manifest_entry)
    return {
        "schema_version": 1,
        "target": target,
        "artifact": "core",
        "version": version,
        "source_revision": source_revision,
        "file_name": artifact.name,
        "bytes": artifact.stat().st_size,
        "sha256": sha256(artifact),
        "components": manifest["components"],
        "files": sorted(files, key=lambda item: item["path"]),
    }


@functools.lru_cache(maxsize=1)
def ocr_model_digest_owners() -> dict[str, str]:
    authority = json.loads(
        (ROOT / "third_party/licenses/downloads.json").read_text(encoding="utf-8")
    )
    if authority.get("schema_version") != 1:
        raise RuntimeError("model download authority schema is invalid")
    owners: dict[str, str] = {}
    selected: set[str] = set()
    for item in authority.get("model_runtime_files", []):
        owner = item.get("artifact_id")
        if owner not in OCR_MODEL_COMPONENTS:
            continue
        digest = item.get("sha256", "")
        if (
            len(digest) != 64
            or any(character not in "0123456789abcdef" for character in digest)
            or digest in owners
        ):
            raise RuntimeError("OCR model download authority is ambiguous")
        owners[digest] = owner
        selected.add(owner)
    if selected != OCR_MODEL_COMPONENTS:
        raise RuntimeError("OCR model download authority is incomplete")
    return owners


def plugin_owner(path: str, digest: str | None = None) -> str | None:
    path_owner = None
    if "onnxruntime" in path and path.rsplit("/", 1)[-1].startswith(("libonnxruntime", "onnxruntime")):
        path_owner = "onnxruntime-cpu"
    elif path.endswith("pp-ocrv6-tiny-detector-onnx/inference.onnx"):
        path_owner = "ppocrv6-tiny-detector-onnx-model"
    elif path.endswith("pp-ocrv6-tiny-recognizer-onnx/inference.onnx"):
        path_owner = "ppocrv6-tiny-recognizer-onnx-model"
    elif path.endswith("pp-ocrv6-tiny-recognizer-onnx/ppocrv6_tiny_dict.txt"):
        path_owner = "ppocrv6-tiny-recognizer-character-table"
    elif path.endswith("ggml-small.bin"):
        path_owner = "whisper-small"
    elif path.endswith("silero_vad_half.onnx"):
        path_owner = "silero-vad-half-onnx-model"
    elif path.endswith("3dspeaker_eres2net_base.onnx"):
        path_owner = "3dspeaker-eres2net-base-onnx-model"
    elif path.startswith(("ffmpeg/", "source/ffmpeg-", "relink/ffmpeg-")):
        path_owner = "ffmpeg"

    digest_owner = ocr_model_digest_owners().get(digest) if digest is not None else None
    if path_owner in OCR_MODEL_COMPONENTS and digest_owner != path_owner:
        raise RuntimeError(f"OCR model member differs from download authority: {path}")
    if digest_owner is not None and path_owner not in {None, digest_owner}:
        raise RuntimeError(f"OCR model digest has conflicting path ownership: {path}")
    return path_owner or digest_owner


def plugin_projection(
    target: str, version: str, source_revision: str, artifact: pathlib.Path
) -> dict:
    with zipfile.ZipFile(artifact) as package:
        infos = package.infolist()
        if len({info.filename for info in infos}) != len(infos):
            raise RuntimeError(f"plugin contains duplicate members: {artifact.name}")
        if any(info.is_dir() for info in infos):
            raise RuntimeError(f"plugin contains non-file members: {artifact.name}")
        manifest = json.loads(package.read("plugin.json"))
        declared = {item["path"]: item for item in manifest["files"]}
        actual_names = {info.filename for info in infos}
        if set(declared) | {"plugin.json"} != actual_names:
            raise RuntimeError(f"plugin ZIP differs from signed manifest: {artifact.name}")
        if manifest["supportedTargets"] != [target] or set(manifest["entrypoints"]) != {target}:
            raise RuntimeError(f"plugin signed manifest target differs: {artifact.name}")
        signature = manifest["signature"]
        for field in ["publicKeySha256", "signedPayloadSha256"]:
            value = signature.get(field, "")
            if len(value) != 64 or any(character not in "0123456789abcdef" for character in value):
                raise RuntimeError(f"plugin signature identity is incomplete: {artifact.name}")

        observed = {}
        for info in infos:
            if info.filename == "plugin.json":
                continue
            contents = package.read(info)
            observed[info.filename] = {
                "bytes": len(contents),
                "sha256": sha256_bytes(contents),
            }
        for path, authority in declared.items():
            if observed[path] != {
                "bytes": authority["bytes"],
                "sha256": authority["sha256"],
            }:
                raise RuntimeError(f"plugin member differs from signed manifest: {path}")

        provider = json.loads(package.read("provider.json"))
        targets = [item for item in provider["targets"] if item["triple"] == target]
        if len(targets) != 1:
            raise RuntimeError(f"provider runtime target differs: {artifact.name}")
        runtime = {item["path"]: item for item in targets[0]["files"]}
        runtime_names = actual_names - {"plugin.json", "provider.json"}
        if set(runtime) != runtime_names:
            raise RuntimeError(f"provider runtime inventory differs: {artifact.name}")
        for path, authority in runtime.items():
            if observed[path] != {
                "bytes": authority["bytes"],
                "sha256": authority["sha256"],
            }:
                raise RuntimeError(f"provider runtime member differs: {path}")

        sources = package.read("SOURCES.json")
        source_manifest = json.loads(sources)
        expected_artifact = plugin_artifact_kind(artifact.name, target)
        expected_identity = {
            "ocr-plugin": "official.ocr.ppocrv6",
            "media-plugin": "official.media.whisper",
        }[expected_artifact]
        if (
            source_manifest["target"] != target
            or source_manifest["version"] != version
            or source_manifest["source_revision"] != source_revision
            or source_manifest["artifact"] != expected_identity
        ):
            raise RuntimeError(f"plugin source identity differs: {artifact.name}")
        source_component_ids = source_components(sources)
        components = scoped_plugin_components(source_component_ids, expected_artifact)
        cargo_components = [item for item in components if item.startswith("cargo:")]
        files = []
        for info in infos:
            contents = package.read(info)
            path = info.filename
            digest = sha256_bytes(contents)
            owner = plugin_owner(path, digest)
            entry = {
                "path": path,
                "bytes": len(contents),
                "sha1": sha1_bytes(contents),
                "sha256": digest,
                "kind": "component" if owner else "project",
            }
            if owner:
                entry["component_id"] = owner
            if path.startswith("bin/into-md-") and "provider" in path:
                entry["embedded_components"] = cargo_components
            files.append(entry)
    return {
        "schema_version": 1,
        "target": target,
        "artifact": expected_artifact,
        "version": version,
        "source_revision": source_revision,
        "file_name": artifact.name,
        "bytes": artifact.stat().st_size,
        "sha256": sha256(artifact),
        "components": components,
        "files": files,
    }


def write_generated(output: pathlib.Path, metadata: dict) -> None:
    output.mkdir(parents=True, exist_ok=True)
    for key in ["sbom", "sources", "third_party_notices", "release_set"]:
        item = metadata.get(key)
        if item is not None:
            path = output / item["path"]
            # Projection digests bind UTF-8 bytes. Writing text would translate
            # LF to CRLF on Windows and invalidate the digest after generation.
            path.write_bytes(item["contents"].encode("utf-8"))
            if path.stat().st_size != item["bytes"] or sha256(path) != item["sha256"]:
                raise RuntimeError(f"generated metadata digest drifted: {path.name}")


def validate_projection_ownership(projection: dict) -> None:
    selected = set(projection["components"])
    owners: set[str] = set()
    for item in projection["files"]:
        owner = item.get("component_id")
        if owner is not None:
            owners.add(owner)
        owners.update(item.get("embedded_components", []))
    missing = sorted(selected - owners)
    if not missing:
        return
    candidates = [
        {
            "path": item["path"],
            "bytes": item["bytes"],
            "sha256": item["sha256"],
            "component_id": item.get("component_id"),
        }
        for item in projection["files"]
        if any(token in item["path"].lower() for token in ("model", "onnx", "dict"))
    ][:24]
    raise RuntimeError(
        f"{projection['artifact']} {projection['file_name']} has unowned components "
        f"{missing}; bounded model candidates={json.dumps(candidates, sort_keys=True)}"
    )


def generate(
    projection_tool: pathlib.Path,
    target: str,
    version: str,
    source_revision: str,
    core_artifact: pathlib.Path,
    core_root: pathlib.Path,
    plugins: pathlib.Path,
    output: pathlib.Path,
) -> None:
    projections = [
        core_projection(target, version, source_revision, core_artifact, core_root)
    ]
    for name in ARTIFACTS:
        stem = pathlib.PurePosixPath(name).stem
        candidates = [plugins / name, plugins / f"{stem}-{target}.imp"]
        present = [artifact for artifact in candidates if artifact.is_file()]
        if len(present) != 1:
            raise RuntimeError(f"release plugin must have one authorized filename: {name}")
        artifact = present[0]
        projections.append(plugin_projection(target, version, source_revision, artifact))
    executions = build_tool_executions(target)
    for projection in projections:
        projection["build_tools"] = executions
    finalized = []
    for projection in projections:
        validate_projection_ownership(projection)
        metadata = run_projection(projection_tool, "finalize", projection)
        write_generated(output, metadata)
        finalized.append((projection, metadata))
    request = {
        "schema_version": 1,
        "target": target,
        "version": version,
        "source_revision": source_revision,
        "artifacts": [
            {
                "artifact": projection["artifact"],
                "file_name": projection["file_name"],
                "bytes": projection["bytes"],
                "sha256": projection["sha256"],
                "components": projection["components"],
                "sbom_sha256": metadata["sbom"]["sha256"],
                "sources_sha256": metadata["sources"]["sha256"],
                "notices_sha256": metadata["third_party_notices"]["sha256"],
            }
            for projection, metadata in finalized
        ],
    }
    aggregate = run_projection(projection_tool, "aggregate", request)
    write_generated(output, aggregate)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--projection-tool", required=True, type=pathlib.Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--version", default="0.0.0")
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--core-artifact", required=True, type=pathlib.Path)
    parser.add_argument("--core-root", required=True, type=pathlib.Path)
    parser.add_argument("--plugins", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    arguments = parser.parse_args()
    generate(
        arguments.projection_tool.resolve(),
        arguments.target,
        arguments.version,
        arguments.source_revision,
        arguments.core_artifact.resolve(),
        arguments.core_root.resolve(),
        arguments.plugins.resolve(),
        arguments.output.resolve(),
    )


if __name__ == "__main__":
    main()
