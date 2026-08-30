#!/usr/bin/env python3
"""Build and verify the compact four-platform Into Markdown release products."""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import hashlib
import importlib.util
import json
import os
import pathlib
import shutil
import stat
import struct
import sys
import time
import zipfile


ROOT = pathlib.Path(__file__).resolve().parents[2]
PORTABLE_RELEASE_DIR = pathlib.Path(__file__).resolve().parent
if str(PORTABLE_RELEASE_DIR) not in sys.path:
    sys.path.insert(0, str(PORTABLE_RELEASE_DIR))
from core_archive import (  # noqa: E402
    CORE_ARCHIVE_MANIFEST,
    CORE_ARCHIVES,
    CORE_MATERIAL_AUTHORITY,
    CORE_MATERIAL_MEMBERS,
    PDFIUM_LICENSE_FILES,
    WINDOWS_PDFIUM_MEMBER,
    PortableReleaseError,
    contains_embedded_pdfium,
    create_core_archive,
    load_authority,
    stage_core_archive_materials,
    write_core_material_authority,
    windows_pdfium_authority,
)

PLATFORM_RELEASE = ROOT / "tools/platform-release/release.py"
MACOS_RELEASE = ROOT / "tools/macos-release/release.py"
SPEECH_NAMES = {
    target: f"official.media.whisper-{target}.imp" for target in CORE_ARCHIVES
}
GGML_RUNTIME_FILES = {
    "x86_64-pc-windows-msvc": {
        *(f"bin/ggml-cpu-{name}.dll" for name in (
            "x64", "sse42", "sandybridge", "haswell", "skylakex",
            "cannonlake", "cascadelake", "icelake", "alderlake",
        )),
        "bin/ggml.dll",
        "bin/ggml-base.dll",
        "bin/whisper.dll",
    },
    "x86_64-unknown-linux-gnu": {
        *(f"bin/libggml-cpu-{name}.so" for name in (
            "x64", "sse42", "sandybridge", "ivybridge", "piledriver",
            "haswell", "skylakex", "cannonlake", "cascadelake", "cooperlake",
            "icelake", "alderlake", "sapphirerapids", "zen4",
        )),
        "bin/libggml.so.0",
        "bin/libggml-base.so.0",
        "bin/libwhisper.so.1",
    },
}
if str(ROOT / "tools") not in sys.path:
    sys.path.insert(0, str(ROOT / "tools"))
from release_timing import record as record_timing  # noqa: E402
FORBIDDEN_PLUGIN_PREFIXES = ("source/", "relink/", "licenses/")
FORBIDDEN_PLUGIN_FILES = {
    "NOTICE",
    "THIRD_PARTY_NOTICES.md",
    "SBOM.spdx.json",
    "SOURCES.json",
}
PLUGIN_MANIFEST_FIELDS = {
    "schemaVersion",
    "id",
    "version",
    "protocol",
    "supportedTargets",
    "entrypoints",
    "runtimeManifest",
    "files",
    "signature",
}
PLUGIN_FILE_FIELDS = {"path", "bytes", "sha256", "executable"}
PLUGIN_SIGNATURE_FIELDS = {
    "signedPayloadVersion",
    "algorithm",
    "keyId",
    "publicKeyBase64",
    "publicKeySha256",
    "signedPayloadSha256",
    "signatureBase64",
}


