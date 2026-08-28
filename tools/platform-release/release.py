"""Build Linux or Windows Core plus two signed self-contained capability plugins."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import platform
import re
import shutil
import sys
import tarfile
import tempfile
import tomllib
import zipfile

from acquire import acquire
from common import (
    ROOT,
    ReleaseError,
    authority,
    regular_files,
    resolve_windows_sdk_tool,
    run,
    sha256,
    write_json,
)

sys.path.append(str(pathlib.Path(__file__).resolve().parents[1] / "macos-release"))
from rust_package import materialize as materialize_rust  # noqa: E402

sys.path.append(str(pathlib.Path(__file__).resolve().parents[1] / "skill-release"))
from skill_release import CORE_RELATIVE as AGENT_SKILL_RELATIVE  # noqa: E402
from skill_release import materialize as materialize_agent_skill  # noqa: E402

VERSION = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"][
    "package"
]["version"]
OCR_COMPONENTS = [
    "onnxruntime-cpu",
    "ppocrv6-tiny-detector-onnx-model",
    "ppocrv6-tiny-recognizer-character-table",
    "ppocrv6-tiny-recognizer-onnx-model",
]
CORE_COMPONENTS = ["pdfium"]
SPEECH_COMPONENTS = [
    "ffmpeg",
    "onnxruntime-cpu",
    "whisper-small",
    "silero-vad-half-onnx-model",
    "3dspeaker-eres2net-base-onnx-model",
]
SPEECH_TRANSCRIPTION_MEMORY_BYTES = 1536 * 1024 * 1024
FIXTURES = [
    "docx/normal.docx",
    "docx/corrupt.docx",
    "epub/normal.epub",
    "msg/normal.msg",
    "ocr/ocr-english-clear-1.png",
    "pdf/structures.pdf",
    "rtf/normal.rtf",
    "text/normal.txt",
    "xlsx/normal.xlsx",
    "xlsb/normal.xlsb",
    "pptx/normal.pptx",
    "odt/normal.odt",
    "ods/normal.ods",
    "odp/normal.odp",
]


def published_plugin_file(filename: str, target: str) -> str:
    path = pathlib.PurePosixPath(filename)
    if path.name != filename or path.suffix != ".imp" or not path.stem:
        raise ReleaseError("plugin package filename is invalid")
    if not re.fullmatch(r"[a-z0-9_]+(?:-[a-z0-9_]+)+", target):
        raise ReleaseError("plugin publication target is invalid")
    return f"{path.stem}-{target}.imp"


def check_host(target: str, config: dict) -> None:
    machine = platform.machine().lower()
    if config["os"] == "linux":
        expected = {"x86_64": {"x86_64", "amd64"}, "aarch64": {"aarch64", "arm64"}}[
            config["architecture"]
        ]
        if sys.platform != "linux" or machine not in expected:
            raise ReleaseError(f"{target} assembly requires its native Linux architecture")
    elif os.name != "nt" or machine not in {"amd64", "x86_64"}:
        raise ReleaseError("Windows release assembly requires native Windows x86_64")
    rust = run(["rustc", "--version"]).split()[1]
    if rust != authority()["rust"]:
        raise ReleaseError(f"rustc {rust} disagrees with fixed toolchain {authority()['rust']}")


def executable_name(name: str, target: str) -> str:
    return f"{name}.exe" if target == "x86_64-pc-windows-msvc" else name


def source_revision() -> str:
    return run(["git", "rev-parse", "HEAD"], cwd=ROOT).strip()


def distributed_source_ids(manifest: dict) -> list[str]:
    return [item["id"] for item in manifest["components"] if item["distributed"]]


def bundled_ocr_components(package: pathlib.Path) -> set[str]:
    try:
        with zipfile.ZipFile(package) as archive:
            sources = json.loads(archive.read("SOURCES.json"))
    except (OSError, KeyError, ValueError, zipfile.BadZipFile, json.JSONDecodeError) as error:
        raise ReleaseError(f"bundled OCR source authority is invalid: {error}") from error
    if sources.get("artifact") != "official.ocr.ppocrv6":
        raise ReleaseError("bundled OCR source authority has the wrong artifact identity")
    return set(distributed_source_ids(sources))


def authenticode_files(paths: list[pathlib.Path], thumbprint: str | None) -> None:
    if thumbprint is None:
        return
    if os.name != "nt" or len(thumbprint) != 40 or any(
        character not in "0123456789abcdefABCDEF" for character in thumbprint
    ):
        raise ReleaseError("Windows signing thumbprint is invalid on this host")
    signtool = resolve_windows_sdk_tool("signtool.exe")
    for path in paths:
        if not path.is_file() or path.is_symlink():
            raise ReleaseError(f"Authenticode input is not a regular file: {path}")
        run(
            [
                signtool,
                "sign",
                "/fd",
                "SHA256",
                "/td",
                "SHA256",
                "/tr",
                "https://timestamp.digicert.com",
                "/sha1",
                thumbprint,
                path,
            ]
        )
        run([signtool, "verify", "/pa", "/all", path])


def refresh_ffmpeg_authority(authority_path: pathlib.Path, executable: pathlib.Path) -> None:
    value = json.loads(authority_path.read_text(encoding="utf-8"))
    value["executable_bytes"] = executable.stat().st_size
    value["executable_sha256"] = sha256(executable)
    write_json(authority_path, value)


def build(target: str, output: pathlib.Path) -> pathlib.Path:
    environment = os.environ.copy()
    environment.update({"CARGO_INCREMENTAL": "0", "CARGO_TARGET_DIR": str(output)})
    rustflags = ["-C", "strip=debuginfo"]
    target_cpu = {
        "x86_64-unknown-linux-gnu": "x86-64",
        "aarch64-unknown-linux-gnu": "generic",
        "x86_64-pc-windows-msvc": "x86-64",
    }[target]
    rustflags.extend(["-C", f"target-cpu={target_cpu}"])
    if target.endswith("linux-gnu"):
        rustflags.extend(["-C", "link-arg=-Wl,-rpath,$ORIGIN/../lib/pdfium"])
    elif target == "x86_64-pc-windows-msvc":
        rustflags.extend(
            [
                "-C",
                "debuginfo=0",
                "-C",
                "link-arg=/Brepro",
                "-C",
                "link-arg=/DEBUG:NONE",
            ]
        )
    environment["RUSTFLAGS"] = " ".join(rustflags)
    # release-projection intentionally runs Cargo metadata offline across the
    # complete lockfile, including dependencies for non-host targets. Seed that
    # immutable closure before compiling only the host release products.
    run(["cargo", "fetch", "--locked"], cwd=ROOT, env=environment)
    run(
        [
            "cargo",
            "build",
            "--release",
            "--locked",
            "-p",
            "into-markdown-onnxruntime",
            "--bin",
            "onnxruntime-worker",
            "-p",
            "into-markdown-plugin-manager",
            "--bin",
            "package_plugin",
            "-p",
            "license-check",
            "--bin",
            "release-projection",
        ],
        cwd=ROOT,
        env=environment,
    )
    run(
        [
            "cargo",
            "build",
            "--release",
            "--locked",
            "-p",
            "into-markdown-official-provider",
            "--bin",
            "into-md-ocr-provider",
            "--bin",
            "into-md-media-provider",
        ],
        cwd=ROOT,
        env=environment,
    )
    return output / "release"


def build_embedded_core(
    target: str,
    output: pathlib.Path,
    pdfium_root: pathlib.Path,
    ocr_root: pathlib.Path,
) -> pathlib.Path:
    """Build the final CLI once with target-native PDF and OCR payloads embedded."""
    environment = os.environ.copy()
    environment.update(
        {
            "CARGO_INCREMENTAL": "0",
            "CARGO_TARGET_DIR": str(output),
            "INTO_MD_EMBEDDED_PDFIUM_ROOT": str(pdfium_root.resolve()),
            "INTO_MD_EMBEDDED_OCR_ROOT": str(ocr_root.resolve()),
        }
    )
    target_cpu = {
        "x86_64-unknown-linux-gnu": "x86-64",
        "aarch64-unknown-linux-gnu": "generic",
        "x86_64-pc-windows-msvc": "x86-64",
    }[target]
    rustflags = ["-C", "strip=debuginfo", "-C", f"target-cpu={target_cpu}"]
    if target == "x86_64-pc-windows-msvc":
        rustflags.extend(
            ["-C", "debuginfo=0", "-C", "link-arg=/Brepro", "-C", "link-arg=/DEBUG:NONE"]
        )
    environment["RUSTFLAGS"] = " ".join(rustflags)
    run(
        [
            "cargo",
            "build",
            "--release",
            "--locked",
            "-p",
            "into-markdown-cli",
            "--bin",
            "into-md",
            "--features",
            "embedded-runtime",
        ],
        cwd=ROOT,
        env=environment,
    )
    return output / "release" / executable_name("into-md", target)


def downloads_for(target_config: dict) -> dict[str, dict]:
    result = dict(authority()["sharedDownloads"])
    result.update(
        {
            "pdfium": target_config["pdfium"],
            "onnxruntime": target_config["onnxruntime"],
        }
    )
    return result


def extract_member(
    archive: pathlib.Path, member: str, destination: pathlib.Path
) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    if archive.suffix == ".zip" or zipfile.is_zipfile(archive):
        with zipfile.ZipFile(archive) as source:
            names = [name for name in source.namelist() if name.rstrip("/") == member]
            if len(names) != 1:
                raise ReleaseError(f"archive member is absent or ambiguous: {member}")
            with source.open(names[0]) as opened, destination.open("xb") as output:
                shutil.copyfileobj(opened, output, 1024 * 1024)
    else:
        with tarfile.open(archive, "r:*") as source:
            names = [item for item in source if item.name.removeprefix("./") == member.removeprefix("./")]
            if len(names) != 1 or not names[0].isfile():
                raise ReleaseError(f"archive member is absent or ambiguous: {member}")
            opened = source.extractfile(names[0])
            if opened is None:
                raise ReleaseError(f"archive member cannot be read: {member}")
            with opened, destination.open("xb") as output:
                shutil.copyfileobj(opened, output, 1024 * 1024)


def copy_file(source: pathlib.Path, destination: pathlib.Path, executable: bool = False) -> None:
    if not source.is_file() or source.is_symlink():
        raise ReleaseError(f"release input is not a regular file: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(0o755 if executable else 0o644)


def install_state(root: pathlib.Path, bundle: str) -> None:
    write_json(
        root / bundle / "install-state.json",
        {"schemaVersion": 1, "bundleId": bundle, "complete": True},
    )


def runtime_inventory(root: pathlib.Path) -> list[dict]:
    result = []
    for path in regular_files(root):
        relative = path.relative_to(root).as_posix()
        executable = (
            relative.startswith("bin/")
            or relative in {"ffmpeg/ffmpeg", "ffmpeg/ffmpeg.exe"}
        )
        result.append(
            {
                "path": relative,
                "bytes": path.stat().st_size,
                "sha256": sha256(path),
                "executable": executable,
            }
        )
    return result


def write_plugin_declarations(
    runtime: pathlib.Path,
    evidence: pathlib.Path | None,
    plugin_id: str,
    artifact: str,
    target: str,
    components: list[str],
    licenses: list[tuple[pathlib.Path, str]],
    projection_tool: pathlib.Path,
) -> None:
    declaration_root = runtime if evidence is None else evidence / plugin_id
    copy_file(ROOT / "NOTICE", declaration_root / "NOTICE")
    for source, name in licenses:
        copy_file(source, declaration_root / "licenses" / name)
    request = runtime.parent / f"{plugin_id}-release-request.json"
    write_json(
        request,
        {
            "schema_version": 1,
            "target": target,
            "artifact": artifact,
            "version": VERSION,
            "source_revision": source_revision(),
            "components": components,
        },
    )
    inputs = json.loads(run([str(projection_tool), "generate", str(request)], cwd=ROOT))
    for key in ["notice", "third_party_notices", "sbom", "sources"]:
        item = inputs[key]
        destination = declaration_root / item["path"]
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(item["contents"], encoding="utf-8", newline="\n")


def resources(memory: int, temporary: int, timeout: int) -> dict:
    return {
        "maxInputBytes": 536870912,
        "maxOutputBytes": 24 * 1024 * 1024,
        "maxMemoryBytes": memory,
        "maxTemporaryBytes": temporary,
        "timeoutMs": timeout,
    }


def provider_manifest(
    plugin_id: str,
    target: str,
    entrypoint: str,
    runtime: pathlib.Path,
    capabilities: list[dict],
    licenses: list[str],
) -> dict:
    return {
        "schemaVersion": 1,
        "id": plugin_id,
        "version": VERSION,
        "publisher": "official.into-markdown",
        "hostApi": {"minimum": 1, "maximum": 1},
        "protocol": "capability-provider",
        "targets": [
            {
                "triple": target,
                "entrypoint": entrypoint,
                "files": runtime_inventory(runtime),
            }
        ],
        "capabilities": capabilities,
        "permissions": {"network": False, "persistentWorker": False, "childProcesses": True},
        "licenses": licenses,
    }


def build_provider_package(
    packager: pathlib.Path,
    runtime: pathlib.Path,
    manifest: dict,
    target: str,
    signing_key: pathlib.Path,
    output: pathlib.Path,
) -> tuple[str, str]:
    write_json(runtime / "provider.json", manifest)
    template = runtime.parent / f"{manifest['id']}-package.json"
    write_json(
        template,
        {
            "schemaVersion": 1,
            "id": manifest["id"],
            "version": manifest["version"],
            "protocol": "process-v1",
            "supportedTargets": [target],
            "entrypoints": {target: manifest["targets"][0]["entrypoint"]},
            "runtimeManifest": None,
        },
    )
    run(
        [packager, runtime, template, signing_key, "official.into-markdown", output],
        cwd=ROOT,
    )
    with zipfile.ZipFile(output) as package:
        package_manifest = json.loads(package.read("plugin.json"))
    signature = package_manifest["signature"]
    return signature["keyId"], signature["publicKeySha256"]


def validate_ffmpeg(root: pathlib.Path, target: str) -> None:
    expected = {
        "COPYING.LGPLv2.1",
        f"ffmpeg-{target}{'.exe' if target.endswith('windows-msvc') else ''}",
        f"ffmpeg-authority-{target}.json",
        f"ffmpeg-inventory-{target}.json",
        f"ffmpeg-relink-{target}.tar",
    }
    if not root.is_dir() or {path.name for path in root.iterdir()} != expected:
        raise ReleaseError("FFmpeg audit output does not contain the exact artifact set")
    metadata = json.loads(
        (root / f"ffmpeg-authority-{target}.json").read_text(encoding="utf-8")
    )
    executable = root / f"ffmpeg-{target}{'.exe' if target.endswith('windows-msvc') else ''}"
    if (
        metadata.get("schema_version") != 1
        or metadata.get("target") != target
        or metadata.get("executable_bytes") != executable.stat().st_size
        or metadata.get("executable_sha256") != sha256(executable)
    ):
        raise ReleaseError("FFmpeg authority does not match the executable")


def package_plugins(
    packages: pathlib.Path,
    cache: pathlib.Path,
    release_bin: pathlib.Path,
    ffmpeg: pathlib.Path,
    signing_key: pathlib.Path,
    target: str,
    target_config: dict,
    windows_signing_thumbprint: str | None,
    evidence: pathlib.Path | None = None,
    embedded_ocr: pathlib.Path | None = None,
) -> tuple[dict[str, dict], tuple[str, str]]:
    if packages.exists():
        raise ReleaseError("plugin output directory already exists")
    packages.mkdir(parents=True)
    suffix = ".exe" if target == "x86_64-pc-windows-msvc" else ""
    packager = release_bin / executable_name("package_plugin", target)
    projection_tool = release_bin / executable_name("release-projection", target)
    signer = None
    records = {}
    with tempfile.TemporaryDirectory(prefix="into-md-platform-plugins-", dir=packages.parent) as name:
        temporary = pathlib.Path(name)
        ocr = temporary / "ocr"
        copy_file(release_bin / executable_name("into-md-ocr-provider", target), ocr / f"bin/into-md-ocr-provider{suffix}", True)
        copy_file(release_bin / executable_name("onnxruntime-worker", target), ocr / f"bin/onnxruntime-worker{suffix}", True)
        extract_member(cache / "onnxruntime", target_config["onnxruntime"]["member"], ocr / target_config["onnxruntime"]["destination"])
        models = ocr / "models"
        extract_member(cache / "ocr-detector", "PP-OCRv6_tiny_det_onnx_infer/inference.onnx", models / "pp-ocrv6-tiny-detector-onnx/inference.onnx")
        extract_member(cache / "ocr-recognizer", "PP-OCRv6_tiny_rec_onnx_infer/inference.onnx", models / "pp-ocrv6-tiny-recognizer-onnx/inference.onnx")
        copy_file(cache / "ocr-dictionary", models / "pp-ocrv6-tiny-recognizer-onnx/ppocrv6_tiny_dict.txt")
        install_state(models, "pp-ocrv6-tiny-detector-onnx")
        install_state(models, "pp-ocrv6-tiny-recognizer-onnx")
        write_plugin_declarations(ocr, evidence, "official.ocr.ppocrv6", "ocr-plugin", target, OCR_COMPONENTS, [(ROOT / "LICENSE", "paddleocr-Apache-2.0.txt")], projection_tool)
        ocr_manifest = provider_manifest("official.ocr.ppocrv6", target, f"bin/into-md-ocr-provider{suffix}", ocr, [{"id": "ocr", "kind": "ocr", "providerId": "builtin.ocr.ppocrv6-image", "languages": ["zh-Hans", "zh-Hant", "en"], "mediaTypes": ["image/png", "image/jpeg", "image/tiff", "image/bmp", "image/webp"], "resources": resources(805306368, 1073741824, 600000)}], ["Apache-2.0", "MIT"])
        output = packages / "official.ocr.ppocrv6.imp"
        signer = build_provider_package(packager, ocr, ocr_manifest, target, signing_key, output)
        if embedded_ocr is not None:
            if embedded_ocr.exists() or embedded_ocr.is_symlink():
                raise ReleaseError("embedded OCR output already exists")
            shutil.copytree(ocr, embedded_ocr, copy_function=shutil.copyfile)
            for path in regular_files(embedded_ocr):
                relative = path.relative_to(embedded_ocr).as_posix()
                path.chmod(0o755 if relative.startswith("bin/") else 0o644)
        records[ocr_manifest["id"]] = {"file": output.name, "sha256": sha256(output)}

        speech = temporary / "speech"
        copy_file(release_bin / executable_name("into-md-media-provider", target), speech / f"bin/into-md-media-provider{suffix}", True)
        copy_file(release_bin / executable_name("onnxruntime-worker", target), speech / f"bin/onnxruntime-worker{suffix}", True)
        extract_member(cache / "onnxruntime", target_config["onnxruntime"]["member"], speech / target_config["onnxruntime"]["destination"])
        ffmpeg_name = f"ffmpeg-{target}{suffix}"
        copy_file(ffmpeg / ffmpeg_name, speech / f"ffmpeg/ffmpeg{suffix}", True)
        copy_file(ffmpeg / f"ffmpeg-authority-{target}.json", speech / "ffmpeg/authority.json")
        authenticode_files(
            [speech / f"ffmpeg/ffmpeg{suffix}"], windows_signing_thumbprint
        )
        if windows_signing_thumbprint is not None:
            refresh_ffmpeg_authority(
                speech / "ffmpeg/authority.json",
                speech / f"ffmpeg/ffmpeg{suffix}",
            )
        models = speech / "models"
        copy_file(cache / "whisper-small", models / "whisper-small-multilingual/ggml-small.bin")
        copy_file(cache / "silero-vad", models / "silero-vad-3dspeaker-eres2net/silero_vad_half.onnx")
        copy_file(cache / "3dspeaker", models / "silero-vad-3dspeaker-eres2net/3dspeaker_eres2net_base.onnx")
        install_state(models, "whisper-small-multilingual")
        install_state(models, "silero-vad-3dspeaker-eres2net")
        write_plugin_declarations(speech, evidence, "official.media.whisper", "media-plugin", target, SPEECH_COMPONENTS, [(ffmpeg / "COPYING.LGPLv2.1", "ffmpeg-LGPL-2.1.txt"), (ROOT / "third_party/licenses/whisper-model-MIT.txt", "whisper-model-MIT.txt"), (ROOT / "third_party/licenses/silero-vad-MIT.txt", "silero-vad-MIT.txt"), (ROOT / "LICENSE", "3dspeaker-Apache-2.0.txt")], projection_tool)
        evidence_root = speech if evidence is None else evidence / "official.media.whisper"
        copy_file(cache / "ffmpeg-source", evidence_root / "source/ffmpeg-8.1.2.tar.xz")
        copy_file(ffmpeg / f"ffmpeg-relink-{target}.tar", evidence_root / f"relink/ffmpeg-relink-{target}.tar")
        media = ["audio/wav", "audio/mpeg", "audio/mp4", "audio/webm", "audio/flac", "audio/ogg", "video/mp4", "video/webm", "video/quicktime", "video/x-matroska"]
        speech_manifest = provider_manifest("official.media.whisper", target, f"bin/into-md-media-provider{suffix}", speech, [{"id": "transcription", "kind": "transcription", "providerId": "builtin.asr.whisper-small", "languages": ["multilingual"], "mediaTypes": media, "resources": resources(SPEECH_TRANSCRIPTION_MEMORY_BYTES, 4294967296, 7200000)}, {"id": "diarization", "kind": "diarization", "providerId": "builtin.diarization.silero-3dspeaker", "languages": ["multilingual"], "mediaTypes": media, "resources": resources(536870912, 4294967296, 7200000)}], ["Apache-2.0", "LGPL-2.1-or-later", "MIT"])
        output = packages / "official.media.whisper.imp"
        if build_provider_package(packager, speech, speech_manifest, target, signing_key, output) != signer:
            raise ReleaseError("official plugin signer identities disagree")
        records[speech_manifest["id"]] = {"file": output.name, "sha256": sha256(output)}

    if signer is None:
        raise ReleaseError("official signer was not produced")
    return records, signer


def write_release_inputs(output: pathlib.Path, projection: pathlib.Path, target: str) -> None:
    request = output.parent / "core-release-request.json"
    write_json(request, {"schema_version": 1, "target": target, "artifact": "core", "version": VERSION, "source_revision": source_revision(), "components": CORE_COMPONENTS})
    inputs = json.loads(run([projection, "generate", request], cwd=ROOT))
    for key in ["notice", "third_party_notices", "sbom", "sources", "core_catalog"]:
        item = inputs[key]
        destination = output / item["path"]
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(item["contents"], encoding="utf-8", newline="\n")


def material(path: pathlib.Path, root: pathlib.Path, kind: str, components: list[str], spdx: list[str], contents: bool = False) -> dict:
    result = {"path": path.relative_to(root).as_posix(), "bytes": path.stat().st_size, "sha256": sha256(path), "kind": kind, "component_ids": components}
    if spdx:
        result["spdx_expressions"] = spdx
    if contents:
        result["contents"] = path.read_text(encoding="utf-8")
    return result


def write_core_license_materials(output: pathlib.Path, cache: pathlib.Path, target: str, target_config: dict) -> list[dict]:
    destination = output / "share/into-markdown/licenses"
    destination.mkdir(parents=True)
    pdfium = destination / pathlib.Path(target_config["pdfium"]["url"]).name
    copy_file(cache / "pdfium", pdfium)
    result = [material(pdfium, output, "notice-bundle", ["pdfium"], [])]
    for component, source, spdx in [("opencc-transcript-character-table", "LICENSE", "Apache-2.0"), ("imageproc-contour-adaptation", "third_party/licenses/imageproc-MIT.txt", "MIT"), ("clipper2-rust", "third_party/licenses/BSL-1.0.txt", "BSL-1.0"), ("calamine", "third_party/licenses/calamine-MIT.txt", "MIT")]:
        path = destination / f"{component}.txt"
        copy_file(ROOT / source, path)
        result.append(material(path, output, "license-text", [component], [spdx], True))
    sbom = json.loads((output / "SOURCES.json").read_text(encoding="utf-8"))
    npm = {item for item in distributed_source_ids(sbom) if item.startswith("npm:")}
    react = sorted(npm - {"npm:lucide-react@1.31.0"})
    if react:
        path = destination / "npm-react-MIT.txt"
        copy_file(ROOT / "third_party/licenses/npm/react-MIT.txt", path)
        result.append(material(path, output, "license-text", react, ["MIT"], True))
    if "npm:lucide-react@1.31.0" in npm:
        path = destination / "npm-lucide-ISC-MIT.txt"
        copy_file(ROOT / "third_party/licenses/npm/lucide-ISC-MIT.txt", path)
        result.append(material(path, output, "license-text", ["npm:lucide-react@1.31.0"], ["ISC", "MIT"], True))
    for component in sbom["components"]:
        if not component["id"].startswith("cargo:"):
            continue
        if component["id"] == "cargo:whisper-rs@0.16.0":
            path = destination / "whisper-rs-Unlicense.txt"
            copy_file(ROOT / "third_party/whisper-rs-0.16.0/LICENSE", path)
            result.append(
                material(path, output, "license-text", [component["id"]], ["Unlicense"], True)
            )
            continue
        checksum = next(item["digest"] for item in component["integrity"] if item["subject"].startswith("crates.io archive"))
        archive_name = component["id"].removeprefix("cargo:").replace("@", "-") + ".crate"
        candidates = list(pathlib.Path.home().glob(f".cargo/registry/cache/*/{archive_name}"))
        if len(candidates) != 1 or sha256(candidates[0]) != checksum:
            raise ReleaseError(f"fixed Cargo source archive is unavailable: {component['id']}")
        path = destination / "cargo" / archive_name
        copy_file(candidates[0], path)
        result.append(material(path, output, "upstream-source-archive", [component["id"]], []))
    return result


def core_projection(output: pathlib.Path, materials: list[dict], target: str, pdfium_path: str, native_transformations: list[dict]) -> dict:
    material_paths = {item["path"] for item in materials}
    selected = distributed_source_ids(
        json.loads((output / "SOURCES.json").read_text(encoding="utf-8"))
    )
    bundled_path = "share/into-markdown/plugins/packages/official.ocr.ppocrv6.imp"
    bundled = bundled_ocr_components(output / bundled_path)
    bundled_cargo = {item for item in bundled if item.startswith("cargo:")}
    if not bundled_cargo.issubset(selected):
        raise ReleaseError("bundled OCR Rust closure is absent from the Core authority")
    embedded = [item for item in selected if item != "pdfium" and item not in bundled_cargo]
    files = []
    for path in regular_files(output):
        relative = path.relative_to(output).as_posix()
        if relative == "archive-manifest.json":
            continue
        if relative in material_paths:
            kind, owner = "license-material", None
        elif relative in {"LICENSE", "NOTICE"}:
            kind, owner = "declaration", None
        elif relative in {"THIRD_PARTY_NOTICES.md", "SBOM.spdx.json", "SOURCES.json", "core-catalog.json"}:
            kind, owner = "generated", None
        elif relative == pdfium_path:
            kind, owner = "component", "pdfium"
        else:
            kind, owner = "project", None
        entry = {"path": relative, "bytes": path.stat().st_size, "sha256": sha256(path), "kind": kind}
        if owner:
            entry["component_id"] = owner
        if relative in {"bin/into-md", "bin/into-md.exe"}:
            entry["embedded_components"] = embedded
        elif relative == bundled_path:
            entry["embedded_components"] = sorted(bundled_cargo)
        files.append(entry)
    return {"schema_version": 1, "target": target, "version": VERSION, "source_revision": source_revision(), "components": selected, "files": files, "license_materials": materials, "native_transformations": native_transformations}


def assemble_core(output: pathlib.Path, cache: pathlib.Path, release_bin: pathlib.Path, packages: pathlib.Path, records: dict, signer: tuple[str, str], base_url: str, target: str, target_config: dict, windows_signing_thumbprint: str | None) -> pathlib.Path:
    if output.exists():
        raise ReleaseError("Core output directory already exists")
    output.mkdir(parents=True)
    suffix = ".exe" if target == "x86_64-pc-windows-msvc" else ""
    for name in ["into-md", "installed-smoke", "archive-check", "into-md-installer"]:
        copy_file(release_bin / executable_name(name, target), output / f"bin/{name}{suffix}", True)
    if target_config["os"] == "linux":
        copy_file(pathlib.Path(__file__).with_name("install"), output / "install", True)
        copy_file(pathlib.Path(__file__).with_name("uninstall"), output / "uninstall", True)
    else:
        copy_file(pathlib.Path(__file__).with_name("Install.ps1"), output / "Install.ps1")
        copy_file(pathlib.Path(__file__).with_name("Uninstall.ps1"), output / "Uninstall.ps1")
        copy_file(pathlib.Path(__file__).with_name("Install.cmd"), output / "Install.cmd")
        copy_file(pathlib.Path(__file__).with_name("Uninstall.cmd"), output / "Uninstall.cmd")
    extract_member(cache / "pdfium", target_config["pdfium"]["member"], output / target_config["pdfium"]["destination"])
    native_transformations = []
    fixture_root = output / "share/into-markdown/smoke/fixtures"
    for relative in FIXTURES:
        copy_file(ROOT / "fixtures/small" / relative, fixture_root / relative)
    for name in ["normal.doc", "normal.ppt", "normal.xls"]:
        copy_file(ROOT / "tools/macos-release/fixtures" / name, fixture_root / "legacy" / name)
    materialize_agent_skill(output / AGENT_SKILL_RELATIVE)
    materialize_rust(output / "lib/into-markdown-rust.zip")
    copy_file(ROOT / "LICENSE", output / "LICENSE")
    bundled_ocr = packages / "official.ocr.ppocrv6.imp"
    copy_file(
        bundled_ocr,
        output / "share/into-markdown/plugins/packages/official.ocr.ppocrv6.imp",
    )
    catalog = {}
    for identity, record in records.items():
        catalog[identity] = {
            "sha256": record["sha256"],
            **(
                {"file": "official.ocr.ppocrv6.imp"}
                if identity == "official.ocr.ppocrv6"
                else {"url": f"{base_url.rstrip('/')}/{published_plugin_file(record['file'], target)}"}
            ),
        }
    write_json(output / "share/into-markdown/plugins/official-publisher.json", {"schemaVersion": 2, "signingKeyId": signer[0], "signingKeySha256": signer[1], "packages": catalog})
    projection = release_bin / executable_name("release-projection", target)
    write_release_inputs(output, projection, target)
    materials = write_core_license_materials(output, cache, target, target_config)
    write_json(output / "archive-manifest.json", core_projection(output, materials, target, target_config["pdfium"]["destination"], native_transformations))
    run([projection, "verify", output / "archive-manifest.json"], cwd=ROOT)
    return output


def create_archive(source: pathlib.Path, destination: pathlib.Path, target_config: dict, epoch: int) -> None:
    if target_config["archive"] == "tar.gz":
        sys.path.append(str(pathlib.Path(__file__).resolve().parents[1] / "macos-release"))
        from archive import create
        create(source, destination, epoch)
        return
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as output:
        for path in regular_files(source):
            relative = path.relative_to(source).as_posix()
            info = zipfile.ZipInfo(relative, (2026, 1, 1, 0, 0, 0))
            info.create_system = 0
            info.external_attr = 0o100644 << 16
            output.writestr(info, path.read_bytes(), compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True, choices=sorted(authority()["targets"]))
    parser.add_argument("--output", type=pathlib.Path)
    parser.add_argument("--cache", type=pathlib.Path)
    parser.add_argument("--build-root", required=True, type=pathlib.Path)
    parser.add_argument("--ffmpeg-artifacts", type=pathlib.Path)
    parser.add_argument("--archive", type=pathlib.Path)
    parser.add_argument("--plugins-output", type=pathlib.Path)
    parser.add_argument("--plugin-base-url")
    parser.add_argument("--plugin-signing-key", type=pathlib.Path)
    parser.add_argument("--evidence-output", type=pathlib.Path)
    parser.add_argument("--windows-signing-thumbprint")
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--build-only", action="store_true")
    arguments = parser.parse_args()
    config = authority()["targets"][arguments.target]
    check_host(arguments.target, config)
    if arguments.windows_signing_thumbprint is not None and config["os"] != "windows":
        raise ReleaseError("Windows signing is only valid for the Windows target")
    if arguments.build_only:
        if arguments.skip_build:
            raise ReleaseError("--build-only and --skip-build conflict")
        build(arguments.target, arguments.build_root.resolve())
        return
    required = [arguments.output, arguments.cache, arguments.ffmpeg_artifacts, arguments.archive, arguments.plugins_output, arguments.plugin_base_url, arguments.plugin_signing_key]
    if any(value is None for value in required):
        raise ReleaseError("complete assembly arguments are required unless --build-only is used")
    validate_ffmpeg(arguments.ffmpeg_artifacts.resolve(), arguments.target)
    release_bin = arguments.build_root.resolve() / "release"
    if not arguments.skip_build:
        release_bin = build(arguments.target, arguments.build_root.resolve())
    acquire(arguments.cache.resolve(), downloads_for(config))
    records, signer = package_plugins(arguments.plugins_output.resolve(), arguments.cache.resolve(), release_bin, arguments.ffmpeg_artifacts.resolve(), arguments.plugin_signing_key.resolve(), arguments.target, config, arguments.windows_signing_thumbprint, arguments.evidence_output.resolve() if arguments.evidence_output else None)
    stage = assemble_core(arguments.output.resolve(), arguments.cache.resolve(), release_bin, arguments.plugins_output.resolve(), records, signer, arguments.plugin_base_url, arguments.target, config, arguments.windows_signing_thumbprint)
    create_archive(stage, arguments.archive.resolve(), config, authority()["sourceDateEpoch"])
    arguments.archive.with_name(arguments.archive.name + ".sha256").write_text(f"{sha256(arguments.archive)}  {arguments.archive.name}\n", encoding="ascii")
    print(f"{sha256(arguments.archive)}  {arguments.archive}")


if __name__ == "__main__":
    try:
        main()
    except ReleaseError as error:
        print(f"platform-release: {error}", file=sys.stderr)
        raise SystemExit(1)
