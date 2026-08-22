"""Regenerate the reviewed Cargo normal-runtime dependency partition."""

from __future__ import annotations

import hashlib
import json
import pathlib
import subprocess
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[2]
OUTPUT = ROOT / "third_party/licenses/cargo-normal-runtime.json"


def run(*arguments: str) -> str:
    result = subprocess.run(arguments, cwd=ROOT, check=True, capture_output=True, text=True)
    return result.stdout


def digest(path: pathlib.Path) -> str:
    text = path.read_text(encoding="utf-8").replace("\r\n", "\n")
    if "\r" in text:
        raise ValueError(f"isolated carriage return in {path}")
    return hashlib.sha256(text.encode()).hexdigest()


def main() -> None:
    metadata = json.loads(run("cargo", "metadata", "--locked", "--offline", "--format-version", "1"))
    packages = {(item["name"], item["version"]): item for item in metadata["packages"]}
    tree = run(
        "cargo", "tree", "--locked", "--offline", "-p", "into-markdown-cli",
        "-e", "normal", "--prefix", "none", "--format", "{p}", "--target", "all",
    )
    normal: set[str] = set()
    local: set[str] = set()
    for raw in tree.splitlines():
        line = raw.removesuffix(" (*)")
        name, version, *_ = line.split()
        version = version.removeprefix("v")
        package = packages[(name, version)]
        if package.get("source") == "registry+https://github.com/rust-lang/crates.io-index":
            normal.add(f"{name}@{version}")
        elif package.get("source") is None:
            manifest = pathlib.Path(package["manifest_path"]).resolve().relative_to(ROOT)
            if manifest.parts[0] == "third_party":
                local.add(f"{name}@{version}")
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    registry = {
        f"{item['name']}@{item['version']}"
        for item in lock["package"]
        if item.get("source") == "registry+https://github.com/rust-lang/crates.io-index"
    }
    members = set(metadata["workspace_members"])
    manifests = {"Cargo.toml"}
    for package in metadata["packages"]:
        if package["id"] in members:
            manifests.add(pathlib.Path(package["manifest_path"]).resolve().relative_to(ROOT).as_posix())
    authority = {
        "schema_version": 1,
        "root": "into-markdown-cli",
        "cargo_lock_sha256": digest(ROOT / "Cargo.lock"),
        "workspace_manifest_sha256": {path: digest(ROOT / path) for path in sorted(manifests)},
        "local_runtime_packages": sorted(local),
        "normal_registry_packages": sorted(normal),
        "non_normal_registry_packages": sorted(registry - normal),
    }
    OUTPUT.write_text(json.dumps(authority, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
