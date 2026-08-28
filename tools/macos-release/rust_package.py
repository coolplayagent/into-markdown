"""Materialize the exact offline Rust facade consumed by installed smoke."""

from __future__ import annotations

import json
import pathlib
import shutil
import stat
import tempfile
import zipfile

from common import ROOT, ReleaseError, run


def materialize(destination: pathlib.Path) -> None:
    """Write the complete offline Rust facade as one reproducible ZIP file."""
    if destination.suffix != ".zip":
        raise ReleaseError("Rust facade destination must be a .zip file")
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        raise ReleaseError("Rust facade archive already exists")
    with tempfile.TemporaryDirectory(prefix="into-md-rust-facade-", dir=destination.parent) as name:
        source = pathlib.Path(name) / "facade"
        _materialize_tree(source)
        _write_reproducible_zip(source, destination)


def _materialize_tree(destination: pathlib.Path) -> None:
    metadata = json.loads(run(["cargo", "metadata", "--locked", "--format-version", "1"], cwd=ROOT))
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    roots = [package["id"] for package in packages.values() if package["name"] == "into-markdown"]
    if len(roots) != 1:
        raise ReleaseError("Rust facade package identity is not unique")
    version = packages[roots[0]]["version"]
    reachable: set[str] = set()
    pending = roots[:]
    while pending:
        package_id = pending.pop()
        if package_id in reachable:
            continue
        reachable.add(package_id)
        pending.extend(nodes[package_id]["dependencies"])
    local = [packages[package_id] for package_id in reachable if packages[package_id]["source"] is None]
    destination.mkdir(parents=True, exist_ok=False)
    api = ROOT / "crates/api"
    shutil.copytree(api / "src", destination / "src")
    for package in sorted(local, key=lambda item: item["name"]):
        source = pathlib.Path(package["manifest_path"]).parent
        if source == api:
            continue
        relative = local_package_path(source)
        shutil.copytree(source, destination / relative, ignore=shutil.ignore_patterns("target"))
    for relative in [
        "models/manifest.json",
        "models/ppocrv6-tiny-detector-authority.json",
        "models/ppocrv6-tiny-detector-onnx-authority.json",
        "models/ppocrv6-tiny-recognizer-authority.json",
        "third_party/ffmpeg/build-policy.json",
        "third_party/licenses/downloads.json",
        "third_party/onnxruntime/manifest.json",
        "third_party/pdfium/manifest.json",
    ]:
        source = ROOT / relative
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
    facade = (api / "Cargo.toml").read_text(encoding="utf-8")
    facade = facade.replace("rust-version.workspace = true", 'rust-version = "1.97.1"')
    facade = facade.replace("version.workspace = true", f'version = "{version}"')
    facade = facade.replace("edition.workspace = true", 'edition = "2024"')
    facade = facade.replace("publish.workspace = true", "publish = false")
    facade = facade.replace("license.workspace = true", 'license = "Apache-2.0"')
    facade = facade.replace('{ path = "../', '{ path = "crates/')
    workspace = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    dependencies = workspace.split("[workspace.dependencies]\n", 1)[1].split("\n[workspace.lints.rust]", 1)[0]
    members = sorted(
        f'    "{local_package_path(pathlib.Path(package["manifest_path"]).parent).as_posix()}",'
        for package in local
        if pathlib.Path(package["manifest_path"]).parent != api
    )
    combined = (
        facade
        + "\n[workspace]\nresolver = \"3\"\nmembers = [\n"
        + "\n".join(members)
        + f"\n]\n\n[workspace.package]\nversion = \"{version}\"\nedition = \"2024\"\n"
        + "rust-version = \"1.97.1\"\npublish = false\nlicense = \"Apache-2.0\"\n\n"
        + "[workspace.dependencies]\n"
        + dependencies
        + "\n[workspace.lints.rust]\nunsafe_code = \"forbid\"\nmissing_docs = \"warn\"\n\n"
        + "[workspace.lints.clippy]\nall = \"warn\"\npedantic = \"warn\"\n"
    )
    (destination / "Cargo.toml").write_text(combined, encoding="utf-8")
    shutil.copy2(ROOT / "Cargo.lock", destination / "Cargo.lock")
    run(["cargo", "generate-lockfile", "--offline"], cwd=destination)
    run(
        ["cargo", "vendor", "--locked", "--versioned-dirs", str(destination / "vendor")],
        cwd=destination,
    )
    run(["cargo", "metadata", "--locked", "--offline", "--format-version", "1"], cwd=destination)


def _write_reproducible_zip(source: pathlib.Path, destination: pathlib.Path) -> None:
    # Store entries rather than deflating them here. The outer distribution archive
    # performs compression, while ZIP_STORED avoids zlib-version-dependent bytes on
    # Windows, Linux, and macOS.
    with zipfile.ZipFile(destination, "x", compression=zipfile.ZIP_STORED) as archive:
        for path in sorted(source.rglob("*"), key=lambda item: item.relative_to(source).as_posix()):
            metadata = path.lstat()
            if path.is_symlink() or not path.is_file():
                if path.is_dir():
                    continue
                raise ReleaseError(f"Rust facade contains a non-regular entry: {path}")
            relative = path.relative_to(source).as_posix()
            entry = zipfile.ZipInfo(relative, date_time=(1980, 1, 1, 0, 0, 0))
            entry.create_system = 3
            entry.compress_type = zipfile.ZIP_STORED
            entry.external_attr = (stat.S_IFREG | 0o644) << 16
            entry.flag_bits |= 0x800
            archive.writestr(entry, path.read_bytes())


def local_package_path(source: pathlib.Path) -> pathlib.Path:
    if source.parent == ROOT / "crates":
        return pathlib.Path("crates") / source.name
    if source == ROOT / "third_party/whisper-rs-0.16.0":
        return pathlib.Path("third_party/whisper-rs-0.16.0")
    raise ReleaseError(f"Rust package escapes the reviewed project roots: {source}")
