"""macOS ARM64 release assembly, projection and deterministic archive entry point."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import shutil
import subprocess
import sys

from acquire import acquire, extract_tar
from archive import create as create_archive
from audit import audit as audit_macho
from common import AUTHORITY, ROOT, ReleaseError, authority, regular_files, run, sha256, write_json
from legacy_authority import generate as generate_legacy_authority
from rust_package import materialize as materialize_rust

CORE_COMPONENTS = [
    "ffmpeg",
    "onnxruntime-cpu",
    "pdfium",
    "ppocrv6-tiny-detector-onnx-model",
    "ppocrv6-tiny-recognizer-character-table",
    "ppocrv6-tiny-recognizer-onnx-model",
]
OCR_COMPONENTS = CORE_COMPONENTS[3:]
FULL_COMPONENTS = [
    "whisper-small",
    "silero-vad-half-onnx-model",
    "3dspeaker-eres2net-base-onnx-model",
    "libreoffice-macos-arm64",
]
FIXTURES = [
    "docx/normal.docx", "docx/corrupt.docx", "epub/normal.epub", "msg/normal.msg",
    "ocr/ocr-english-clear-1.png", "pdf/structures.pdf", "rtf/normal.rtf", "text/normal.txt",
    "xlsx/normal.xlsx", "xlsb/normal.xlsb", "pptx/normal.pptx",
    "odt/normal.odt", "ods/normal.ods", "odp/normal.odp",
]


def check_host() -> None:
    config = authority()
    if os.uname().sysname != "Darwin" or os.uname().machine != "arm64":
        raise ReleaseError("release assembly requires a native macOS ARM64 host")
    rust = run(["rustc", "--version"]).split()[1]
    if rust != config["rust"]:
        raise ReleaseError(f"rustc {rust} disagrees with fixed toolchain {config['rust']}")


def build(target: pathlib.Path) -> None:
    environment = os.environ.copy()
    cargo_home = pathlib.Path(environment.get("CARGO_HOME", pathlib.Path.home() / ".cargo")).resolve()
    rustup_home = pathlib.Path(environment.get("RUSTUP_HOME", pathlib.Path.home() / ".rustup")).resolve()
    sysroot = pathlib.Path(run(["rustc", "--print", "sysroot"]).strip()).resolve()
    remaps = [
        (ROOT, "/usr/src/into-markdown"),
        (target, "/usr/src/into-markdown-target"),
        (cargo_home, "/usr/src/cargo-home"),
        (rustup_home, "/usr/src/rustup-home"),
        (sysroot, "/usr/src/rust-sysroot"),
    ]
    environment.update(
        {
            "CARGO_INCREMENTAL": "0",
            "MACOSX_DEPLOYMENT_TARGET": authority()["minimumMacos"],
            "CFLAGS": " ".join(
                f"-ffile-prefix-map={source}={destination}" for source, destination in remaps
            ),
            "CXXFLAGS": " ".join(
                f"-ffile-prefix-map={source}={destination}" for source, destination in remaps
            ),
            "RUSTFLAGS": (
                " ".join(f"--remap-path-prefix={source}={destination}" for source, destination in remaps)
                + " -C strip=debuginfo "
                f"-C link-arg=-mmacosx-version-min={authority()['minimumMacos']}"
            ),
            "CARGO_TARGET_DIR": str(target),
        }
    )
    run(
        [
            "cargo", "build", "-j2", "--release", "--locked", "--features", "metal",
            "-p", "into-markdown-cli", "--bin", "into-md",
            "-p", "into-markdown-onnxruntime", "--bin", "onnxruntime-worker",
            "-p", "into-markdown-legacy-office", "--bin", "legacy-office-worker",
            "-p", "installed-smoke", "--bin", "installed-smoke",
            "-p", "installed-smoke", "--bin", "archive-check",
            "-p", "license-check", "--bin", "release-projection",
        ],
        cwd=ROOT,
        env=environment,
    )


def validate_ffmpeg_artifacts(root: pathlib.Path) -> dict:
    expected = {
        "COPYING.LGPLv2.1",
        "ffmpeg-aarch64-apple-darwin",
        "ffmpeg-authority-aarch64-apple-darwin.json",
        "ffmpeg-inventory-aarch64-apple-darwin.json",
        "ffmpeg-relink-aarch64-apple-darwin.tar",
    }
    if not root.is_dir() or root.is_symlink():
        raise ReleaseError("FFmpeg audit output is not a trusted directory")
    entries = list(root.iterdir())
    if {entry.name for entry in entries} != expected or any(
        not entry.is_file() or entry.is_symlink() for entry in entries
    ):
        raise ReleaseError("FFmpeg audit output does not contain the exact reviewed artifact set")
    authority_path = root / "ffmpeg-authority-aarch64-apple-darwin.json"
    try:
        ffmpeg_authority = json.loads(authority_path.read_text(encoding="utf-8"))
        inventory = json.loads(
            (root / "ffmpeg-inventory-aarch64-apple-darwin.json").read_text(encoding="utf-8")
        )
    except (OSError, json.JSONDecodeError) as error:
        raise ReleaseError(f"FFmpeg audit metadata is invalid: {error}") from error
    executable = root / "ffmpeg-aarch64-apple-darwin"
    relink = root / "ffmpeg-relink-aarch64-apple-darwin.tar"
    license_path = root / "COPYING.LGPLv2.1"
    if (
        ffmpeg_authority.get("schema_version") != 1
        or ffmpeg_authority.get("ffmpeg_version") != "8.1.2"
        or ffmpeg_authority.get("target") != "aarch64-apple-darwin"
        or ffmpeg_authority.get("executable_bytes") != executable.stat().st_size
        or ffmpeg_authority.get("executable_sha256") != sha256(executable)
        or ffmpeg_authority.get("relink_bytes") != relink.stat().st_size
        or ffmpeg_authority.get("relink_sha256") != sha256(relink)
        or inventory.get("schema_version") != 1
        or inventory.get("target") != "aarch64-apple-darwin"
        or set(inventory.get("distributed_files", []))
        != {
            "COPYING.LGPLv2.1",
            "ffmpeg-aarch64-apple-darwin",
            "ffmpeg-authority-aarch64-apple-darwin.json",
            "ffmpeg-relink-aarch64-apple-darwin.tar",
        }
        or inventory.get("license_sha256") != sha256(license_path)
    ):
        raise ReleaseError("FFmpeg audit metadata does not match its artifact bytes")
    return ffmpeg_authority


def assemble(
    output: pathlib.Path,
    cache: pathlib.Path,
    build_root: pathlib.Path,
    ffmpeg_artifacts: pathlib.Path,
    profile: str,
) -> pathlib.Path:
    if output.exists():
        raise ReleaseError("release output directory already exists")
    ffmpeg_authority = validate_ffmpeg_artifacts(ffmpeg_artifacts)
    downloads = {
        "ffmpeg-source", "pdfium", "onnxruntime", "ocr-detector", "ocr-recognizer",
        "ocr-dictionary",
    }
    if profile == "full":
        downloads.update({"libreoffice", "whisper-small", "silero-vad", "3dspeaker"})
    acquire(cache, downloads)
    output.mkdir(parents=True)
    binaries = output / "bin"
    binaries.mkdir()
    release_bin = build_root / "release"
    for name in ["into-md", "onnxruntime-worker", "installed-smoke", "archive-check"]:
        copy_file(release_bin / name, binaries / name, 0o755)
    for name in ["install", "uninstall"]:
        copy_file(pathlib.Path(__file__).with_name(name), output / name, 0o755)

    pdfium = output / "lib/pdfium"
    extract_tar(cache / "pdfium", pdfium, {"lib/libpdfium.dylib": "libpdfium.dylib"})
    onnx = binaries / "onnxruntime/lib"
    extract_tar(
        cache / "onnxruntime",
        onnx,
        {"./onnxruntime-osx-arm64-1.29.0/lib/libonnxruntime.dylib": "libonnxruntime.dylib"},
    )
    ffmpeg = binaries / "ffmpeg"
    copy_file(ffmpeg_artifacts / "ffmpeg-aarch64-apple-darwin", ffmpeg / "ffmpeg", 0o755)
    copy_file(
        ffmpeg_artifacts / "ffmpeg-authority-aarch64-apple-darwin.json",
        ffmpeg / "authority.json",
        0o644,
    )
    models = binaries / "models"
    extract_tar(
        cache / "ocr-detector",
        models / "pp-ocrv6-tiny-detector-onnx",
        {"PP-OCRv6_tiny_det_onnx_infer/inference.onnx": "inference.onnx"},
    )
    extract_tar(
        cache / "ocr-recognizer",
        models / "pp-ocrv6-tiny-recognizer-onnx",
        {"PP-OCRv6_tiny_rec_onnx_infer/inference.onnx": "inference.onnx"},
    )
    copy_file(
        cache / "ocr-dictionary",
        models / "pp-ocrv6-tiny-recognizer-onnx/ppocrv6_tiny_dict.txt",
        0o644,
    )
    for bundle in ["pp-ocrv6-tiny-detector-onnx", "pp-ocrv6-tiny-recognizer-onnx"]:
        write_json(
            models / bundle / "install-state.json",
            {"schemaVersion": 1, "bundleId": bundle, "complete": True},
        )
    if profile == "full":
        copy_file(
            cache / "whisper-small",
            models / "whisper-small-multilingual/ggml-small.bin",
            0o644,
        )
        copy_file(
            cache / "silero-vad",
            models / "silero-vad-3dspeaker-eres2net/silero_vad_half.onnx",
            0o644,
        )
        copy_file(
            cache / "3dspeaker",
            models / "silero-vad-3dspeaker-eres2net/3dspeaker_eres2net_base.onnx",
            0o644,
        )
        for bundle in ["whisper-small-multilingual", "silero-vad-3dspeaker-eres2net"]:
            write_json(
                models / bundle / "install-state.json",
                {"schemaVersion": 1, "bundleId": bundle, "complete": True},
            )

    legacy = binaries / "legacy-office-runtime"
    legacy.mkdir()
    copy_file(release_bin / "legacy-office-worker", legacy / "legacy-office-worker", 0o755)
    if profile == "full":
        copy_file(cache / "libreoffice", legacy / "LibreOffice_26.2.5_MacOS_aarch64.dmg", 0o444)
        lo_artifact = next(item for item in authority()["downloads"] if item["id"] == "libreoffice")
        generate_legacy_authority(
            legacy,
            release_bin / "legacy-office-worker",
            legacy / "LibreOffice_26.2.5_MacOS_aarch64.dmg",
            lo_artifact,
        )

    fixture_root = output / "share/into-markdown/smoke/fixtures"
    for relative in FIXTURES:
        copy_file(ROOT / "fixtures/small" / relative, fixture_root / relative, 0o644)
    for name in ["normal.doc", "normal.ppt", "normal.xls"]:
        copy_file(pathlib.Path(__file__).with_name("fixtures") / name, fixture_root / "legacy" / name, 0o644)
    materialize_rust(output / "lib/into-markdown-rust")
    copy_file(ROOT / "LICENSE", output / "LICENSE", 0o644)
    components = CORE_COMPONENTS + (FULL_COMPONENTS if profile == "full" else [])
    projection_tool = release_bin / "release-projection"
    write_release_inputs(output, components, projection_tool)
    materials = write_license_materials(output, cache, ffmpeg_artifacts, profile)
    projection = projection_for(output, materials, ffmpeg_authority)
    write_json(output / "archive-manifest.json", projection)
    verify_projection(output / "archive-manifest.json", projection_tool)
    audit_macho(output, profile, authority()["minimumMacos"])
    return output


def write_release_inputs(
    output: pathlib.Path,
    components: list[str],
    projection_tool: pathlib.Path,
) -> None:
    request = output.parent / "release-request.json"
    write_json(request, {"schema_version": 1, "target": "aarch64-apple-darwin", "components": components})
    text = run(
        [str(projection_tool), "generate", str(request)],
        cwd=ROOT,
    )
    inputs = json.loads(text)
    for key in ["notice", "third_party_notices", "sbom_input", "core_catalog"]:
        item = inputs[key]
        (output / item["path"]).write_text(item["contents"], encoding="utf-8")


def write_license_materials(
    output: pathlib.Path,
    cache: pathlib.Path,
    ffmpeg_artifacts: pathlib.Path,
    profile: str,
) -> list[dict]:
    destination = output / "share/into-markdown/licenses"
    destination.mkdir(parents=True)
    result = []
    native = [
        ("pdfium", "pdfium", "pdfium-mac-arm64.tgz"),
        ("onnxruntime-cpu", "onnxruntime", "onnxruntime-osx-arm64-1.29.0.tgz"),
    ]
    if profile == "full":
        native.append(
            ("libreoffice-macos-arm64", "libreoffice", "LibreOffice_26.2.5_MacOS_aarch64.dmg")
        )
    for component, cached, name in native:
        path = destination / name
        copy_file(cache / cached, path, 0o644)
        result.append(material(path, output, "notice-bundle", [component], []))
    ffmpeg_source = output / "share/into-markdown/source/ffmpeg-8.1.2.tar.xz"
    copy_file(cache / "ffmpeg-source", ffmpeg_source, 0o644)
    result.append(material(ffmpeg_source, output, "corresponding-source", ["ffmpeg"], []))
    ffmpeg_relink = output / "share/into-markdown/relink/ffmpeg-relink-aarch64-apple-darwin.tar"
    copy_file(
        ffmpeg_artifacts / "ffmpeg-relink-aarch64-apple-darwin.tar",
        ffmpeg_relink,
        0o644,
    )
    result.append(material(ffmpeg_relink, output, "relink-material", ["ffmpeg"], []))
    ffmpeg_license = destination / "ffmpeg-LGPL-2.1.txt"
    copy_file(ffmpeg_artifacts / "COPYING.LGPLv2.1", ffmpeg_license, 0o644)
    result.append(
        material(
            ffmpeg_license,
            output,
            "license-text",
            ["ffmpeg"],
            ["LGPL-2.1-or-later"],
            contents=True,
        )
    )
    model_license = destination / "paddleocr-Apache-2.0.txt"
    copy_file(ROOT / "LICENSE", model_license, 0o644)
    result.append(
        material(
            model_license,
            output,
            "license-text",
            OCR_COMPONENTS,
            ["Apache-2.0"],
            contents=True,
        )
    )
    if profile == "full":
        for component, source, name, spdx in [
            (
                "whisper-small",
                "third_party/licenses/whisper-model-MIT.txt",
                "whisper-model-MIT.txt",
                "MIT",
            ),
            (
                "silero-vad-half-onnx-model",
                "third_party/licenses/silero-vad-MIT.txt",
                "silero-vad-MIT.txt",
                "MIT",
            ),
            (
                "3dspeaker-eres2net-base-onnx-model",
                "LICENSE",
                "3dspeaker-Apache-2.0.txt",
                "Apache-2.0",
            ),
        ]:
            path = destination / name
            copy_file(ROOT / source, path, 0o644)
            result.append(
                material(path, output, "license-text", [component], [spdx], contents=True)
            )
    for component, source, spdx in [
        ("opencc-transcript-character-table", "LICENSE", "Apache-2.0"),
        ("imageproc-contour-adaptation", "third_party/licenses/imageproc-MIT.txt", "MIT"),
        ("clipper2-rust", "third_party/licenses/BSL-1.0.txt", "BSL-1.0"),
        ("calamine", "third_party/licenses/calamine-MIT.txt", "MIT"),
    ]:
        path = destination / f"{component}.txt"
        copy_file(ROOT / source, path, 0o644)
        result.append(material(path, output, "license-text", [component], [spdx], contents=True))
    sbom = json.loads((output / "sbom-input.json").read_text(encoding="utf-8"))
    npm = [
        component["id"]
        for component in sbom["components"]
        if component["id"].startswith("npm:")
        and component["id"] != "npm:lucide-react@1.31.0"
    ]
    lucide = [
        component["id"]
        for component in sbom["components"]
        if component["id"] == "npm:lucide-react@1.31.0"
    ]
    if npm:
        path = destination / "npm-react-MIT.txt"
        copy_file(ROOT / "third_party/licenses/npm/react-MIT.txt", path, 0o644)
        result.append(material(path, output, "license-text", npm, ["MIT"], contents=True))
    if lucide:
        path = destination / "npm-lucide-ISC-MIT.txt"
        copy_file(ROOT / "third_party/licenses/npm/lucide-ISC-MIT.txt", path, 0o644)
        result.append(
            material(path, output, "license-text", lucide, ["ISC", "MIT"], contents=True)
        )
    for component in sbom["components"]:
        if not component["id"].startswith("cargo:"):
            continue
        if component["id"] == "cargo:whisper-rs@0.16.0":
            path = destination / "whisper-rs-Unlicense.txt"
            copy_file(ROOT / "third_party/whisper-rs-0.16.0/LICENSE", path, 0o644)
            result.append(
                material(
                    path,
                    output,
                    "license-text",
                    [component["id"]],
                    ["Unlicense"],
                    contents=True,
                )
            )
            continue
        checksum = next(
            evidence["digest"] for evidence in component["integrity"]
            if evidence["subject"].startswith("crates.io archive")
        )
        name_version = component["id"].removeprefix("cargo:").replace("@", "-") + ".crate"
        candidates = list(pathlib.Path.home().glob(f".cargo/registry/cache/*/{name_version}"))
        if len(candidates) != 1 or sha256(candidates[0]) != checksum:
            raise ReleaseError(f"fixed Cargo source archive is unavailable: {component['id']}")
        path = destination / "cargo" / name_version
        copy_file(candidates[0], path, 0o644)
        result.append(material(path, output, "upstream-source-archive", [component["id"]], []))
    return result


def material(path: pathlib.Path, root: pathlib.Path, kind: str, components: list[str], spdx: list[str], contents: bool = False) -> dict:
    value = {
        "path": path.relative_to(root).as_posix(), "bytes": path.stat().st_size,
        "sha256": sha256(path), "kind": kind, "component_ids": components,
    }
    if spdx:
        value["spdx_expressions"] = spdx
    if contents:
        value["contents"] = path.read_text(encoding="utf-8")
    return value


def projection_for(output: pathlib.Path, materials: list[dict], ffmpeg_authority: dict) -> dict:
    material_paths = {item["path"] for item in materials}
    sbom = json.loads((output / "sbom-input.json").read_text(encoding="utf-8"))
    selected = [component["id"] for component in sbom["components"]]
    embedded = [
        item for item in selected
        if item.startswith(("cargo:", "npm:"))
        or item in {
            "calamine",
            "clipper2-rust",
            "imageproc-contour-adaptation",
            "opencc-transcript-character-table",
        }
    ]
    files = []
    for path in regular_files(output):
        relative = path.relative_to(output).as_posix()
        if relative == "archive-manifest.json":
            continue
        kind, owner = classify(relative, material_paths)
        entry = {"path": relative, "bytes": path.stat().st_size, "sha256": sha256(path), "kind": kind}
        if owner:
            entry["component_id"] = owner
        if relative == "bin/into-md":
            entry["embedded_components"] = embedded
        files.append(entry)
    authority_path = "bin/ffmpeg/authority.json"
    authority_contents = (output / authority_path).read_text(encoding="utf-8")
    evidence = {
        "authority_path": authority_path,
        "authority_bytes": len(authority_contents.encode("utf-8")),
        "authority_sha256": sha256(output / authority_path),
        "authority_contents": authority_contents,
        **ffmpeg_authority,
        "executable_path": "bin/ffmpeg/ffmpeg",
    }
    return {
        "schema_version": 1, "target": "aarch64-apple-darwin", "components": selected,
        "files": files, "license_materials": materials, "ffmpeg_evidence": evidence,
    }


def classify(path: str, material_paths: set[str]) -> tuple[str, str | None]:
    if path in material_paths:
        return "license-material", None
    if path in {"LICENSE", "NOTICE"}:
        return "declaration", None
    if path in {"THIRD_PARTY_NOTICES.md", "sbom-input.json", "core-catalog.json"}:
        return "generated", None
    if path == "lib/pdfium/libpdfium.dylib":
        return "component", "pdfium"
    if path == "bin/onnxruntime/lib/libonnxruntime.dylib":
        return "component", "onnxruntime-cpu"
    if path in {"bin/ffmpeg/ffmpeg", "bin/ffmpeg/authority.json"}:
        return "component", "ffmpeg"
    if path.endswith("models/pp-ocrv6-tiny-detector-onnx/inference.onnx"):
        return "component", "ppocrv6-tiny-detector-onnx-model"
    if path.endswith("models/pp-ocrv6-tiny-recognizer-onnx/inference.onnx"):
        return "component", "ppocrv6-tiny-recognizer-onnx-model"
    if path.endswith("models/pp-ocrv6-tiny-recognizer-onnx/ppocrv6_tiny_dict.txt"):
        return "component", "ppocrv6-tiny-recognizer-character-table"
    if path.endswith("models/whisper-small-multilingual/ggml-small.bin"):
        return "component", "whisper-small"
    if path.endswith("models/silero-vad-3dspeaker-eres2net/silero_vad_half.onnx"):
        return "component", "silero-vad-half-onnx-model"
    if path.endswith("models/silero-vad-3dspeaker-eres2net/3dspeaker_eres2net_base.onnx"):
        return "component", "3dspeaker-eres2net-base-onnx-model"
    if path.endswith("/install-state.json") and path.startswith("bin/models/"):
        return "project", None
    if path.startswith("bin/legacy-office-runtime/") and not path.endswith("legacy-office-worker"):
        return "component", "libreoffice-macos-arm64"
    return "project", None


def verify_projection(manifest: pathlib.Path, projection_tool: pathlib.Path) -> None:
    run(
        [str(projection_tool), "verify", str(manifest)],
        cwd=ROOT,
    )


def copy_file(source: pathlib.Path, destination: pathlib.Path, mode: int) -> None:
    if not source.is_file() or source.is_symlink():
        raise ReleaseError(f"release input is not a regular file: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(mode)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--cache", required=True, type=pathlib.Path)
    parser.add_argument("--build-root", required=True, type=pathlib.Path)
    parser.add_argument("--ffmpeg-artifacts", required=True, type=pathlib.Path)
    parser.add_argument("--archive", required=True, type=pathlib.Path)
    parser.add_argument("--profile", required=True, choices=["core", "full"])
    parser.add_argument("--skip-build", action="store_true")
    arguments = parser.parse_args()
    check_host()
    if not arguments.skip_build:
        build(arguments.build_root.resolve())
    stage = assemble(
        arguments.output.resolve(),
        arguments.cache.resolve(),
        arguments.build_root.resolve(),
        arguments.ffmpeg_artifacts.resolve(),
        arguments.profile,
    )
    create_archive(stage, arguments.archive.resolve(), authority()["sourceDateEpoch"])
    archive = arguments.archive.resolve()
    digest = sha256(archive)
    checksum = archive.with_name(archive.name + ".sha256")
    checksum.write_text(f"{digest}  {archive.name}\n", encoding="ascii")
    print(f"{digest}  {archive}")


if __name__ == "__main__":
    try:
        main()
    except ReleaseError as error:
        print(f"macos-release: {error}", file=sys.stderr)
        raise SystemExit(1)
