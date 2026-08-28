#!/usr/bin/env python3

from __future__ import annotations

import pathlib
import hashlib
import json
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
    ggml_cpu_init_uses_nonbaseline_x86,
    requires_x86_64_extension_level,
    resolve_release_packages,
    run,
    safe_zip_extract,
)
from platform_acceptance import (
    BUILTIN_CAPABILITIES,
    PLUGINS,
    assert_states,
    capability_map,
    fixture_conversion_arguments,
    repairable_payload_files,
    resolve_package,
    tree_hash,
)

WINDOW_FLAGS = subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0


def windows_powershell_hosts() -> list[pathlib.Path]:
    """Return the user-visible Windows PowerShell hosts without duplicates."""
    if sys.platform != "win32":
        return []
    candidates = [
        pathlib.Path(os.environ.get("SystemRoot", r"C:\Windows"))
        / "System32"
        / "WindowsPowerShell"
        / "v1.0"
        / "powershell.exe",
    ]
    pwsh = shutil.which("pwsh")
    if pwsh:
        candidates.append(pathlib.Path(pwsh))
    hosts: list[pathlib.Path] = []
    seen: set[str] = set()
    for candidate in candidates:
        if not candidate.is_file():
            continue
        identity = os.path.normcase(str(candidate.resolve()))
        if identity not in seen:
            seen.add(identity)
            hosts.append(candidate)
    return hosts


def windows_powershell_environment(
    shell: pathlib.Path, base: dict[str, str] | None = None
) -> dict[str, str]:
    environment = dict(base or os.environ)
    if shell.name.lower() == "powershell.exe":
        # Codex and some build hosts launch tests from PowerShell 7 and prepend
        # its module tree. Recreate the inbox host's normal search path so this
        # test measures Windows PowerShell 5.1 rather than parent-shell leakage.
        environment["PSModulePath"] = os.pathsep.join(
            [
                str(pathlib.Path(environment["USERPROFILE"]) / "Documents/WindowsPowerShell/Modules"),
                str(pathlib.Path(environment["PROGRAMFILES"]) / "WindowsPowerShell/Modules"),
                str(
                    pathlib.Path(environment["SYSTEMROOT"])
                    / "System32/WindowsPowerShell/v1.0/Modules"
                ),
            ]
        )
    return environment


