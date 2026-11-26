"""Declare the locked OCR guest embedded in the Windows portable executable."""

from __future__ import annotations

import hashlib
import json
import pathlib
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[2]
INVENTORY = "third_party/licenses/ocr-wasm-inventory.json"
INVENTORY_SHA256 = "af9115b5451b6310e530f774850e725655c306317b07a0378ccc5710f53eabb3"
NOTICES = "third_party/licenses/ocr-wasm-NOTICES.txt"
GUEST = "tools/ocr-wasm"
SPDX_GUEST = "SPDXRef-Embedded-OCR-Wasm"


def digest(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def reviewed_inputs() -> tuple[dict, dict]:
    inventory_path = ROOT / INVENTORY
    if digest(inventory_path) != INVENTORY_SHA256:
        raise ValueError("Windows OCR dependency inventory differs from its reviewed bytes")
    inventory = json.loads(inventory_path.read_text(encoding="utf-8"))
    manifest = ROOT / GUEST / "Cargo.toml"
    lock = ROOT / GUEST / "Cargo.lock"
    for path, key in ((manifest, "manifest_sha256"), (lock, "lock_sha256"),
                      (ROOT / NOTICES, "notices_sha256")):
        if digest(path) != inventory[key]:
            raise ValueError(f"Windows OCR guest declaration differs from {path.name}")
    locked = {p["name"]: p for p in tomllib.loads(lock.read_text(encoding="utf-8"))["package"]}
    for p in inventory["packages"]:
        crate = locked[p["name"]]
        if (crate["version"], crate["checksum"]) != (p["version"], p["sha256"]):
            raise ValueError(f"unreviewed Windows OCR guest crate {p['name']}")
    return inventory, tomllib.loads(manifest.read_text(encoding="utf-8"))["package"]


def integrity(checksum: str, subject: str, target: str) -> dict:
    return {"algorithm": "SHA-256", "digest": checksum, "subject": subject, "target": target}


def relationship(source: str, kind: str, target: str) -> dict:
    return {"spdxElementId": source, "relationshipType": kind, "relatedSpdxElement": target}


def spdx_package(source: dict, spdx_id: str, declared: str, copyright_text: str) -> dict:
    return {
        "name": source["id"], "SPDXID": spdx_id, "versionInfo": source["version"],
        "downloadLocation": source["source"] if spdx_id != SPDX_GUEST else "NOASSERTION",
        "filesAnalyzed": False, "licenseDeclared": declared, "licenseConcluded": source["license"],
        "copyrightText": copyright_text, "sourceInfo": source["authority"],
        "checksums": [{"algorithm": "SHA256", "checksumValue": source["integrity"][0]["digest"]}],
    }


def add_guest_declarations(directory: pathlib.Path, guest: pathlib.Path) -> None:
    """Extend freshly generated portable Core declarations before archive hashing."""
    inventory, project = reviewed_inputs()
    sources_path = directory / "SOURCES.json"
    sbom_path = directory / "SBOM.spdx.json"
    sources = json.loads(sources_path.read_text(encoding="utf-8"))
    sbom = json.loads(sbom_path.read_text(encoding="utf-8"))
    if sources["target"] != "x86_64-pc-windows-msvc" or sources["artifact"] != "into-markdown-core":
        raise ValueError("the WebAssembly OCR guest belongs to Windows portable Core")
    if any(p["SPDXID"] == SPDX_GUEST for p in sbom["packages"]):
        raise ValueError("Windows OCR guest declarations already exist")
    source = f"https://github.com/coolplayagent/into-markdown/tree/{sources['source_revision']}/{GUEST}"
    guest_checksum = digest(guest)
    components = [{
        "id": project["name"], "version": project["version"], "kind": "embedded-wasm",
        "scope": "runtime", "distributed": True, "source": source, "license": project["license"],
        "authority": GUEST, "files": [], "integrity": [integrity(
            guest_checksum, "embedded WebAssembly module", inventory["target"]
        )],
    }]
    packages = [spdx_package(
        components[0], SPDX_GUEST, project["license"], "Copyright Into Markdown contributors",
    )]
    relationships = [relationship("SPDXRef-Package-Root", "CONTAINS", SPDX_GUEST)]
    for crate in inventory["packages"]:
        name, version = crate["name"], crate["version"]
        spdx_id = f"{SPDX_GUEST}-{name.replace('_', '-')}-{version}"
        runtime = crate["scope"] == "runtime"
        component = {
            "id": f"ocr-wasm:{name}@{version}", "version": version, "kind": "rust-crate",
            "source": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
            "license": crate["license"], "scope": crate["scope"], "distributed": runtime,
            "authority": f"{INVENTORY}#{name}", "files": [], "integrity": [integrity(
                crate["sha256"], f"crates.io archive {name}@{version}",
                inventory["target"] if runtime else sources["target"],
            )],
        }
        components.append(component)
        packages.append(spdx_package(component, spdx_id, crate["declared"], crate["copyright"]))
        relationships.append(
            relationship(SPDX_GUEST, "DEPENDS_ON", spdx_id) if runtime
            else relationship(spdx_id, "BUILD_DEPENDENCY_OF", SPDX_GUEST)
        )
    sources["components"].extend(components)
    sbom["packages"].extend(packages)
    sbom["relationships"].extend(relationships)
    for path, value in ((sources_path, sources), (sbom_path, sbom)):
        path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n",
                        encoding="utf-8", newline="\n")
    with (directory / "THIRD_PARTY_NOTICES.md").open("a", encoding="utf-8", newline="\n") as output:
        output.write("\n## Embedded Windows OCR guest\n\n")
        output.write((ROOT / NOTICES).read_text(encoding="utf-8"))
