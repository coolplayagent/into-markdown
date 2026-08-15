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
    "onnxruntime-cpu",
    "pdfium",
    "ppocrv6-tiny-detector-onnx-model",
    "ppocrv6-tiny-recognizer-character-table",
    "ppocrv6-tiny-recognizer-onnx-model",
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
    sysroot = pathlib.Path(run(["rustc", "--print", "sysroot"])).resolve()
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
            "cargo", "build", "-j2", "--release", "--locked",
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


def assemble(
    output: pathlib.Path,
    cache: pathlib.Path,
    build_root: pathlib.Path,
    profile: str,
) -> pathlib.Path:
    if output.exists():
        raise ReleaseError("release output directory already exists")
    downloads = {"pdfium", "onnxruntime", "ocr-detector", "ocr-recognizer", "ocr-dictionary"}
    if profile == "full":
        downloads.add("libreoffice")
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
    components = CORE_COMPONENTS + (["libreoffice-macos-arm64"] if profile == "full" else [])
    projection_tool = release_bin / "release-projection"
    write_release_inputs(output, components, projection_tool)
    materials = write_license_materials(output, cache, profile)
    projection = projection_for(output, materials)
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


def write_license_materials(output: pathlib.Path, cache: pathlib.Path, profile: str) -> list[dict]:
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
    model_license = destination / "paddleocr-Apache-2.0.txt"
    copy_file(ROOT / "LICENSE", model_license, 0o644)
    result.append(material(model_license, output, "license-text", CORE_COMPONENTS[2:], ["Apache-2.0"], contents=True))
    for component, source, spdx in [
        ("imageproc-contour-adaptation", "third_party/licenses/imageproc-MIT.txt", "MIT"),
        ("clipper2-rust", "third_party/licenses/BSL-1.0.txt", "BSL-1.0"),
        ("calamine", "third_party/licenses/calamine-MIT.txt", "MIT"),
    ]:
        path = destination / f"{component}.txt"
        copy_file(ROOT / source, path, 0o644)
        result.append(material(path, output, "license-text", [component], [spdx], contents=True))
    sbom = json.loads((output / "sbom-input.json").read_text(encoding="utf-8"))
    npm = [component["id"] for component in sbom["components"] if component["id"].startswith("npm:")]
    if npm:
        path = destination / "npm-react-MIT.txt"
        copy_file(ROOT / "third_party/licenses/npm/react-MIT.txt", path, 0o644)
        result.append(material(path, output, "license-text", npm, ["MIT"], contents=True))
    for component in sbom["components"]:
        if not component["id"].startswith("cargo:"):
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


def projection_for(output: pathlib.Path, materials: list[dict]) -> dict:
    material_paths = {item["path"] for item in materials}
    sbom = json.loads((output / "sbom-input.json").read_text(encoding="utf-8"))
    selected = [component["id"] for component in sbom["components"]]
    embedded = [
        item for item in selected
        if item.startswith(("cargo:", "npm:"))
        or item in {"calamine", "clipper2-rust", "imageproc-contour-adaptation"}
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
    return {
        "schema_version": 1, "target": "aarch64-apple-darwin", "components": selected,
        "files": files, "license_materials": materials,
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
    if path.endswith("models/pp-ocrv6-tiny-detector-onnx/inference.onnx"):
        return "component", "ppocrv6-tiny-detector-onnx-model"
    if path.endswith("models/pp-ocrv6-tiny-recognizer-onnx/inference.onnx"):
        return "component", "ppocrv6-tiny-recognizer-onnx-model"
    if path.endswith("models/pp-ocrv6-tiny-recognizer-onnx/ppocrv6_tiny_dict.txt"):
        return "component", "ppocrv6-tiny-recognizer-character-table"
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
