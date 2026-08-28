#!/usr/bin/env python3

from __future__ import annotations

import pathlib
import hashlib
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
import zipfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from audit import (
    Audit,
    AuditFailure,
    clr_il_only,
    distributed_source_fixture,
    run,
    safe_zip_extract,
)
from platform_acceptance import (
    capability_map,
    repairable_payload_files,
    resolve_package,
    tree_hash,
)

WINDOW_FLAGS = subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0


class PlatformToolTests(unittest.TestCase):
    def test_acceptance_resolves_internal_or_public_plugin_name_without_ambiguity(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            internal = root / "official.ocr.ppocrv6.imp"
            public = root / "official.ocr.ppocrv6-x86_64-pc-windows-msvc.imp"
            internal.write_bytes(b"internal")
            self.assertEqual(
                resolve_package(root, "official.ocr.ppocrv6", "x86_64-pc-windows-msvc"),
                internal.resolve(),
            )
            internal.unlink()
            public.write_bytes(b"public")
            self.assertEqual(
                resolve_package(root, "official.ocr.ppocrv6", "x86_64-pc-windows-msvc"),
                public.resolve(),
            )
            internal.write_bytes(b"ambiguous")
            with self.assertRaisesRegex(RuntimeError, "exactly one"):
                resolve_package(root, "official.ocr.ppocrv6", "x86_64-pc-windows-msvc")
            with self.assertRaisesRegex(RuntimeError, "bounded"):
                resolve_package(root, "official.ocr.ppocrv6", "../windows")

    def test_platform_audit_rejects_unknown_signing_mode_before_io(self) -> None:
        with self.assertRaisesRegex(ValueError, "unsupported Windows signing mode"):
            run(
                "x86_64-pc-windows-msvc",
                pathlib.Path("missing-core"),
                pathlib.Path("missing-plugins"),
                windows_signing_mode="surprise",
            )

    def test_repair_fixture_never_corrupts_plugin_manager_authority(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            for authority in ["plugin.json", ".installed.json", ".package.zip"]:
                (root / authority).write_bytes(b"authority")
            payload = root / "bin" / "provider.exe"
            payload.parent.mkdir()
            payload.write_bytes(b"payload")
            self.assertEqual(repairable_payload_files([root]), [payload])

    def test_acceptance_normalizes_only_windows_verbatim_path_prefixes(self) -> None:
        from platform_acceptance import legacy_windows_path

        self.assertEqual(legacy_windows_path(r"\\?\C:\隔离\config.toml"), r"C:\隔离\config.toml")
        self.assertEqual(
            legacy_windows_path(r"\\?\UNC\server\share\config.toml"),
            r"\\server\share\config.toml",
        )
        self.assertEqual(legacy_windows_path(r"C:\plain\config.toml"), r"C:\plain\config.toml")

    def test_clr_il_only_requires_no_managed_native_image(self) -> None:
        header = """
               9 flags
                   IL Only
               0 [       0] RVA [size] of ManagedNativeHeader Directory
"""
        self.assertTrue(clr_il_only(header))
        self.assertFalse(clr_il_only(header.replace("0 [       0]", "2000 [      80]")))
        self.assertFalse(clr_il_only("native PE"))

    def test_only_rust_vendor_native_samples_are_source_fixtures(self) -> None:
        self.assertTrue(
            distributed_source_fixture(
                pathlib.Path("lib/into-markdown-rust/vendor/example/tests/fixture.dll")
            )
        )
        self.assertFalse(distributed_source_fixture(pathlib.Path("bin/into-md.exe")))
        self.assertFalse(
            distributed_source_fixture(pathlib.Path("lib/pdfium/pdfium.dll"))
        )

    def test_zip_extractor_rejects_parent_traversal(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            package = root / "bad.imp"
            with zipfile.ZipFile(package, "w") as archive:
                archive.writestr("../escape", b"bad")
            with self.assertRaises(AuditFailure):
                safe_zip_extract(package, root / "output", Audit("fixture"))
            self.assertFalse((root / "escape").exists())

    def test_zip_extractor_rejects_symbolic_link_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            package = root / "bad.imp"
            entry = zipfile.ZipInfo("link")
            entry.external_attr = (stat.S_IFLNK | 0o777) << 16
            with zipfile.ZipFile(package, "w") as archive:
                archive.writestr(entry, "target")
            with self.assertRaises(AuditFailure):
                safe_zip_extract(package, root / "output", Audit("fixture"))

    def test_tree_hash_is_path_and_content_bound(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            (root / "a").write_bytes(b"one")
            first = tree_hash(root)
            (root / "a").write_bytes(b"two")
            self.assertNotEqual(first, tree_hash(root))

    def test_capability_json_contract_is_indexed_by_id(self) -> None:
        value = capability_map('{"capabilities":[{"id":"ocr","status":"ready"}]}')
        self.assertEqual(value["ocr"]["status"], "ready")

    @unittest.skipUnless(sys.platform == "win32" and shutil.which("rustc") and shutil.which("pwsh"), "native Windows transaction")
    def test_windows_native_install_is_idempotent_and_rolls_back_bad_upgrade(self) -> None:
        if os.environ.get("TEST_SRCDIR"):
            self.skipTest("PowerShell cannot start a child console binary through Bazel's closed test pipe")
        source = pathlib.Path(__file__).resolve().parent
        with tempfile.TemporaryDirectory(prefix="into-md-安装-") as name:
            root = pathlib.Path(name)
            installer = root / "into-md-installer.exe"
            fixture = root / "fixture.exe"
            subprocess.run(["rustc", "--edition=2024", "-Dwarnings", str(source / "installer.rs"), "-o", str(installer)], check=True, creationflags=WINDOW_FLAGS)
            subprocess.run(["rustc", "--edition=2024", "-Dwarnings", str(source / "installer_test_program.rs"), "-o", str(fixture)], check=True, creationflags=WINDOW_FLAGS)

            def distribution(label: str, rejected: bool = False) -> tuple[pathlib.Path, str]:
                value = root / label
                (value / "bin").mkdir(parents=True)
                shutil.copy2(installer, value / "bin" / "into-md-installer.exe")
                shutil.copy2(fixture, value / "bin" / "archive-check.exe")
                shutil.copy2(fixture, value / "bin" / "into-md.exe")
                shutil.copy2(source / "Install.ps1", value / "Install.ps1")
                shutil.copy2(source / "Uninstall.ps1", value / "Uninstall.ps1")
                manifest = (f"manifest-{label}\n").encode()
                (value / "archive-manifest.json").write_bytes(manifest)
                if rejected:
                    (value / "reject-install").write_bytes(b"reject")
                return value, hashlib.sha256(manifest).hexdigest()

            first, first_hash = distribution("distribution-one")
            subprocess.run([str(first / "bin" / "archive-check.exe"), str(first)], check=True, creationflags=WINDOW_FLAGS)
            prefix = root / "用户" / "install"
            commands = root / "用户" / "commands"
            install_command = [shutil.which("pwsh"), "-NoProfile", "-File", str(first / "Install.ps1"), "-Prefix", str(prefix), "-CommandDirectory", str(commands)]
            first_install = subprocess.run(install_command, text=True, encoding="utf-8", errors="replace", stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, creationflags=WINDOW_FLAGS)
            self.assertEqual(first_install.returncode, 0, first_install.stderr)
            installed = first_install.stdout.strip().splitlines()[-1]
            second_install = subprocess.run(install_command, text=True, encoding="utf-8", errors="replace", stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, creationflags=WINDOW_FLAGS)
            self.assertEqual(second_install.returncode, 0, second_install.stderr)
            repeated = second_install.stdout.strip().splitlines()[-1]
            self.assertEqual(installed, repeated)
            launched = subprocess.run([str(commands / "into-md.exe"), "hello", "世界"], check=True, text=True, encoding="utf-8", stdout=subprocess.PIPE, creationflags=WINDOW_FLAGS).stdout
            self.assertIn("fixture:hello|世界", launched)

            second, second_hash = distribution("distribution-two", rejected=True)
            failure = subprocess.run([str(installer), "install", str(second), str(prefix), str(commands), second_hash], text=True, encoding="utf-8", stdout=subprocess.PIPE, stderr=subprocess.PIPE, creationflags=WINDOW_FLAGS)
            self.assertNotEqual(failure.returncode, 0)
            self.assertEqual((prefix / "current.txt").read_text(encoding="utf-8").strip(), first_hash)
            self.assertEqual([path.name for path in (prefix / "versions").iterdir()], [first_hash])
            uninstall_command = [shutil.which("pwsh"), "-NoProfile", "-File", str(first / "Uninstall.ps1"), "-Prefix", str(prefix), "-CommandDirectory", str(commands)]
            subprocess.run(uninstall_command, check=True, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, creationflags=WINDOW_FLAGS)
            subprocess.run(uninstall_command, check=True, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, creationflags=WINDOW_FLAGS)
            self.assertFalse((commands / "into-md.exe").exists())


if __name__ == "__main__":
    unittest.main()
