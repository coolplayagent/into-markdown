#!/usr/bin/env python3
"""Host-independent contract tests for the Linux and Windows release assembler."""

from __future__ import annotations

import pathlib
import re
import sys
import tempfile
import tomllib
import unittest
import zipfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from common import ROOT, ReleaseError, authority, run, sha256
from release import (
    CORE_COMPONENTS,
    OCR_COMPONENTS,
    SPEECH_COMPONENTS,
    VERSION,
    create_archive,
    distributed_source_ids,
    published_plugin_file,
)


class PlatformReleaseTests(unittest.TestCase):
    def test_release_version_matches_workspace_and_bazel_module(self) -> None:
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        self.assertEqual(VERSION, workspace["workspace"]["package"]["version"])
        module = (ROOT / "MODULE.bazel").read_text(encoding="utf-8")
        self.assertRegex(module, rf'(?s)module\(.*?version = "{re.escape(VERSION)}"')

    def test_pull_requests_expose_exactly_four_bounded_checks(self) -> None:
        workflow = (ROOT / ".github/workflows/pr-fast-gate.yml").read_text(
            encoding="utf-8"
        )
        checks = re.findall(r"(?m)^  [a-z][a-z0-9-]*:\n    name:", workflow)
        self.assertEqual(len(checks), 4)
        timeouts = [int(value) for value in re.findall(r"timeout-minutes: (\d+)", workflow)]
        self.assertEqual(len(timeouts), len(checks))
        self.assertTrue(all(value <= 20 for value in timeouts))

    def test_one_cargo_only_release_matrix_builds_all_four_targets(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertEqual(workflow.count("workflow_dispatch:"), 1)
        for target in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
            "aarch64-apple-darwin",
        ]:
            self.assertEqual(workflow.count(f"target: {target}"), 1)
        self.assertIn("tools/portable-release/assemble.py build", workflow)
        self.assertNotIn("bazel ", workflow)
        self.assertNotIn("tools/platform-release/release.py", workflow)
        for forbidden in ["installed-smoke", "platform_acceptance.py", "into-md-installer"]:
            self.assertNotIn(forbidden, workflow)

    def test_embedded_core_is_verified_without_an_installer_or_launcher(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("tools/portable-release/assemble.py verify", workflow)
        self.assertIn("tools/portable-release/native_acceptance.py", workflow)
        self.assertIn("native-audit.json", (ROOT / "tools/portable-release/native_acceptance.py").read_text(encoding="utf-8"))
        self.assertIn("e2e.json", (ROOT / "tools/portable-release/native_acceptance.py").read_text(encoding="utf-8"))
        self.assertNotIn("portable-launcher", workflow)
        self.assertNotIn("portable-pack", workflow)
        self.assertNotIn("Install.ps1", workflow)
        self.assertNotIn("Uninstall.ps1", workflow)

    def test_windows_core_archive_contains_clickable_install_entries(self) -> None:
        source = (ROOT / "tools/platform-release/release.py").read_text(
            encoding="utf-8"
        )
        for name in ["Install.ps1", "Uninstall.ps1", "Install.cmd", "Uninstall.cmd"]:
            self.assertIn(
                f'copy_file(pathlib.Path(__file__).with_name("{name}"), output / "{name}")',
                source,
            )
        install_ps1 = (ROOT / "tools/platform-release/Install.ps1").read_text(
            encoding="utf-8"
        )
        install_cmd = (ROOT / "tools/platform-release/Install.cmd").read_text(
            encoding="utf-8"
        )
        uninstall_ps1 = (ROOT / "tools/platform-release/Uninstall.ps1").read_text(
            encoding="utf-8"
        )
        uninstall_cmd = (ROOT / "tools/platform-release/Uninstall.cmd").read_text(
            encoding="utf-8"
        )
        self.assertIn("into-md-installer.exe\" install", install_ps1)
        self.assertIn("| Out-Null", install_ps1)
        self.assertEqual(install_cmd.count("Installation completed successfully."), 0)
        self.assertIn("$helper uninstall", uninstall_ps1)
        self.assertIn("| Out-Null", uninstall_ps1)
        self.assertEqual(uninstall_cmd.count("Into Markdown was removed successfully."), 0)

    def test_native_release_reuses_fixed_toolchain_cargo_dependencies(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("${{ runner.temp }}/release-work/build", workflow)
        self.assertIn(
            "key: release-cargo-only-v1-${{ matrix.target }}-"
            "${{ hashFiles('Cargo.lock') }}",
            workflow,
        )

    def test_release_build_prefetches_complete_locked_cargo_closure(self) -> None:
        source = (ROOT / "tools/platform-release/release.py").read_text(
            encoding="utf-8"
        )
        self.assertIn('run(["cargo", "fetch", "--locked"]', source)

    def test_linux_release_bootstrap_provides_bindgen_libclang(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("dnf install --assumeyes binutils clang-libs", workflow)
        self.assertIn('echo "LIBCLANG_PATH=$(dirname "$libclang_path")"', workflow)

    def test_linux_release_does_not_bootstrap_a_second_build_system(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("bazelisk", workflow.lower())
        self.assertNotIn("setup-bazel", workflow)
        self.assertNotIn("pnpm", workflow.lower())

    def test_unsigned_is_default_and_uses_an_ephemeral_integrity_key(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertRegex(workflow, r"signing_mode:\n(?s:.*?)default: unsigned")
        self.assertNotIn("PLUGIN_SIGNING_KEY_BASE64", workflow)
        self.assertIn("openssl genpkey -algorithm ED25519", workflow)
        self.assertIn('--plugin-signing-key "$RUNNER_TEMP/plugin-integrity-key.pk8"', workflow)
        self.assertIn('rm -f "$RUNNER_TEMP/plugin-integrity-key.pk8"', workflow)

    def test_release_does_not_run_slow_source_tree_or_lifecycle_fixtures(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("fixtures/asr-quality", workflow)
        self.assertNotIn("plugin_manager_process_fixture", workflow)
        self.assertNotIn("capabilities list", workflow)

    def test_release_has_no_double_assembly_or_installed_lifecycle(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("core-a", workflow)
        self.assertNotIn("core-b", workflow)
        self.assertNotIn("plugins install", workflow)
        self.assertNotIn("plugins verify", workflow)

    def test_windows_cmake_uses_the_activated_pinned_msvc_toolchain(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("toolset: 14.44.35207", workflow)
        self.assertIn("sdk: 10.0.26100.0", workflow)
        self.assertIn("CMAKE_GENERATOR=NMake Makefiles", workflow)
        self.assertIn("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER", workflow)
        self.assertIn("$env:VCToolsInstallDir", workflow)

    def test_command_failure_preserves_bounded_diagnostic_tail(self) -> None:
        script = (
            "import sys; "
            "[print(f'diagnostic-{index}', file=sys.stderr) for index in range(45)]; "
            "raise SystemExit(7)"
        )
        with self.assertRaises(ReleaseError) as raised:
            run([sys.executable, "-c", script])
        message = str(raised.exception)
        self.assertIn("exit 7", message)
        self.assertNotIn("diagnostic-4\n", message)
        self.assertIn("diagnostic-5", message)
        self.assertIn("diagnostic-44", message)

    def test_published_plugin_names_are_flat_and_target_unique(self) -> None:
        self.assertEqual(
            published_plugin_file("official.ocr.ppocrv6.imp", "x86_64-pc-windows-msvc"),
            "official.ocr.ppocrv6-x86_64-pc-windows-msvc.imp",
        )
        with self.assertRaisesRegex(RuntimeError, "filename"):
            published_plugin_file("nested/package.imp", "x86_64-pc-windows-msvc")

    def test_release_core_bundles_ocr_and_only_speech_remains_optional(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("tools/portable-release/assemble.py build", workflow)
        self.assertIn("official.media.whisper-${{ matrix.target }}.imp", workflow)
        self.assertNotIn("official.ocr.ppocrv6-${{ matrix.target }}.imp", workflow)
        self.assertIn("OCR is already included in Core", workflow)

    def test_release_page_exposes_four_core_four_speech_skill_and_evidence(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        for name in [
            "into-md-linux-x86_64.zip",
            "into-md-linux-arm64.zip",
            "into-md-windows-x86_64.zip",
            "into-md-macos-arm64.zip",
            "into-markdown-skill.zip",
            "-audit.zip",
        ]:
            self.assertIn(name, workflow)
        self.assertIn("official.media.whisper-${{ matrix.target }}.imp", workflow)
        self.assertIn('test "$(find "$RUNNER_TEMP/publish" -maxdepth 1 -type f | wc -l)" -eq 10', workflow)
        self.assertIn('expected nine public products before audit', workflow)
        self.assertIn('evidence / "release-set.json"', workflow)
        self.assertEqual(workflow.count('gh release upload "$RELEASE_TAG"'), 1)
        self.assertIn("GitHub provides source archives automatically", workflow)

    def test_release_projection_excludes_non_distributed_source_records(self) -> None:
        manifest = {
            "components": [
                {"id": "cargo:runtime@1", "distributed": True},
                {"id": "npm:build@1", "distributed": False},
                {"id": "font:test", "distributed": False},
            ]
        }
        self.assertEqual(distributed_source_ids(manifest), ["cargo:runtime@1"])

    def test_release_matrix_has_exact_core_and_plugin_resource_partition(self) -> None:
        # The outer archive projection declares only direct Core files. The
        # release metadata pass opens the exact bundled OCR IMP and folds its
        # verified component closure into the final Core SBOM.
        self.assertEqual(CORE_COMPONENTS, ["pdfium"])
        for target in authority()["targets"]:
            groups = {
                "core": set(CORE_COMPONENTS),
                "ocr": set(OCR_COMPONENTS),
                "speech": set(SPEECH_COMPONENTS),
            }
            self.assertFalse(groups["core"] & groups["ocr"])
            self.assertFalse(groups["core"] & groups["speech"])
            self.assertIn("onnxruntime-cpu", groups["ocr"])
            self.assertIn("onnxruntime-cpu", groups["speech"])

    def test_authority_is_exact_and_hash_pinned(self) -> None:
        value = authority()
        self.assertEqual(value["sourceDateEpoch"], 1_767_225_600)
        for target, config in value["targets"].items():
            self.assertIn(config["os"], {"linux", "windows"})
            baseline = config["buildBaseline"]
            self.assertNotIn("native", baseline["cpu"])
            if config["os"] == "linux":
                self.assertEqual(baseline["glibcMaximum"], "2.28")
                self.assertEqual(baseline["kernelMinimum"], "5.15")
                self.assertTrue(
                    baseline["container"].startswith(
                        "docker.io/rockylinux/rockylinux:8.10@sha256:"
                    ),
                    (target, baseline["container"]),
                )
                self.assertRegex(baseline["container"], r"@sha256:[0-9a-f]{64}$")
            else:
                self.assertRegex(baseline["msvcTools"], r"^\d+\.\d+\.\d+$")
                self.assertRegex(baseline["windowsSdk"], r"^\d+\.\d+\.\d+\.\d+$")
            for name in ["pdfium", "onnxruntime"]:
                download = config[name]
                self.assertTrue(download["url"].startswith("https://"), (target, name))
                self.assertRegex(download["sha256"], r"^[0-9a-f]{64}$")
                if name == "pdfium":
                    self.assertGreater(download["bytes"], 0)
        for download in value["sharedDownloads"].values():
            self.assertTrue(download["url"].startswith("https://"))
            self.assertGreater(download["bytes"], 0)
            self.assertRegex(download["sha256"], r"^[0-9a-f]{64}$")

    def test_installed_smoke_uses_the_pinned_windows_native_toolchain(self) -> None:
        windows = authority()["targets"]["x86_64-pc-windows-msvc"]["buildBaseline"]
        consumer = (
            pathlib.Path(__file__).resolve().parents[1]
            / "installed-smoke"
            / "src"
            / "rust_consumer.rs"
        ).read_text(encoding="utf-8")
        constants = dict(
            re.findall(r'const (MSVC_VERSION|SDK_VERSION): &str = "([^"]+)";', consumer)
        )
        self.assertEqual(constants["MSVC_VERSION"], windows["msvcTools"])
        self.assertEqual(constants["SDK_VERSION"], windows["windowsSdk"])
    def test_windows_zip_is_byte_reproducible_and_contains_only_regular_files(self) -> None:
        config = {"archive": "zip"}
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = root / "source"
            source.mkdir()
            (source / "z.txt").write_text("z\n", encoding="utf-8")
            (source / "a.txt").write_text("a\n", encoding="utf-8")
            first = root / "first.zip"
            second = root / "second.zip"
            create_archive(source, first, config, 1_767_225_600)
            create_archive(source, second, config, 1_767_225_600)
            self.assertEqual(sha256(first), sha256(second))
            with zipfile.ZipFile(first) as archive:
                self.assertEqual(archive.namelist(), ["a.txt", "z.txt"])
                self.assertEqual(
                    [item.date_time for item in archive.infolist()],
                    [(2026, 1, 1, 0, 0, 0), (2026, 1, 1, 0, 0, 0)],
                )


if __name__ == "__main__":
    unittest.main()
