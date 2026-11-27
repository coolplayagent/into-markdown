"""Verify the embedded Windows guest's portable release declarations."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import pathlib
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("ocr_guest", pathlib.Path(__file__).with_name("ocr_guest.py"))
assert SPEC and SPEC.loader
ocr_guest = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ocr_guest)


class GuestDeclarations(unittest.TestCase):
    def test_reviewed_sources_accept_lf_and_crlf_and_reject_content_changes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            inputs = [ocr_guest.INVENTORY, ocr_guest.NOTICES,
                      f"{ocr_guest.GUEST}/Cargo.toml", f"{ocr_guest.GUEST}/Cargo.lock"]
            originals = {name: (ocr_guest.ROOT / name).read_bytes().replace(b"\r\n", b"\n")
                         for name in inputs}
            with mock.patch.object(ocr_guest, "ROOT", root):
                for newline in (b"\n", b"\r\n"):
                    for name, value in originals.items():
                        path = root / name
                        path.parent.mkdir(parents=True, exist_ok=True)
                        path.write_bytes(value.replace(b"\n", newline))
                    with self.subTest(newline=newline):
                        self.assertEqual(ocr_guest.reviewed_inputs()[1]["name"], "into-md-ocr-wasm")
                for name, value in originals.items():
                    path = root / name
                    for invalid in (value + b" ", value.replace(b"\n", b"\r", 1), value + b"\xff"):
                        path.write_bytes(invalid)
                        with self.subTest(name=name), self.assertRaises(ValueError):
                            ocr_guest.reviewed_inputs()
                    path.write_bytes(value)

    def test_windows_guest_joins_core_declarations(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            guest = directory / "guest.wasm"
            guest.write_bytes(b"\0asm\x01\0\0\0\x00\x05\x01x\r\n\xff")
            (directory / "THIRD_PARTY_NOTICES.md").write_text("Core notices\n", encoding="utf8")
            (directory / "SBOM.spdx.json").write_text(json.dumps({
                "packages": [{"SPDXID": "SPDXRef-Package-Root"}], "relationships": []
            }), encoding="utf8")
            sources = {"target": "x86_64-pc-windows-msvc", "artifact": "into-markdown-core",
                       "source_revision": "0" * 40, "components": []}
            (directory / "SOURCES.json").write_text(json.dumps(sources), encoding="utf8")
            ocr_guest.add_guest_declarations(directory, guest)
            sources = json.loads((directory / "SOURCES.json").read_text(encoding="utf8"))
            components = {p["id"]: p for p in sources["components"]}
            self.assertTrue(components["ocr-wasm:rten@0.28.0"]["distributed"])
            self.assertFalse(components["ocr-wasm:wasm-bindgen-macro@0.2.130"]["distributed"])
            self.assertEqual(components["ocr-wasm:unicode-ident@1.0.27"]["license"], "MIT AND Unicode-3.0")
            image = components["into-md-ocr-wasm"]["integrity"][0]
            self.assertEqual(image["digest"], hashlib.sha256(guest.read_bytes()).hexdigest())
            notices = (directory / "THIRD_PARTY_NOTICES.md").read_text(encoding="utf8")
            self.assertIn("Copyright (c) 2026 RTen project contributors.", notices)
            self.assertIn("Copyright © 1991-2023 Unicode, Inc.", notices)
            sbom = json.loads((directory / "SBOM.spdx.json").read_text(encoding="utf8"))
            guest_id = "SPDXRef-Embedded-OCR-Wasm"
            root_id = "SPDXRef-Package-Root"
            packages = {p["name"]: p["SPDXID"] for p in sbom["packages"] if "name" in p}
            self.assertEqual(set(packages), set(components))
            identifiers = [p["SPDXID"] for p in sbom["packages"]]
            self.assertEqual(len(set(identifiers)), len(identifiers))
            for identifier in identifiers:
                self.assertRegex(identifier, r"^SPDXRef-[A-Za-z0-9.-]+$")
            expected = {(root_id, "CONTAINS", guest_id)}
            for name, component in components.items():
                if component["kind"] != "rust-crate":
                    continue
                if component["scope"] == "runtime":
                    expected.add((guest_id, "DEPENDS_ON", packages[name]))
                else:
                    expected.add((packages[name], "BUILD_DEPENDENCY_OF", guest_id))
            actual = [(r["spdxElementId"], r["relationshipType"], r["relatedSpdxElement"])
                      for r in sbom["relationships"]]
            self.assertCountEqual(actual, expected)