def load_release(path: pathlib.Path, name: str):
    module_dir = str(path.parent)
    if module_dir not in sys.path:
        sys.path.insert(0, module_dir)
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise PortableReleaseError(f"cannot load release authority: {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write_json(path: pathlib.Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )


def stage_catalog(
    root: pathlib.Path,
    records: dict[str, dict],
    signer: tuple[str, str],
    plugin_base_url: str,
    target: str,
) -> None:
    speech = records["official.media.whisper"]
    write_json(
        root / "official-publisher.json",
        {
            "schemaVersion": 2,
            "signingKeyId": signer[0],
            "signingKeySha256": signer[1],
            "packages": {
                "official.media.whisper": {
                    "sha256": speech["sha256"],
                    "url": f"{plugin_base_url.rstrip('/')}/{SPEECH_NAMES[target]}",
                }
            },
        },
    )


def build_and_acquire(
    release,
    target: str,
    config: dict,
    build_root: pathlib.Path,
    cache: pathlib.Path,
    timings: pathlib.Path | None = None,
) -> pathlib.Path:
    """Compile release tools while independently acquiring hash-pinned inputs."""
    downloads = release.downloads_for(config)
    with concurrent.futures.ThreadPoolExecutor(
        max_workers=2, thread_name_prefix="into-md-release"
    ) as executor:
        def timed_build():
            started = time.monotonic_ns()
            try:
                return release.build(target, build_root)
            finally:
                if timings is not None:
                    record_timing(
                        timings,
                        target,
                        "helper-provider-build",
                        (time.monotonic_ns() - started) // 1_000_000,
                    )

        build_future = executor.submit(timed_build)
        acquire_future = executor.submit(release.acquire, cache, downloads)
        futures = (build_future, acquire_future)
        try:
            for future in concurrent.futures.as_completed(futures):
                future.result()
        except BaseException:
            for future in futures:
                future.cancel()
            raise
    return build_future.result()


def build_platform(arguments: argparse.Namespace) -> None:
    release = load_release(PLATFORM_RELEASE, "into_markdown_platform_release")
    target = arguments.target
    config = release.authority()["targets"][target]
    release.check_host(target, config)
    release.validate_ffmpeg(arguments.ffmpeg_artifacts, target)
    build_root = arguments.work_root / "build"
    cache = arguments.work_root / "cache"
    release_bin = build_and_acquire(
        release, target, config, build_root, cache, arguments.timings
    )
    evidence = arguments.output / "evidence" / target
    ocr = arguments.work_root / "embedded-ocr"
    packages = arguments.work_root / "plugins"
    records, signer = release.package_plugins(
        packages,
        cache,
        release_bin,
        arguments.ffmpeg_artifacts,
        arguments.plugin_signing_key,
        target,
        config,
        arguments.windows_signing_thumbprint,
        evidence,
        ocr,
    )
    stage_catalog(ocr, records, signer, arguments.plugin_base_url, target)
    pdfium = arguments.work_root / "embedded-pdfium"
    release.extract_member(
        cache / "pdfium",
        config["pdfium"]["member"],
        pdfium / config["pdfium"]["destination"],
    )
    started = time.monotonic_ns()
    try:
        core = release.build_embedded_core(target, build_root, pdfium, ocr)
    finally:
        if arguments.timings is not None:
            record_timing(
                arguments.timings,
                target,
                "final-core-link",
                (time.monotonic_ns() - started) // 1_000_000,
            )
    release.authenticode_files([core], arguments.windows_signing_thumbprint)
    projection = release_bin / release.executable_name("release-projection", target)
    core_evidence = evidence / "core"
    core_evidence.mkdir(parents=True, exist_ok=True)
    release.write_release_inputs(core_evidence, projection, target)
    release.write_core_license_materials(core_evidence, cache, target, config)
    materials = stage_core_archive_materials(
        cache / "pdfium",
        core_evidence,
        arguments.work_root / "portable-core-materials",
    )
    write_core_material_authority(
        core_evidence / CORE_MATERIAL_AUTHORITY, materials, target
    )
    archive_name, member = CORE_ARCHIVES[target]
    archive = arguments.output / "release" / archive_name
    runtime = None
    if target == "x86_64-pc-windows-msvc":
        runtime = (pdfium / config["pdfium"]["destination"], WINDOWS_PDFIUM_MEMBER)
    create_core_archive(core, archive, member, runtime, materials, target)
    speech = arguments.output / "release" / SPEECH_NAMES[target]
    speech.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(packages / "official.media.whisper.imp", speech)
    write_target_manifest(arguments.output, target, archive, speech, release.sha256)
    verify_target(arguments.output, target)


def build_macos(arguments: argparse.Namespace) -> None:
    release = load_release(MACOS_RELEASE, "into_markdown_macos_release")
    release.check_host()
    release.validate_ffmpeg_artifacts(arguments.ffmpeg_artifacts)
    build_root = arguments.work_root / "build"
    started = time.monotonic_ns()
    try:
        release.build(build_root)
    finally:
        if arguments.timings is not None:
            record_timing(
                arguments.timings,
                arguments.target,
                "helper-provider-build",
                (time.monotonic_ns() - started) // 1_000_000,
            )
    cache = arguments.work_root / "cache"
    release.acquire(
        cache,
        {
            "pdfium",
            "onnxruntime",
            "ocr-detector",
            "ocr-recognizer",
            "ocr-dictionary",
            "ffmpeg-source",
            "whisper-small",
            "silero-vad",
            "3dspeaker",
        },
    )
    target = arguments.target
    evidence = arguments.output / "evidence" / target
    ocr = arguments.work_root / "embedded-ocr"
    packages = arguments.work_root / "plugins"
    records, signer = release.package_official_plugins(
        packages,
        cache,
        build_root / "release",
        arguments.ffmpeg_artifacts,
        arguments.plugin_signing_key,
        arguments.codesign_identity,
        evidence,
        ocr,
    )
    stage_catalog(ocr, records, signer, arguments.plugin_base_url, target)
    pdfium = arguments.work_root / "embedded-pdfium"
    release.extract_tar(
        cache / "pdfium",
        pdfium / "lib/pdfium",
        {"lib/libpdfium.dylib": "libpdfium.dylib"},
    )
    started = time.monotonic_ns()
    try:
        core = release.build_embedded_core(build_root, pdfium, ocr)
    finally:
        if arguments.timings is not None:
            record_timing(
                arguments.timings,
                target,
                "final-core-link",
                (time.monotonic_ns() - started) // 1_000_000,
            )
    release.codesign_files([core], arguments.codesign_identity)
    core_evidence = evidence / "core"
    core_evidence.mkdir(parents=True, exist_ok=True)
    projection = build_root / "release/release-projection"
    release.write_release_inputs(core_evidence, projection)
    release.write_core_license_materials(core_evidence, cache)
    materials = stage_core_archive_materials(
        cache / "pdfium",
        core_evidence,
        arguments.work_root / "portable-core-materials",
    )
    write_core_material_authority(
        core_evidence / CORE_MATERIAL_AUTHORITY, materials, target
    )
    archive_name, member = CORE_ARCHIVES[target]
    archive = arguments.output / "release" / archive_name
    create_core_archive(core, archive, member, materials=materials, target=target)
    speech = arguments.output / "release" / SPEECH_NAMES[target]
    speech.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(packages / "official.media.whisper.imp", speech)
    write_target_manifest(arguments.output, target, archive, speech, release.sha256)
    verify_target(arguments.output, target)


def write_target_manifest(
    output: pathlib.Path,
    target: str,
    core: pathlib.Path,
    speech: pathlib.Path,
    digest,
) -> None:
    write_json(
        output / "evidence" / target / "release-files.json",
        {
            "schemaVersion": 1,
            "target": target,
            "files": [
                {"name": core.name, "bytes": core.stat().st_size, "sha256": digest(core)},
                {
                    "name": speech.name,
                    "bytes": speech.stat().st_size,
                    "sha256": digest(speech),
                },
            ],
        },
    )


def binary_architecture(data: bytes, target: str) -> bool:
    if target == "x86_64-pc-windows-msvc":
        if len(data) < 64 or data[:2] != b"MZ":
            return False
        offset = struct.unpack_from("<I", data, 0x3C)[0]
        return (
            offset + 6 <= len(data)
            and data[offset : offset + 4] == b"PE\0\0"
            and struct.unpack_from("<H", data, offset + 4)[0] == 0x8664
        )
    if target.endswith("linux-gnu"):
        expected = 62 if target.startswith("x86_64") else 183
        return (
            len(data) >= 20
            and data[:6] == b"\x7fELF\x02\x01"
            and struct.unpack_from("<H", data, 18)[0] == expected
        )
    if len(data) < 8:
        return False
    if data[:4] == b"\xcf\xfa\xed\xfe":
        machine = struct.unpack_from("<I", data, 4)[0]
    elif data[:4] == b"\xfe\xed\xfa\xcf":
        machine = struct.unpack_from(">I", data, 4)[0]
    else:
        return False
    return machine == 0x0100000C


def _package_path(name: object) -> str:
    if (
        not isinstance(name, str)
        or not name
        or len(name) > 1024
        or not name.isascii()
        or "\\" in name
        or "\0" in name
    ):
        raise PortableReleaseError("speech package contains an unsafe path")
    path = pathlib.PurePosixPath(name)
    segments = name.split("/")
    if (
        path.is_absolute()
        or path.as_posix() != name
        or any(
            not part
            or len(part) > 240
            or part.endswith((".", " "))
            or any(
                not (character.isascii() and (character.isalnum() or character in "._-@"))
                for character in part
            )
            or part.split(".", 1)[0].upper()
            in {
                "CON",
                "PRN",
                "AUX",
                "NUL",
                "CLOCK$",
                *(f"COM{index}" for index in range(1, 10)),
                *(f"LPT{index}" for index in range(1, 10)),
            }
            for part in segments
        )
    ):
        raise PortableReleaseError("speech package contains an unsafe path")
    return name


def _hex_sha256(value: object) -> bool:
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


def _canonical_signed_payload(manifest: dict) -> bytes:
    signature = manifest["signature"]
    payload = {
        "signatureDomain": "into-markdown/plugin-package/v1",
        "signedPayloadVersion": signature["signedPayloadVersion"],
        "algorithm": signature["algorithm"],
        "keyId": signature["keyId"],
        "publicKeySha256": signature["publicKeySha256"],
        "schemaVersion": manifest["schemaVersion"],
        "id": manifest["id"],
        "version": manifest["version"],
        "protocol": manifest["protocol"],
        "supportedTargets": manifest["supportedTargets"],
        "entrypoints": {
            key: manifest["entrypoints"][key] for key in sorted(manifest["entrypoints"])
        },
        "runtimeManifest": manifest["runtimeManifest"],
        "files": manifest["files"],
    }
    return json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def _validate_speech_manifest(
    package: zipfile.ZipFile,
    infos: list[zipfile.ZipInfo],
    target: str,
) -> None:
    try:
        manifest = json.loads(package.read("plugin.json"))
        provider = json.loads(package.read("provider.json"))
    except (KeyError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise PortableReleaseError("speech package manifest is absent or invalid") from error
    if not isinstance(manifest, dict) or set(manifest) != PLUGIN_MANIFEST_FIELDS:
        raise PortableReleaseError("speech package manifest fields are invalid")
    if (
        manifest["schemaVersion"] != 1
        or manifest["id"] != "official.media.whisper"
        or not isinstance(manifest["version"], str)
        or not manifest["version"]
        or manifest["protocol"] != "process-v1"
        or manifest["supportedTargets"] != [target]
        or not isinstance(manifest["entrypoints"], dict)
        or set(manifest["entrypoints"]) != {target}
        or manifest["runtimeManifest"] is not None
        or not isinstance(manifest["files"], list)
        or not manifest["files"]
    ):
        raise PortableReleaseError("speech package signed identity is invalid")

    declared: dict[str, dict] = {}
    for record in manifest["files"]:
        if not isinstance(record, dict) or set(record) != PLUGIN_FILE_FIELDS:
            raise PortableReleaseError("speech package signed file inventory is invalid")
        path = _package_path(record["path"])
        if (
            path in declared
            or not isinstance(record["bytes"], int)
            or isinstance(record["bytes"], bool)
            or record["bytes"] < 0
            or not _hex_sha256(record["sha256"])
            or not isinstance(record["executable"], bool)
        ):
            raise PortableReleaseError("speech package signed file inventory is invalid")
        declared[path] = record
    if list(declared) != sorted(declared):
        raise PortableReleaseError("speech package signed file inventory is not deterministic")

    actual_names = {info.filename for info in infos}
    if set(declared) | {"plugin.json"} != actual_names:
        raise PortableReleaseError("speech package differs from its signed file inventory")
    observed: dict[str, dict] = {}
    for info in infos:
        if info.filename == "plugin.json":
            continue
        contents = package.read(info)
        observed[info.filename] = {
            "bytes": len(contents),
            "sha256": hashlib.sha256(contents).hexdigest(),
        }
    for path, record in declared.items():
        if observed[path] != {"bytes": record["bytes"], "sha256": record["sha256"]}:
            raise PortableReleaseError(f"speech package member differs from signed manifest: {path}")

    signature = manifest["signature"]
    if not isinstance(signature, dict) or set(signature) != PLUGIN_SIGNATURE_FIELDS:
        raise PortableReleaseError("speech package signature fields are invalid")
    try:
        public_key = base64.b64decode(signature["publicKeyBase64"], validate=True)
        signature_bytes = base64.b64decode(signature["signatureBase64"], validate=True)
    except (TypeError, ValueError) as error:
        raise PortableReleaseError("speech package signature encoding is invalid") from error
    payload = _canonical_signed_payload(manifest)
    if (
        signature["signedPayloadVersion"] != 1
        or signature["algorithm"] != "ed25519"
        or not isinstance(signature["keyId"], str)
        or not signature["keyId"]
        or len(public_key) != 32
        or len(signature_bytes) != 64
        or not _hex_sha256(signature["publicKeySha256"])
        or not _hex_sha256(signature["signedPayloadSha256"])
        or hashlib.sha256(public_key).hexdigest() != signature["publicKeySha256"]
        or hashlib.sha256(payload).hexdigest() != signature["signedPayloadSha256"]
    ):
        raise PortableReleaseError("speech package signature authority is invalid")

    if not isinstance(provider, dict):
        raise PortableReleaseError("speech provider manifest is invalid")
    targets = provider.get("targets")
    matching = (
        [item for item in targets if isinstance(item, dict) and item.get("triple") == target]
        if isinstance(targets, list)
        else []
    )
    if (
        provider.get("id") != manifest["id"]
        or provider.get("version") != manifest["version"]
        or len(matching) != 1
        or matching[0].get("entrypoint") != manifest["entrypoints"][target]
        or not isinstance(matching[0].get("files"), list)
    ):
        raise PortableReleaseError("speech provider manifest identity is invalid")
    runtime = matching[0]["files"]
    if any(not isinstance(item, dict) or set(item) != PLUGIN_FILE_FIELDS for item in runtime):
        raise PortableReleaseError("speech provider runtime inventory is invalid")
    runtime_by_path = {item.get("path"): item for item in runtime}
    runtime_names = actual_names - {"plugin.json", "provider.json"}
    if (
        len(runtime_by_path) != len(runtime)
        or set(runtime_by_path) != runtime_names
        or any(
            {
                "bytes": runtime_by_path[path].get("bytes"),
                "sha256": runtime_by_path[path].get("sha256"),
            }
            != {"bytes": declared[path]["bytes"], "sha256": declared[path]["sha256"]}
            for path in runtime_names
        )
    ):
        raise PortableReleaseError("speech provider runtime inventory differs from signed package")
    expected_ggml = GGML_RUNTIME_FILES.get(target, set())
    observed_ggml = {
        path
        for path in runtime_names
        if pathlib.PurePosixPath(path).name.startswith(("ggml", "libggml", "whisper", "libwhisper"))
        and pathlib.PurePosixPath(path).suffix in {".dll", ".so", ".0", ".1"}
    }
    if observed_ggml != expected_ggml:
        raise PortableReleaseError("speech package GGML runtime inventory is invalid")
    for path in expected_ggml:
        if not binary_architecture(package.read(path), target):
            raise PortableReleaseError(f"speech package GGML architecture is invalid: {path}")
    entrypoint = manifest["entrypoints"][target]
    if entrypoint not in runtime_by_path or runtime_by_path[entrypoint].get("executable") is not True:
        raise PortableReleaseError("speech provider entrypoint is absent or non-executable")


def _validate_speech_package(package: zipfile.ZipFile, target: str) -> None:
    infos = package.infolist()
    names = [info.filename for info in infos]
    if len(names) != len(set(names)):
        raise PortableReleaseError("speech package contains duplicate entries")
    if package.comment:
        raise PortableReleaseError("speech package metadata is not deterministic")
    for info in infos:
        name = _package_path(info.filename)
        path = pathlib.PurePosixPath(name)
        if path.name in FORBIDDEN_PLUGIN_FILES or any(
            part in {prefix.rstrip("/") for prefix in FORBIDDEN_PLUGIN_PREFIXES}
            for part in path.parts
        ):
            raise PortableReleaseError(f"speech package contains audit-only entry: {name}")
        mode = (info.external_attr >> 16) & 0o177777
        if (
            info.is_dir()
            or mode != stat.S_IFREG | 0o644
            or info.create_system != 3
            or info.date_time != (1980, 1, 1, 0, 0, 0)
            or info.flag_bits & 0x1
            or info.comment
            or info.extra
            or info.compress_type != zipfile.ZIP_STORED
        ):
            raise PortableReleaseError(f"speech package entry metadata is invalid: {name}")
    _validate_speech_manifest(package, infos, target)


def verify_core_archive(
    archive_path: pathlib.Path,
    target: str,
    material_authority: dict | None = None,
) -> None:
    from core_archive import verify_core_archive as verify

    authority = (
        windows_pdfium_authority()
        if target == "x86_64-pc-windows-msvc"
        else None
    )
    verify(
        archive_path,
        target,
        binary_architecture,
        authority,
        material_authority,
    )


def verify_target(output: pathlib.Path, target: str) -> None:
    archive_name, _ = CORE_ARCHIVES[target]
    authority_path = output / "evidence" / target / "core" / CORE_MATERIAL_AUTHORITY
    try:
        material_authority = load_authority(
            authority_path, target, CORE_MATERIAL_MEMBERS
        )
    except RuntimeError as error:
        raise PortableReleaseError(str(error)) from error
    verify_core_archive(
        output / "release" / archive_name, target, material_authority
    )
    speech = output / "release" / SPEECH_NAMES[target]
    with zipfile.ZipFile(speech) as package:
        _validate_speech_package(package, target)


def parse() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=["build", "verify"])
    parser.add_argument("--target", required=True, choices=sorted(CORE_ARCHIVES))
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--work-root", type=pathlib.Path)
    parser.add_argument("--ffmpeg-artifacts", type=pathlib.Path)
    parser.add_argument("--plugin-signing-key", type=pathlib.Path)
    parser.add_argument("--plugin-base-url")
    parser.add_argument("--windows-signing-thumbprint")
    parser.add_argument("--codesign-identity")
    parser.add_argument("--timings", type=pathlib.Path)
    arguments = parser.parse_args()
    if arguments.command == "build" and any(
        value is None
        for value in (
            arguments.work_root,
            arguments.ffmpeg_artifacts,
            arguments.plugin_signing_key,
            arguments.plugin_base_url,
        )
    ):
        parser.error("build requires work, FFmpeg, plugin key, and plugin base URL inputs")
    return arguments


def main() -> None:
    arguments = parse()
    arguments.output = arguments.output.resolve()
    if arguments.command == "verify":
        verify_target(arguments.output, arguments.target)
    elif arguments.target == "aarch64-apple-darwin":
        build_macos(arguments)
    else:
        build_platform(arguments)


if __name__ == "__main__":
    try:
        main()
    except (PortableReleaseError, OSError, ValueError, zipfile.BadZipFile) as error:
        print(f"portable-release: {error}", file=sys.stderr)
        raise SystemExit(1)
