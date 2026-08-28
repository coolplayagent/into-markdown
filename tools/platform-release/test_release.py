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

    def test_expensive_native_gates_run_in_parallel_with_release_acceptance(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        gates = (ROOT / ".github/workflows/native-release-gates.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("native-gates:", workflow)
        self.assertIn("needs: [native-gates, native-release]", workflow)
        self.assertNotIn("cargo test --workspace --all-targets", workflow)
        self.assertNotIn("cargo test --workspace --all-targets", gates)
        self.assertIn("cargo clippy --workspace", gates)
        self.assertIn("bazel --output_user_root", gates)
        self.assertIn("run: cargo fetch --locked", gates)
        self.assertIn("~/.cargo/registry", gates)
        self.assertIn("repository-cache: true", gates)
        self.assertIn("disk-cache: release-gates-${{ inputs.bazel_config }}", gates)

    def test_windows_core_smoke_uses_installed_pdfium_destination(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertNotIn(r'--pdfium-library "$installed\bin\pdfium.dll"', workflow)
        self.assertGreaterEqual(
            workflow.count(r'--pdfium-library "$installed\lib\pdfium\pdfium.dll"'),
            2,
        )

    def test_native_release_reuses_fixed_toolchain_cargo_dependencies(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("path: ${{ runner.temp }}/build", workflow)
        self.assertIn(
            "key: native-release-rust-1.97.1-${{ runner.os }}-"
            "${{ matrix.target }}-${{ hashFiles('Cargo.lock') }}",
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

    def test_linux_release_bootstrap_installs_hash_pinned_bazelisk(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("bazelbuild/bazelisk/releases/download/v1.27.0", workflow)
        self.assertIn("bazelisk-linux-amd64", workflow)
        self.assertIn("bazelisk-linux-arm64", workflow)
        self.assertIn(
            "e1508323f347ad1465a887bc5d2bfb91cffc232d11e8e997b623227c6b32fb76",
            workflow,
        )
        self.assertIn(
            "bb608519a440d45d10304eb684a73a2b6bb7699c5b0e5434361661b25f113a5d",
            workflow,
        )
        self.assertIn("sha256sum --check", workflow)
        self.assertIn("ln -sf /usr/local/bin/bazelisk /usr/local/bin/bazel", workflow)

    def test_plugin_key_uses_the_same_authoritative_temp_path_as_packaging(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        key_path = "${{ runner.temp }}/plugin-key.pk8"
        windows_key_path = r"${{ runner.temp }}\plugin-key.pk8"
        self.assertIn('install -d -m 700 "${{ runner.temp }}"', workflow)
        self.assertGreaterEqual(workflow.count(key_path), 3)
        self.assertIn(windows_key_path, workflow)
        self.assertNotIn("$RUNNER_TEMP/plugin-key.pk8", workflow)
        self.assertNotIn(r"$env:RUNNER_TEMP\plugin-key.pk8", workflow)

    def test_runtime_fixtures_use_shell_native_workspace_authorities(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        fixture = "fixtures/asr-quality/source/en-clear.wav"
        self.assertEqual(workflow.count(f"$GITHUB_WORKSPACE/{fixture}"), 3)
        self.assertEqual(
            workflow.count(r"$env:GITHUB_WORKSPACE\fixtures\asr-quality\source\en-clear.wav"),
            3,
        )
        self.assertNotIn("${{ github.workspace }}/fixtures/asr-quality", workflow)
        self.assertNotIn(r"${{ github.workspace }}\fixtures\asr-quality", workflow)

    def test_installed_smoke_failures_print_the_machine_report(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertEqual(workflow.count('cat "${{ runner.temp }}/installed-smoke.json"'), 1)
        self.assertEqual(
            workflow.count('Get-Content "${{ runner.temp }}\\installed-smoke.json" -Raw'),
            1,
        )
        self.assertIn(
            'cat "${{ runner.temp }}/into-md-${{ matrix.target }}-core-smoke.json"',
            workflow,
        )
        self.assertIn(
            'Get-Content "${{ runner.temp }}\\into-md-${{ matrix.target }}-core-smoke.json" -Raw',
            workflow,
        )

    def test_windows_cmake_uses_the_activated_pinned_msvc_toolchain(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("toolset: 14.44.35207", workflow)
        self.assertIn("sdk: 10.0.26100.0", workflow)
        self.assertIn("CMAKE_GENERATOR=NMake Makefiles", workflow)

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
        self.assertEqual(CORE_COMPONENTS, ["pdfium"])
        for target in authority()["targets"]:
            groups = {
                "core": set(CORE_COMPONENTS),
                "ocr": set(OCR_COMPONENTS),
                "speech": set(SPEECH_COMPONENTS),
            }
            for plugin, components in groups.items():
                if plugin != "core":
                    self.assertFalse(
                        groups["core"] & components,
                        f"{target}: Core and {plugin} duplicate release resources",
                    )
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