class PlatformToolTests(unittest.TestCase):
    def test_platform_audit_accepts_only_bundled_ocr_and_external_speech(self) -> None:
        target = "x86_64-pc-windows-msvc"
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            core = root / "core"
            plugins = root / "plugins"
            bundled = (
                core
                / "share/into-markdown/plugins/packages/official.ocr.ppocrv6.imp"
            )
            built_ocr = plugins / "official.ocr.ppocrv6.imp"
            external = plugins / "official.media.whisper.imp"
            bundled.parent.mkdir(parents=True)
            plugins.mkdir()

            def package(path: pathlib.Path, plugin_id: str) -> None:
                with zipfile.ZipFile(path, "w") as archive:
                    archive.writestr(
                        "plugin.json",
                        json.dumps(
                            {
                                "id": plugin_id,
                                "supportedTargets": [target],
                                "entrypoints": {target: "bin/provider.exe"},
                            }
                        ),
                    )

            package(bundled, "official.ocr.ppocrv6")
            shutil.copy2(bundled, built_ocr)
            package(external, "official.media.whisper")
            catalog = {
                "schemaVersion": 2,
                "signingKeyId": "official-test",
                "signingKeySha256": "a" * 64,
                "packages": {
                    "official.ocr.ppocrv6": {
                        "file": bundled.name,
                        "sha256": hashlib.sha256(bundled.read_bytes()).hexdigest(),
                    },
                    "official.media.whisper": {
                        "url": "https://example.invalid/official.media.whisper.imp",
                        "sha256": hashlib.sha256(external.read_bytes()).hexdigest(),
                    },
                },
            }
            (core / "share/into-markdown/plugins/official-publisher.json").write_text(
                json.dumps(catalog), encoding="utf-8"
            )
            selected = resolve_release_packages(target, core, plugins, Audit(target))
            self.assertEqual(selected, (bundled, external))

            package(plugins / "unexpected.imp", "official.ocr.ppocrv6")
            with self.assertRaises(AuditFailure):
                resolve_release_packages(target, core, plugins, Audit(target))

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

    def test_linux_native_audit_rejects_raised_x86_64_isa_levels(self) -> None:
        self.assertFalse(
            requires_x86_64_extension_level(
                "Properties: x86 ISA needed: x86-64-baseline\n"
            )
        )
        self.assertTrue(
            requires_x86_64_extension_level(
                "Properties: x86 ISA needed: x86-64-baseline, x86-64-v4\n"
            )
        )
        self.assertFalse(
            requires_x86_64_extension_level(
                "Properties: x86 ISA used: x86-64-v4\n"
            )
        )

    def test_linux_native_audit_rejects_unnoted_ggml_native_instructions(self) -> None:
        baseline = """
0000000000001000 <ggml_cpu_init>:
    1000: 0f 10 00              movups xmm0,XMMWORD PTR [rax]
    1003: 66 0f ef c0           pxor   xmm0,xmm0
"""
        avx = """
0000000000001000 <ggml_cpu_init>:
    1000: c5 f8 57 c0           vxorps xmm0,xmm0,xmm0
"""
        evex = """
0000000000001000 <ggml_cpu_init>:
    1000: 62 e1 6e 08 58 44 24 03 vaddss xmm16,xmm2,DWORD PTR [rsp+0xc]
"""
        self.assertFalse(ggml_cpu_init_uses_nonbaseline_x86(baseline))
        self.assertTrue(ggml_cpu_init_uses_nonbaseline_x86(avx))
        self.assertTrue(ggml_cpu_init_uses_nonbaseline_x86(evex))

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

    def test_real_fixture_commands_have_explicit_isolated_outputs(self) -> None:
        source = pathlib.Path("state/fixture.png")
        output = pathlib.Path("state/outputs/fixture.md")
        arguments = fixture_conversion_arguments(
            source, output, ["--ocr", "always"]
        )
        self.assertEqual(arguments[0], str(source))
        self.assertEqual(arguments[1:5], ["--output", str(output), "--conflict", "error"])
        self.assertEqual(arguments[-4:], ["--ocr", "always", "--emit", "result-json"])
        with self.assertRaisesRegex(ValueError, "different paths"):
            fixture_conversion_arguments(source, source, ["--ocr", "always"])

    def test_core_only_acceptance_requires_ready_ocr_and_absent_speech(self) -> None:
        self.assertEqual(BUILTIN_CAPABILITIES, {"ocr": "official.ocr.ppocrv6"})
        self.assertEqual(
            PLUGINS, {"official.media.whisper": ("transcription", "diarization")}
        )

        class FixtureRunner:
            def __init__(self, ocr_status: str):
                self.ocr_status = ocr_status

            def call(self, *_args, **_kwargs):
                capabilities = [
                    {
                        "id": "ocr",
                        "status": self.ocr_status,
                        "sources": ["plugin:official.ocr.ppocrv6/ocr", "off"],
                    },
                    {
                        "id": "transcription",
                        "status": "not-installed",
                        "sources": [
                            "plugin:official.media.whisper/transcription",
                            "off",
                        ],
                        "setup": "plugins install official.media.whisper",
                    },
                    {
                        "id": "diarization",
                        "status": "not-installed",
                        "sources": [
                            "plugin:official.media.whisper/diarization",
                            "off",
                        ],
                        "setup": "plugins install official.media.whisper",
                    },
                ]
                return subprocess.CompletedProcess(
                    [], 0, json.dumps({"capabilities": capabilities}), ""
                )

        states = assert_states(
            FixtureRunner("ready"), pathlib.Path("state"), set(), "core-only"
        )
        self.assertEqual(states["ocr"]["status"], "ready")
        with self.assertRaisesRegex(RuntimeError, "expected ready from Core"):
            assert_states(
                FixtureRunner("not-installed"),
                pathlib.Path("state"),
                set(),
                "core-only",
            )

    @unittest.skipUnless(
        sys.platform == "win32" and shutil.which("rustc") and windows_powershell_hosts(),
        "native Windows transaction",
    )
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
                shutil.copy2(source / "Install.cmd", value / "Install.cmd")
                shutil.copy2(source / "Uninstall.cmd", value / "Uninstall.cmd")
                manifest = (f"manifest-{label}\n").encode()
                (value / "archive-manifest.json").write_bytes(manifest)
                if rejected:
                    (value / "reject-install").write_bytes(b"reject")
                return value, hashlib.sha256(manifest).hexdigest()

            first, first_hash = distribution("distribution-one")
            subprocess.run([str(first / "bin" / "archive-check.exe"), str(first)], check=True, creationflags=WINDOW_FLAGS)
            second, second_hash = distribution("distribution-two", rejected=True)
            for index, shell in enumerate(windows_powershell_hosts()):
                with self.subTest(powershell=str(shell)):
                    shell_environment = windows_powershell_environment(shell)
                    prefix = root / f"用户-{index}" / "install"
                    commands = root / f"用户-{index}" / "commands"
                    install_command = [
                        str(shell),
                        "-NoLogo",
                        "-NoProfile",
                        "-File",
                        str(first / "Install.ps1"),
                        "-Prefix",
                        str(prefix),
                        "-CommandDirectory",
                        str(commands),
                    ]
                    first_install = subprocess.run(
                        install_command,
                        text=True,
                        encoding="utf-8",
                        errors="replace",
                        stdin=subprocess.DEVNULL,
                        env=shell_environment,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        creationflags=WINDOW_FLAGS,
                    )
                    self.assertEqual(first_install.returncode, 0, first_install.stderr)
                    installed = first_install.stdout.strip().splitlines()[-1]
                    second_install = subprocess.run(
                        install_command,
                        text=True,
                        encoding="utf-8",
                        errors="replace",
                        stdin=subprocess.DEVNULL,
                        env=shell_environment,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        creationflags=WINDOW_FLAGS,
                    )
                    self.assertEqual(second_install.returncode, 0, second_install.stderr)
                    repeated = second_install.stdout.strip().splitlines()[-1]
                    self.assertEqual(installed, repeated)
                    launched = subprocess.run(
                        [str(commands / "into-md.exe"), "hello", "世界"],
                        check=True,
                        text=True,
                        encoding="utf-8",
                        stdout=subprocess.PIPE,
                        creationflags=WINDOW_FLAGS,
                    ).stdout
                    self.assertIn("fixture:hello|世界", launched)

                    failure = subprocess.run(
                        [str(installer), "install", str(second), str(prefix), str(commands), second_hash],
                        text=True,
                        encoding="utf-8",
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        creationflags=WINDOW_FLAGS,
                    )
                    self.assertNotEqual(failure.returncode, 0)
                    self.assertEqual(
                        (prefix / "current.txt").read_text(encoding="utf-8").strip(),
                        first_hash,
                    )
                    self.assertEqual(
                        [path.name for path in (prefix / "versions").iterdir()],
                        [first_hash],
                    )

                    unsafe = subprocess.run(
                        [
                            str(shell),
                            "-NoLogo",
                            "-NoProfile",
                            "-File",
                            str(first / "Install.ps1"),
                            "-Prefix",
                            "relative-install",
                            "-CommandDirectory",
                            str(commands),
                        ],
                        text=True,
                        encoding="utf-8",
                        errors="replace",
                        stdin=subprocess.DEVNULL,
                        env=shell_environment,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        creationflags=WINDOW_FLAGS,
                    )
                    self.assertNotEqual(unsafe.returncode, 0)
                    self.assertIn("installPathUnsafe", unsafe.stdout + unsafe.stderr)

                    uninstall_command = [
                        str(shell),
                        "-NoLogo",
                        "-NoProfile",
                        "-File",
                        str(first / "Uninstall.ps1"),
                        "-Prefix",
                        str(prefix),
                        "-CommandDirectory",
                        str(commands),
                    ]
                    subprocess.run(
                        uninstall_command,
                        check=True,
                        env=shell_environment,
                        stdin=subprocess.DEVNULL,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        creationflags=WINDOW_FLAGS,
                    )
                    subprocess.run(
                        uninstall_command,
                        check=True,
                        env=shell_environment,
                        stdin=subprocess.DEVNULL,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        creationflags=WINDOW_FLAGS,
                    )
                    self.assertFalse((commands / "into-md.exe").exists())

            # The Explorer entry point always uses the inbox Windows PowerShell
            # host and default per-user locations. Keep the entire click-through
            # journey inside the temporary directory.
            click_local = root / "点击安装用户"
            click_env = os.environ.copy()
            click_env["LOCALAPPDATA"] = str(click_local)
            click_env = windows_powershell_environment(
                windows_powershell_hosts()[0], click_env
            )
            subprocess.run(
                [os.environ.get("ComSpec", "cmd.exe"), "/d", "/c", str(first / "Install.cmd")],
                check=True,
                env=click_env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                creationflags=WINDOW_FLAGS,
            )
            self.assertTrue(
                (click_local / "Microsoft" / "WindowsApps" / "into-md.exe").is_file()
            )
            subprocess.run(
                [os.environ.get("ComSpec", "cmd.exe"), "/d", "/c", str(first / "Uninstall.cmd")],
                check=True,
                env=click_env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                creationflags=WINDOW_FLAGS,
            )
            self.assertFalse(
                (click_local / "Microsoft" / "WindowsApps" / "into-md.exe").exists()
            )


if __name__ == "__main__":
    unittest.main()
