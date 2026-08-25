"""Build the macOS ARM64 Core archive and two signed capability plugins."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import shutil
import sys
import tempfile
import zipfile

from acquire import acquire, extract_tar
from archive import create as create_archive
from audit import audit as audit_macho
from common import ROOT, ReleaseError, authority, regular_files, run, sha256, write_json
from rust_package import materialize as materialize_rust

sys.path.append(str(pathlib.Path(__file__).resolve().parents[1] / "skill-release"))
from skill_release import CORE_RELATIVE as AGENT_SKILL_RELATIVE  # noqa: E402
from skill_release import materialize as materialize_agent_skill  # noqa: E402

TARGET = "aarch64-apple-darwin"
VERSION = "0.0.0"
CORE_COMPONENTS = ["pdfium"]
OCR_COMPONENTS = ["onnxruntime-cpu", "ppocrv6-tiny-detector-onnx-model", "ppocrv6-tiny-recognizer-character-table", "ppocrv6-tiny-recognizer-onnx-model"]
SPEECH_COMPONENTS = ["ffmpeg", "onnxruntime-cpu", "whisper-small", "silero-vad-half-onnx-model", "3dspeaker-eres2net-base-onnx-model"]
SPEECH_TRANSCRIPTION_MEMORY_BYTES = 1536 * 1024 * 1024
FIXTURES = [
    "docx/normal.docx", "docx/corrupt.docx", "epub/normal.epub", "msg/normal.msg",
    "ocr/ocr-english-clear-1.png", "pdf/structures.pdf", "rtf/normal.rtf",
    "text/normal.txt", "xlsx/normal.xlsx", "xlsb/normal.xlsb", "pptx/normal.pptx",
    "odt/normal.odt", "ods/normal.ods", "odp/normal.odp",
]


def check_host() -> None:
    config = authority()
    if os.uname().sysname != "Darwin" or os.uname().machine != "arm64":
        raise ReleaseError("release assembly requires a native macOS ARM64 host")
    rust = run(["rustc", "--version"]).split()[1]
    if rust != config["rust"]:
        raise ReleaseError(f"rustc {rust} disagrees with fixed toolchain {config['rust']}")


def source_revision() -> str:
    return run(["git", "rev-parse", "HEAD"], cwd=ROOT).strip()


def distributed_source_ids(manifest: dict) -> list[str]:
    return [item["id"] for item in manifest["components"] if item["distributed"]]


def build(target: pathlib.Path) -> None:
    environment = os.environ.copy()
    cargo_home = pathlib.Path(environment.get("CARGO_HOME", pathlib.Path.home() / ".cargo")).resolve()
    rustup_home = pathlib.Path(environment.get("RUSTUP_HOME", pathlib.Path.home() / ".rustup")).resolve()
    sysroot = pathlib.Path(run(["rustc", "--print", "sysroot"]).strip()).resolve()
    remaps = [(ROOT, "/usr/src/into-markdown"), (target, "/usr/src/into-markdown-target"), (cargo_home, "/usr/src/cargo-home"), (rustup_home, "/usr/src/rustup-home"), (sysroot, "/usr/src/rust-sysroot")]
    environment.update({
        "CARGO_INCREMENTAL": "0",
        "MACOSX_DEPLOYMENT_TARGET": authority()["minimumMacos"],
        "CFLAGS": " ".join(f"-ffile-prefix-map={source}={destination}" for source, destination in remaps),
        "CXXFLAGS": " ".join(f"-ffile-prefix-map={source}={destination}" for source, destination in remaps),
        "RUSTFLAGS": " ".join(f"--remap-path-prefix={source}={destination}" for source, destination in remaps) + " -C strip=debuginfo " + f"-C link-arg=-mmacosx-version-min={authority()['minimumMacos']}",
        "CARGO_TARGET_DIR": str(target),
    })
    run(["cargo", "build", "-j2", "--release", "--locked", "-p", "into-markdown-cli", "--bin", "into-md", "-p", "into-markdown-onnxruntime", "--bin", "onnxruntime-worker", "-p", "into-markdown-plugin-manager", "--bin", "package_plugin", "-p", "installed-smoke", "--bin", "installed-smoke", "-p", "installed-smoke", "--bin", "archive-check", "-p", "license-check", "--bin", "release-projection"], cwd=ROOT, env=environment)
    run(["cargo", "build", "-j2", "--release", "--locked", "--features", "metal", "-p", "into-markdown-official-provider", "--bin", "into-md-ocr-provider", "--bin", "into-md-media-provider"], cwd=ROOT, env=environment)


def validate_ffmpeg_artifacts(root: pathlib.Path) -> None:
    expected = {"COPYING.LGPLv2.1", "ffmpeg-aarch64-apple-darwin", "ffmpeg-authority-aarch64-apple-darwin.json", "ffmpeg-inventory-aarch64-apple-darwin.json", "ffmpeg-relink-aarch64-apple-darwin.tar"}
    if not root.is_dir() or root.is_symlink():
        raise ReleaseError("FFmpeg audit output is not a trusted directory")
    entries = list(root.iterdir())
    if {entry.name for entry in entries} != expected or any(not entry.is_file() or entry.is_symlink() for entry in entries):
        raise ReleaseError("FFmpeg audit output does not contain the reviewed artifact set")
    metadata = json.loads((root / "ffmpeg-authority-aarch64-apple-darwin.json").read_text(encoding="utf-8"))
    executable = root / "ffmpeg-aarch64-apple-darwin"
    if metadata.get("schema_version") != 1 or metadata.get("target") != TARGET or metadata.get("executable_bytes") != executable.stat().st_size or metadata.get("executable_sha256") != sha256(executable):
        raise ReleaseError("FFmpeg authority does not match its executable")


def copy_file(source: pathlib.Path, destination: pathlib.Path, mode: int) -> None:
    if not source.is_file() or source.is_symlink():
        raise ReleaseError(f"release input is not a regular file: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(mode)


def codesign_files(
    paths: list[pathlib.Path],
    identity: str | None,
    fixed_sources: dict[pathlib.Path, str] | None = None,
) -> dict[str, str]:
    """Sign exact release copies and bind any transformed native source bytes."""
    if identity is None:
        return {}
    derivatives: dict[str, str] = {}
    for path in paths:
        if not path.is_file() or path.is_symlink():
            raise ReleaseError(f"code-signing input is not a regular file: {path}")
        expected = (fixed_sources or {}).get(path)
        if expected is not None and sha256(path) != expected:
            raise ReleaseError(f"code-signing source differs from release authority: {path.name}")
        command = ["/usr/bin/codesign", "--force", "--sign", identity]
        if identity != "-":
            command.extend(["--options", "runtime", "--timestamp"])
        command.append(str(path))
        run(command)
        run(["/usr/bin/codesign", "--verify", "--strict", "--verbose=2", str(path)])
        if expected is not None:
            derivatives[sha256(path)] = expected
    return derivatives


def refresh_ffmpeg_authority(authority_path: pathlib.Path, executable: pathlib.Path) -> None:
    value = json.loads(authority_path.read_text(encoding="utf-8"))
    value["executable_bytes"] = executable.stat().st_size
    value["executable_sha256"] = sha256(executable)
    write_json(authority_path, value)


def install_state(root: pathlib.Path, bundle: str) -> None:
    write_json(root / bundle / "install-state.json", {"schemaVersion": 1, "bundleId": bundle, "complete": True})


def runtime_inventory(root: pathlib.Path) -> list[dict]:
    result = []
    for path in regular_files(root):
        relative = path.relative_to(root).as_posix()
        executable = relative.startswith("bin/") or relative == "ffmpeg/ffmpeg"
        result.append({"path": relative, "bytes": path.stat().st_size, "sha256": sha256(path), "executable": executable})
    return result


def write_plugin_declarations(
    runtime: pathlib.Path,
    plugin_id: str,
    artifact: str,
    components: list[str],
    licenses: list[tuple[pathlib.Path, str]],
    projection_tool: pathlib.Path,
) -> None:
    copy_file(ROOT / "NOTICE", runtime / "NOTICE", 0o644)
    for source, name in licenses:
        copy_file(source, runtime / "licenses" / name, 0o644)
    request = runtime.parent / f"{plugin_id}-release-request.json"
    write_json(
        request,
        {
            "schema_version": 1,
            "target": TARGET,
            "artifact": artifact,
            "version": VERSION,
            "source_revision": source_revision(),
            "components": components,
        },
    )
    inputs = json.loads(run([str(projection_tool), "generate", str(request)], cwd=ROOT))
    for key in ["notice", "third_party_notices", "sbom", "sources"]:
        item = inputs[key]
        destination = runtime / item["path"]
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(item["contents"], encoding="utf-8")


def resources(memory_bytes: int, temporary_bytes: int, timeout_ms: int) -> dict:
    # The process-v1 JSON frame is capped at 64 MiB and reserves worst-case
    # string escaping overhead before the provider is launched.
    return {"maxInputBytes": 536870912, "maxOutputBytes": 24 * 1024 * 1024, "maxMemoryBytes": memory_bytes, "maxTemporaryBytes": temporary_bytes, "timeoutMs": timeout_ms}


def provider_manifest(plugin_id: str, entrypoint: str, runtime: pathlib.Path, capabilities: list[dict], licenses: list[str]) -> dict:
    return {
        "schemaVersion": 1, "id": plugin_id, "version": VERSION, "publisher": "official.into-markdown",
        "hostApi": {"minimum": 1, "maximum": 1}, "protocol": "capability-provider",
        "targets": [{"triple": TARGET, "entrypoint": entrypoint, "files": runtime_inventory(runtime)}],
        "capabilities": capabilities,
        "permissions": {"network": False, "persistentWorker": False, "childProcesses": True},
        "licenses": licenses,
    }


def build_provider_package(packager: pathlib.Path, runtime: pathlib.Path, manifest: dict, signing_key: pathlib.Path, output: pathlib.Path) -> tuple[str, str]:
    write_json(runtime / "provider.json", manifest)
    template = runtime.parent / f"{manifest['id']}-package.json"
    write_json(template, {"schemaVersion": 1, "id": manifest["id"], "version": manifest["version"], "protocol": "process-v1", "supportedTargets": [TARGET], "entrypoints": {TARGET: manifest["targets"][0]["entrypoint"]}, "runtimeManifest": None})
    run([str(packager), str(runtime), str(template), str(signing_key), "official.into-markdown", str(output)], cwd=ROOT)
    with zipfile.ZipFile(output) as package:
        package_manifest = json.loads(package.read("plugin.json"))
    signature = package_manifest["signature"]
    output.with_name(output.name + ".sha256").write_text(f"{sha256(output)}  {output.name}\n", encoding="ascii")
    return signature["keyId"], signature["publicKeySha256"]


def package_official_plugins(packages: pathlib.Path, cache: pathlib.Path, release_bin: pathlib.Path, ffmpeg_artifacts: pathlib.Path, signing_key: pathlib.Path, codesign_identity: str | None) -> tuple[dict[str, dict], tuple[str, str]]:
    if packages.exists():
        raise ReleaseError("plugin output directory already exists")
    if not signing_key.is_file() or signing_key.is_symlink():
        raise ReleaseError("official plugin signing key is unavailable")
    packages.mkdir(parents=True)
    packager = release_bin / "package_plugin"
    projection_tool = release_bin / "release-projection"
    records: dict[str, dict] = {}
    signer: tuple[str, str] | None = None
    with tempfile.TemporaryDirectory(prefix="into-md-capability-runtime-", dir=packages.parent) as name:
        temporary = pathlib.Path(name)

        ocr = temporary / "ocr"
        copy_file(release_bin / "into-md-ocr-provider", ocr / "bin/into-md-ocr-provider", 0o755)
        copy_file(release_bin / "onnxruntime-worker", ocr / "bin/onnxruntime-worker", 0o755)
        extract_tar(cache / "onnxruntime", ocr / "onnxruntime", {"./onnxruntime-osx-arm64-1.29.0/lib/libonnxruntime.dylib": "lib/libonnxruntime.dylib"})
        models = ocr / "models"
        extract_tar(cache / "ocr-detector", models / "pp-ocrv6-tiny-detector-onnx", {"PP-OCRv6_tiny_det_onnx_infer/inference.onnx": "inference.onnx"})
        extract_tar(cache / "ocr-recognizer", models / "pp-ocrv6-tiny-recognizer-onnx", {"PP-OCRv6_tiny_rec_onnx_infer/inference.onnx": "inference.onnx"})
        copy_file(cache / "ocr-dictionary", models / "pp-ocrv6-tiny-recognizer-onnx/ppocrv6_tiny_dict.txt", 0o644)
        install_state(models, "pp-ocrv6-tiny-detector-onnx")
        install_state(models, "pp-ocrv6-tiny-recognizer-onnx")
        codesign_files(
            [ocr / "bin/into-md-ocr-provider", ocr / "bin/onnxruntime-worker"],
            codesign_identity,
        )
        write_plugin_declarations(ocr, "official.ocr.ppocrv6", "ocr-plugin", OCR_COMPONENTS, [(ROOT / "LICENSE", "paddleocr-Apache-2.0.txt")], projection_tool)
        ocr_manifest = provider_manifest("official.ocr.ppocrv6", "bin/into-md-ocr-provider", ocr, [{"id": "ocr", "kind": "ocr", "providerId": "builtin.ocr.ppocrv6-image", "languages": ["zh-Hans", "zh-Hant", "en"], "mediaTypes": ["image/png", "image/jpeg", "image/tiff", "image/bmp", "image/webp"], "resources": resources(805306368, 1073741824, 600000)}], ["Apache-2.0", "MIT"])
        ocr_output = packages / "official.ocr.ppocrv6.imp"
        signer = build_provider_package(packager, ocr, ocr_manifest, signing_key, ocr_output)
        records["official.ocr.ppocrv6"] = {"sha256": sha256(ocr_output), "file": ocr_output.name}

        speech = temporary / "speech"
        copy_file(release_bin / "into-md-media-provider", speech / "bin/into-md-media-provider", 0o755)
        copy_file(release_bin / "onnxruntime-worker", speech / "bin/onnxruntime-worker", 0o755)
        extract_tar(cache / "onnxruntime", speech / "onnxruntime", {"./onnxruntime-osx-arm64-1.29.0/lib/libonnxruntime.dylib": "lib/libonnxruntime.dylib"})
        copy_file(ffmpeg_artifacts / "ffmpeg-aarch64-apple-darwin", speech / "ffmpeg/ffmpeg", 0o755)
        copy_file(ffmpeg_artifacts / "ffmpeg-authority-aarch64-apple-darwin.json", speech / "ffmpeg/authority.json", 0o644)
        models = speech / "models"
        copy_file(cache / "whisper-small", models / "whisper-small-multilingual/ggml-small.bin", 0o644)
        copy_file(cache / "silero-vad", models / "silero-vad-3dspeaker-eres2net/silero_vad_half.onnx", 0o644)
        copy_file(cache / "3dspeaker", models / "silero-vad-3dspeaker-eres2net/3dspeaker_eres2net_base.onnx", 0o644)
        install_state(models, "whisper-small-multilingual")
        install_state(models, "silero-vad-3dspeaker-eres2net")
        codesign_files(
            [
                speech / "bin/into-md-media-provider",
                speech / "bin/onnxruntime-worker",
                speech / "ffmpeg/ffmpeg",
            ],
            codesign_identity,
        )
        if codesign_identity is not None:
            refresh_ffmpeg_authority(speech / "ffmpeg/authority.json", speech / "ffmpeg/ffmpeg")
        write_plugin_declarations(speech, "official.media.whisper", "media-plugin", SPEECH_COMPONENTS, [(ffmpeg_artifacts / "COPYING.LGPLv2.1", "ffmpeg-LGPL-2.1.txt"), (ROOT / "third_party/licenses/whisper-model-MIT.txt", "whisper-model-MIT.txt"), (ROOT / "third_party/licenses/silero-vad-MIT.txt", "silero-vad-MIT.txt"), (ROOT / "LICENSE", "3dspeaker-Apache-2.0.txt")], projection_tool)
        copy_file(cache / "ffmpeg-source", speech / "source/ffmpeg-8.1.2.tar.xz", 0o644)
        copy_file(ffmpeg_artifacts / "ffmpeg-relink-aarch64-apple-darwin.tar", speech / "relink/ffmpeg-relink-aarch64-apple-darwin.tar", 0o644)
        media_types = ["audio/wav", "audio/mpeg", "audio/mp4", "audio/webm", "audio/flac", "audio/ogg", "video/mp4", "video/webm", "video/quicktime", "video/x-matroska"]
        speech_manifest = provider_manifest("official.media.whisper", "bin/into-md-media-provider", speech, [
            {"id": "transcription", "kind": "transcription", "providerId": "builtin.asr.whisper-small", "languages": ["multilingual"], "mediaTypes": media_types, "resources": resources(SPEECH_TRANSCRIPTION_MEMORY_BYTES, 4294967296, 7200000)},
            {"id": "diarization", "kind": "diarization", "providerId": "builtin.diarization.silero-3dspeaker", "languages": ["multilingual"], "mediaTypes": media_types, "resources": resources(536870912, 4294967296, 7200000)},
        ], ["Apache-2.0", "LGPL-2.1-or-later", "MIT"])
        speech_output = packages / "official.media.whisper.imp"
        if build_provider_package(packager, speech, speech_manifest, signing_key, speech_output) != signer:
            raise ReleaseError("official plugin signer identities disagree")
        records["official.media.whisper"] = {"sha256": sha256(speech_output), "file": speech_output.name}

    if signer is None:
        raise ReleaseError("official plugin signer was not produced")
    audit_macho(packages, "plugins", authority()["minimumMacos"])
    return records, signer


def write_release_inputs(output: pathlib.Path, projection_tool: pathlib.Path) -> None:
    request = output.parent / "core-release-request.json"
    write_json(request, {"schema_version": 1, "target": TARGET, "artifact": "core", "version": VERSION, "source_revision": source_revision(), "components": CORE_COMPONENTS})
    inputs = json.loads(run([str(projection_tool), "generate", str(request)], cwd=ROOT))
    for key in ["notice", "third_party_notices", "sbom", "sources", "core_catalog"]:
        item = inputs[key]
        destination = output / item["path"]
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(item["contents"], encoding="utf-8")


def material(path: pathlib.Path, root: pathlib.Path, kind: str, components: list[str], spdx: list[str], contents: bool = False) -> dict:
    result = {"path": path.relative_to(root).as_posix(), "bytes": path.stat().st_size, "sha256": sha256(path), "kind": kind, "component_ids": components}
    if spdx:
        result["spdx_expressions"] = spdx
    if contents:
        result["contents"] = path.read_text(encoding="utf-8")
    return result


def write_core_license_materials(output: pathlib.Path, cache: pathlib.Path) -> list[dict]:
    destination = output / "share/into-markdown/licenses"
    destination.mkdir(parents=True)
    result = []
    pdfium = destination / "pdfium-mac-arm64.tgz"
    copy_file(cache / "pdfium", pdfium, 0o644)
    result.append(material(pdfium, output, "notice-bundle", ["pdfium"], []))
    for component, source, spdx in [("opencc-transcript-character-table", "LICENSE", "Apache-2.0"), ("imageproc-contour-adaptation", "third_party/licenses/imageproc-MIT.txt", "MIT"), ("clipper2-rust", "third_party/licenses/BSL-1.0.txt", "BSL-1.0"), ("calamine", "third_party/licenses/calamine-MIT.txt", "MIT")]:
        path = destination / f"{component}.txt"
        copy_file(ROOT / source, path, 0o644)
        result.append(material(path, output, "license-text", [component], [spdx], contents=True))
    sbom = json.loads((output / "SOURCES.json").read_text(encoding="utf-8"))
    npm_components = {
        item for item in distributed_source_ids(sbom) if item.startswith("npm:")
    }
    react_components = sorted(npm_components - {"npm:lucide-react@1.31.0"})
    if react_components:
        path = destination / "npm-react-MIT.txt"
        copy_file(ROOT / "third_party/licenses/npm/react-MIT.txt", path, 0o644)
        result.append(material(path, output, "license-text", react_components, ["MIT"], contents=True))
    if "npm:lucide-react@1.31.0" in npm_components:
        path = destination / "npm-lucide-ISC-MIT.txt"
        copy_file(ROOT / "third_party/licenses/npm/lucide-ISC-MIT.txt", path, 0o644)
        result.append(material(path, output, "license-text", ["npm:lucide-react@1.31.0"], ["ISC", "MIT"], contents=True))
    for component in sbom["components"]:
        if not component["id"].startswith("cargo:"):
            continue
        checksum = next(evidence["digest"] for evidence in component["integrity"] if evidence["subject"].startswith("crates.io archive"))
        name_version = component["id"].removeprefix("cargo:").replace("@", "-") + ".crate"
        candidates = list(pathlib.Path.home().glob(f".cargo/registry/cache/*/{name_version}"))
        if len(candidates) != 1 or sha256(candidates[0]) != checksum:
            raise ReleaseError(f"fixed Cargo source archive is unavailable: {component['id']}")
        path = destination / "cargo" / name_version
        copy_file(candidates[0], path, 0o644)
        result.append(material(path, output, "upstream-source-archive", [component["id"]], []))
    return result


def core_projection(output: pathlib.Path, materials: list[dict], native_transformations: list[dict]) -> dict:
    material_paths = {item["path"] for item in materials}
    sbom = json.loads((output / "SOURCES.json").read_text(encoding="utf-8"))
    selected = distributed_source_ids(sbom)
    # PDFium is the only standalone native Core file. All other selected
    # runtime components are linked or compiled into the product executable.
    embedded = [item for item in selected if item != "pdfium"]
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
        elif relative == "lib/pdfium/libpdfium.dylib":
            kind, owner = "component", "pdfium"
        else:
            kind, owner = "project", None
        entry = {"path": relative, "bytes": path.stat().st_size, "sha256": sha256(path), "kind": kind}
        if owner:
            entry["component_id"] = owner
        if relative == "bin/into-md":
            entry["embedded_components"] = embedded
        files.append(entry)
    return {"schema_version": 1, "target": TARGET, "version": VERSION, "source_revision": source_revision(), "components": selected, "files": files, "license_materials": materials, "native_transformations": native_transformations}


def assemble_core(output: pathlib.Path, cache: pathlib.Path, release_bin: pathlib.Path, records: dict[str, dict], signer: tuple[str, str], plugin_base_url: str, codesign_identity: str | None) -> pathlib.Path:
    if output.exists():
        raise ReleaseError("Core output directory already exists")
    output.mkdir(parents=True)
    binaries = output / "bin"
    binaries.mkdir()
    for name in ["into-md", "installed-smoke", "archive-check"]:
        copy_file(release_bin / name, binaries / name, 0o755)
    for name in ["install", "uninstall"]:
        copy_file(pathlib.Path(__file__).with_name(name), output / name, 0o755)
    extract_tar(cache / "pdfium", output / "lib/pdfium", {"lib/libpdfium.dylib": "libpdfium.dylib"})
    pdfium = output / "lib/pdfium/libpdfium.dylib"
    pdfium_source = {"bytes": pdfium.stat().st_size, "sha256": sha256(pdfium)}
    signed_derivatives = codesign_files(
        [*(binaries / name for name in ["into-md", "installed-smoke", "archive-check"]), pdfium],
        codesign_identity,
        {pdfium: "33c98063af28c0b7cbf8227f4422bf5c15942df2455cf7f0a5dce3dc601d52b0"},
    )
    native_transformations = []
    if codesign_identity is not None:
        native_transformations.append({
            "component_id": "pdfium",
            "path": "lib/pdfium/libpdfium.dylib",
            "kind": "apple-code-sign",
            "source_bytes": pdfium_source["bytes"],
            "source_sha256": pdfium_source["sha256"],
            "output_bytes": pdfium.stat().st_size,
            "output_sha256": sha256(pdfium),
        })
    fixture_root = output / "share/into-markdown/smoke/fixtures"
    for relative in FIXTURES:
        copy_file(ROOT / "fixtures/small" / relative, fixture_root / relative, 0o644)
    for name in ["normal.doc", "normal.ppt", "normal.xls"]:
        copy_file(pathlib.Path(__file__).with_name("fixtures") / name, fixture_root / "legacy" / name, 0o644)
    materialize_agent_skill(output / AGENT_SKILL_RELATIVE)
    materialize_rust(output / "lib/into-markdown-rust")
    copy_file(ROOT / "LICENSE", output / "LICENSE", 0o644)
    catalog_records = {plugin_id: {"url": f"{plugin_base_url.rstrip('/')}/{record['file']}", "sha256": record["sha256"]} for plugin_id, record in records.items()}
    write_json(output / "share/into-markdown/plugins/official-publisher.json", {"schemaVersion": 2, "signingKeyId": signer[0], "signingKeySha256": signer[1], "packages": catalog_records})
    projection_tool = release_bin / "release-projection"
    write_release_inputs(output, projection_tool)
    materials = write_core_license_materials(output, cache)
    write_json(output / "archive-manifest.json", core_projection(output, materials, native_transformations))
    run([str(projection_tool), "verify", str(output / "archive-manifest.json")], cwd=ROOT)
    audit_macho(output, "core", authority()["minimumMacos"], signed_derivatives)
    return output


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=pathlib.Path, help="Core staging directory")
    parser.add_argument("--cache", required=True, type=pathlib.Path)
    parser.add_argument("--build-root", required=True, type=pathlib.Path)
    parser.add_argument("--ffmpeg-artifacts", required=True, type=pathlib.Path)
    parser.add_argument("--archive", required=True, type=pathlib.Path, help="Core archive")
    parser.add_argument("--plugins-output", required=True, type=pathlib.Path)
    parser.add_argument("--plugin-base-url", required=True)
    parser.add_argument("--plugin-signing-key", required=True, type=pathlib.Path)
    parser.add_argument(
        "--codesign-identity",
        help="Developer ID Application identity; use '-' only for local release-gate testing",
    )
    parser.add_argument("--skip-build", action="store_true")
    arguments = parser.parse_args()
    check_host()
    validate_ffmpeg_artifacts(arguments.ffmpeg_artifacts.resolve())
    if not arguments.skip_build:
        build(arguments.build_root.resolve())
    acquire(arguments.cache.resolve(), {"pdfium", "onnxruntime", "ocr-detector", "ocr-recognizer", "ocr-dictionary", "ffmpeg-source", "whisper-small", "silero-vad", "3dspeaker", "libreoffice"})
    release_bin = arguments.build_root.resolve() / "release"
    records, signer = package_official_plugins(arguments.plugins_output.resolve(), arguments.cache.resolve(), release_bin, arguments.ffmpeg_artifacts.resolve(), arguments.plugin_signing_key.resolve(), arguments.codesign_identity)
    stage = assemble_core(arguments.output.resolve(), arguments.cache.resolve(), release_bin, records, signer, arguments.plugin_base_url, arguments.codesign_identity)
    create_archive(stage, arguments.archive.resolve(), authority()["sourceDateEpoch"])
    archive = arguments.archive.resolve()
    archive.with_name(archive.name + ".sha256").write_text(f"{sha256(archive)}  {archive.name}\n", encoding="ascii")
    print(f"{sha256(archive)}  {archive}")


if __name__ == "__main__":
    try:
        main()
    except ReleaseError as error:
        print(f"macos-release: {error}", file=sys.stderr)
        raise SystemExit(1)
