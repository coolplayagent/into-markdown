#!/usr/bin/env python3
"""Host-independent contract tests for the Linux and Windows release assembler."""

from __future__ import annotations

import ast
import hashlib
import importlib.util
import inspect
import json
import pathlib
import re
import sys
import tempfile
import tomllib
import unittest
import zipfile
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import release as release_module
from common import ROOT, ReleaseError, authority, run, sha256
from release import (
    CORE_COMPONENTS,
    OCR_COMPONENTS,
    RELEASE_BUILD_PRODUCTS,
    SPEECH_COMPONENTS,
    VERSION,
    WINDOWS_CORE_PREWARM_PRODUCT,
    create_archive,
    distributed_source_ids,
    portable_cpu_environment,
    published_plugin_file,
)


class PlatformReleaseTests(unittest.TestCase):
    def test_cargo_runtime_authority_binds_current_workspace_manifests(self) -> None:
        authority_path = ROOT / "third_party/licenses/cargo-normal-runtime.json"
        value = json.loads(authority_path.read_text(encoding="utf-8"))

        def canonical_digest(path: pathlib.Path) -> str:
            contents = path.read_bytes().replace(b"\r\n", b"\n")
            self.assertNotIn(b"\r", contents)
            return hashlib.sha256(contents).hexdigest()

        self.assertEqual(value["cargo_lock_sha256"], canonical_digest(ROOT / "Cargo.lock"))
        for relative, expected in value["workspace_manifest_sha256"].items():
            self.assertEqual(expected, canonical_digest(ROOT / relative), relative)

        whisper_sys = tomllib.loads(
            (ROOT / "third_party/whisper-rs-0.16.0/sys/Cargo.toml").read_text(
                encoding="utf-8"
            )
        )["package"]
        self.assertEqual(whisper_sys["license"], "Unlicense")
        self.assertFalse(whisper_sys["publish"])

    def test_all_platforms_preserve_vendored_whisper_sys_source_and_licenses(self) -> None:
        required = [
            'component["id"] == "cargo:whisper-rs-sys@0.15.0"',
            '"cargo/whisper-rs-sys-0.15.0-vendored.zip"',
            '"whisper-rs-sys-Unlicense.txt"',
            '"whisper.cpp-MIT.txt"',
        ]
        for relative in [
            "tools/platform-release/release.py",
            "tools/macos-release/release.py",
        ]:
            source = (ROOT / relative).read_text(encoding="utf-8")
            for marker in required:
                self.assertIn(marker, source, f"{relative} lacks {marker}")

    def test_macos_archive_calls_match_the_real_writer_signature(self) -> None:
        archive_path = ROOT / "tools/macos-release/archive.py"
        spec = importlib.util.spec_from_file_location("macos_release_archive", archive_path)
        self.assertIsNotNone(spec)
        self.assertIsNotNone(spec.loader)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.assertEqual(
            tuple(inspect.signature(module.create).parameters),
            ("source", "destination", "epoch"),
        )

        release_path = ROOT / "tools/macos-release/release.py"
        tree = ast.parse(release_path.read_text(encoding="utf-8"), release_path.as_posix())
        calls = [
            node
            for node in ast.walk(tree)
            if isinstance(node, ast.Call)
            and isinstance(node.func, ast.Name)
            and node.func.id == "create_archive"
        ]
        self.assertGreaterEqual(len(calls), 2)
        self.assertTrue(all(len(call.args) == 3 and not call.keywords for call in calls))

    def test_web_release_spdx_binds_the_production_app(self) -> None:
        value = json.loads(
            (ROOT / "third_party/licenses/npm-release.spdx.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(len(value["files"]), 1)
        entry = value["files"][0]
        relative = entry["fileName"].removeprefix("./")
        app = ROOT / relative
        self.assertTrue(app.is_file(), relative)
        expected = next(
            item["checksumValue"]
            for item in entry["checksums"]
            if item["algorithm"] == "SHA256"
        )
        self.assertEqual(expected, hashlib.sha256(app.read_bytes()).hexdigest())

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
        self.assertTrue(all(value <= 5 for value in timeouts))

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
        self.assertIn("timeout-minutes: ${{ matrix.timeout_minutes }}", workflow)
        self.assertEqual(workflow.count("timeout_minutes: 30"), 4)
        self.assertNotIn("timeout_minutes: 35", workflow)
        windows = workflow.index("target: x86_64-pc-windows-msvc")
        self.assertIn("timeout_minutes: 30", workflow[windows : windows + 100])
        self.assertIn("INTO_MD_RELEASE_STREAM_LOGS: '1'", workflow)

    def test_release_build_only_validates_without_mutating_github_release(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertRegex(
            workflow,
            r"build_only:\n(?s:.*?)default: false\n\s+type: boolean",
        )
        self.assertIn("if: ${{ inputs.build_only }}", workflow)
        self.assertEqual(workflow.count("if: ${{ !inputs.build_only }}"), 1)
        mutation_step = workflow.index("- name: Create or update draft release")
        guard = workflow.index("if: ${{ !inputs.build_only }}", mutation_step)
        self.assertLess(guard, workflow.index("gh release view", mutation_step))
        for command in [
            "gh release create",
            "gh release edit",
            "gh release delete-asset",
            "gh release upload",
        ]:
            self.assertGreater(workflow.index(command), guard)

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
            "key: release-cargo-only-v2-${{ matrix.target }}-"
            "${{ hashFiles('Cargo.lock') }}",
            workflow,
        )
        self.assertIn("${{ runner.temp }}/release-work/cache", workflow)
        self.assertIn(
            "key: release-native-inputs-v1-${{ matrix.target }}-"
            "${{ hashFiles('tools/platform-release/authority.json', "
            "'tools/macos-release/authority.json') }}",
            workflow,
        )

    def test_product_release_acquires_reviewed_ffmpeg_without_source_build(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("tools/ffmpeg_runtime.py acquire", workflow)
        self.assertIn('third_party/ffmpeg/runtime-assets.json', (
            ROOT / "tools/ffmpeg_runtime.py"
        ).read_text(encoding="utf-8"))
        self.assertNotIn("tools/ffmpeg-build-audit.sh", workflow)
        self.assertNotIn("release-ffmpeg-lgpl", workflow)
        audit = (ROOT / ".github/workflows/ffmpeg-artifact-audit.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("tools/ffmpeg-build-audit.sh", audit)
        self.assertIn("tools/ffmpeg_runtime.py package", audit)
        self.assertIn("ffmpeg-runtime-candidate-${{ matrix.target }}", audit)

    def test_release_records_actionable_phase_timings(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        for phase in [
            "ffmpeg-acquire",
            "helper-provider-build",
            "final-core-link",
            "native-e2e",
            "artifact-upload",
            "cache-upload",
        ]:
            self.assertIn(phase, workflow if phase not in {
                "helper-provider-build", "final-core-link"
            } else (ROOT / "tools/portable-release/assemble.py").read_text(encoding="utf-8"))
        self.assertIn("$GITHUB_STEP_SUMMARY", workflow)
        self.assertIn("timing-${{ matrix.target }}", workflow)

    def test_ffmpeg_runtime_asset_authority_is_exact_and_repository_owned(self) -> None:
        value = json.loads(
            (ROOT / "third_party/ffmpeg/runtime-assets.json").read_text(encoding="utf-8")
        )
        self.assertEqual(value["schemaVersion"], 1)
        self.assertEqual(value["releaseTag"], "runtime-assets")
        self.assertEqual(value["ffmpegVersion"], "8.1.2")
        self.assertRegex(value["sourceRevision"], r"^[0-9a-f]{40}$")
        self.assertEqual(
            value["sourceSha256"],
            "464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c",
        )
        self.assertEqual(
            set(value["targets"]),
            {
                "aarch64-apple-darwin",
                "aarch64-unknown-linux-gnu",
                "x86_64-pc-windows-msvc",
                "x86_64-unknown-linux-gnu",
            },
        )
        for target, record in value["targets"].items():
            self.assertEqual(
                record["url"],
                "https://github.com/coolplayagent/into-markdown/releases/download/"
                f"runtime-assets/ffmpeg-lgpl-8.1.2-{target}.zip",
            )
            self.assertGreater(record["bytes"], 0)
            self.assertRegex(record["sha256"], r"^[0-9a-f]{64}$")
            suffix = ".exe" if target == "x86_64-pc-windows-msvc" else ""
            self.assertEqual(
                set(record["members"]),
                {
                    "COPYING.LGPLv2.1",
                    f"ffmpeg-{target}{suffix}",
                    f"ffmpeg-authority-{target}.json",
                    f"ffmpeg-inventory-{target}.json",
                    f"ffmpeg-relink-{target}.tar",
                },
            )
            for member in record["members"].values():
                self.assertGreater(member["bytes"], 0)
                self.assertRegex(member["sha256"], r"^[0-9a-f]{64}$")

    def test_release_build_compiles_exact_helper_products_once(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            output = pathlib.Path(name)
            commands = []

            def fake_run(arguments, **_kwargs):
                command = [str(value) for value in arguments]
                commands.append(command)
                if command[:2] == ["cargo", "build"]:
                    release_bin = output / "release"
                    release_bin.mkdir(exist_ok=True)
                    for _, binary in [
                        *RELEASE_BUILD_PRODUCTS,
                        WINDOWS_CORE_PREWARM_PRODUCT,
                    ]:
                        (release_bin / release_module.executable_name(binary, "x86_64-pc-windows-msvc")).touch()
                    for binary, _ in release_module.PROVIDER_BUILD_PRODUCTS:
                        (release_bin / f"{binary}.exe").touch()
                return ""

            with mock.patch.object(release_module, "run", side_effect=fake_run):
                self.assertEqual(
                    release_module.build("x86_64-pc-windows-msvc", output),
                    output / "release",
                )
            builds = [command for command in commands if command[:2] == ["cargo", "build"]]
            self.assertEqual(len(builds), 3)
            selections = [
                builds[0][index : index + 4]
                for index, value in enumerate(builds[0])
                if value == "-p"
            ]
            self.assertEqual(
                selections,
                [
                    ["-p", package, "--bin", binary]
                    for package, binary in [
                        *RELEASE_BUILD_PRODUCTS,
                        WINDOWS_CORE_PREWARM_PRODUCT,
                    ]
                ],
            )
            self.assertEqual(
                [(build[build.index("--bin") + 1], build[build.index("--features") + 1]) for build in builds[1:]],
                list(release_module.PROVIDER_BUILD_PRODUCTS),
            )

    def test_release_build_disables_native_ggml_cpu_tuning(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            output = pathlib.Path(name)
            environments = []

            def fake_run(arguments, **kwargs):
                command = [str(value) for value in arguments]
                if command[:2] == ["cargo", "build"]:
                    environments.append(kwargs["env"])
                    release_bin = output / "release"
                    release_bin.mkdir(exist_ok=True)
                    for _, binary in RELEASE_BUILD_PRODUCTS:
                        (release_bin / binary).touch()
                    for binary, _ in release_module.PROVIDER_BUILD_PRODUCTS:
                        (release_bin / binary).touch()
                return ""

            with mock.patch.object(release_module, "run", side_effect=fake_run):
                release_module.build("x86_64-unknown-linux-gnu", output)
            self.assertEqual(len(environments), 3)
            self.assertEqual(environments[0]["GGML_NATIVE"], "OFF")
            self.assertEqual(environments[0]["GGML_CPU_ALL_VARIANTS"], "ON")
            for key in (
                "GGML_SSE42",
                "GGML_AVX",
                "GGML_AVX2",
                "GGML_FMA",
                "GGML_F16C",
                "GGML_BMI2",
                "GGML_AVX512",
            ):
                self.assertEqual(environments[0][key], "OFF")
            self.assertIn("target-cpu=x86-64", environments[0]["RUSTFLAGS"])

    def test_release_cpu_policy_is_explicit_for_every_x86_64_extension(self) -> None:
        policy = portable_cpu_environment("x86_64-unknown-linux-gnu")
        self.assertEqual(policy["GGML_NATIVE"], "OFF")
        self.assertEqual(policy["GGML_CPU_ALL_VARIANTS"], "ON")
        self.assertTrue(
            all(
                value == ("ON" if key == "GGML_CPU_ALL_VARIANTS" else "OFF")
                for key, value in policy.items()
            )
        )
        self.assertNotIn("GGML_AVX", portable_cpu_environment("aarch64-unknown-linux-gnu"))

    def test_explicit_ggml_directory_does_not_append_environment_backend(self) -> None:
        source = (
            ROOT
            / "third_party/whisper-rs-0.16.0/sys/whisper.cpp/ggml/src/ggml-backend-reg.cpp"
        ).read_text(encoding="utf-8")
        marker = source.index('std::getenv("GGML_BACKEND_PATH")')
        guard = source.rfind("if (dir_path == nullptr)", 0, marker)
        self.assertGreater(guard, source.index("void ggml_backend_load_all_from_path"))
        cpu_loader = source[source.index("void ggml_backend_load_cpu_from_path") :]
        self.assertIn('ggml_backend_load_best("cpu", silent, dir_path)', cpu_loader)
        self.assertNotIn("GGML_BACKEND_PATH", cpu_loader)
        for backend in ("rpc", "cuda", "vulkan", "blas"):
            self.assertNotIn(f'ggml_backend_load_best("{backend}"', cpu_loader)

    def test_runtime_dispatch_pins_shared_library_install_directory(self) -> None:
        source = (
            ROOT / "third_party/whisper-rs-0.16.0/sys/build.rs"
        ).read_text(encoding="utf-8")
        install_dir = 'config.define("CMAKE_INSTALL_LIBDIR", "lib")'
        self.assertEqual(source.count(install_dir), 1)
        self.assertGreater(
            source.index(install_dir),
            source.index("for (key, value) in env::vars()"),
        )
        self.assertLess(
            source.index(install_dir),
            source.index('if cfg!(not(feature = "openmp"))'),
        )

    def test_stage_ggml_runtime_requires_and_copies_exact_windows_closure(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            output = root / "release/build/whisper-rs-sys-fixture/out"
            expected = {
                *release_module.GGML_CPU_VARIANTS["x86_64-pc-windows-msvc"],
                "whisper.dll",
                "ggml.dll",
                "ggml-base.dll",
            }
            for filename in expected:
                path = output / "bin" / filename
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(filename.encode())
            destination = root / "speech/bin"
            staged = release_module.stage_ggml_runtime(
                root, "x86_64-pc-windows-msvc", destination
            )
            self.assertEqual({path.name for path in staged}, expected)
            self.assertEqual({path.name for path in destination.iterdir()}, expected)

    def test_release_build_rejects_a_missing_helper_product(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            output = pathlib.Path(name)

            def fake_run(arguments, **_kwargs):
                command = [str(value) for value in arguments]
                if command[:2] == ["cargo", "build"]:
                    release_bin = output / "release"
                    release_bin.mkdir(exist_ok=True)
                    for _, binary in RELEASE_BUILD_PRODUCTS:
                        (release_bin / release_module.executable_name(binary, "x86_64-pc-windows-msvc")).touch()
                    for binary, _ in release_module.PROVIDER_BUILD_PRODUCTS:
                        (release_bin / f"{binary}.exe").touch()
                return ""

            with mock.patch.object(release_module, "run", side_effect=fake_run):
                with self.assertRaisesRegex(ReleaseError, "omitted required products"):
                    release_module.build("x86_64-pc-windows-msvc", output)

    def test_final_core_rebuild_uses_staged_embedded_runtime_feature(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            commands = []

            environments = []

            def fake_run(arguments, **kwargs):
                commands.append([str(value) for value in arguments])
                environments.append(kwargs["env"])
                return ""

            with mock.patch.object(release_module, "run", side_effect=fake_run):
                result = release_module.build_embedded_core(
                    "x86_64-pc-windows-msvc",
                    root / "build",
                    root / "pdfium",
                    root / "ocr",
                )
            self.assertEqual(
                result,
                root / "build/release/into-md.exe",
            )
            self.assertEqual(len(commands), 1)
            self.assertIn("into-markdown-cli", commands[0])
            self.assertIn("into-md", commands[0])
            feature = commands[0].index("--features")
            self.assertEqual(commands[0][feature + 1], "embedded-runtime")
            self.assertEqual(environments[0]["GGML_NATIVE"], "OFF")
            self.assertIn("target-cpu=x86-64", environments[0]["RUSTFLAGS"])

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

    def test_linux_x86_release_binds_a_compiler_for_every_ggml_variant(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        baseline = authority()["targets"]["x86_64-unknown-linux-gnu"][
            "buildBaseline"
        ]
        self.assertEqual(baseline["nativeCompiler"], "gcc-toolset-15")
        self.assertIn("gcc-toolset-15-gcc gcc-toolset-15-gcc-c++", workflow)
        self.assertIn("/opt/rh/gcc-toolset-15/root/usr/bin", workflow)
        self.assertIn('echo "CC=$toolset_bin/gcc"', workflow)
        self.assertIn('echo "CXX=$toolset_bin/g++"', workflow)
        self.assertIn("-mavxvnni", workflow)
        self.assertIn("-mamx-int8", workflow)
        self.assertNotIn("GGML_CPU_ALL_VARIANTS=OFF", workflow)

    def test_release_cargo_cache_is_bound_to_native_cpu_policy(self) -> None:
        workflow = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("release-cargo-only-v2-", workflow)
        self.assertIn(
            "hashFiles('tools/platform-release/authority.json', 'tools/platform-release/cpu-policy.json')",
            workflow,
        )
        self.assertNotIn("release-cargo-only-v1-", workflow)

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
        self.assertIn('unexpected public product set before audit', workflow)
        self.assertIn('unexpected final release set', workflow)
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
            if "path" in download:
                self.assertTrue(download["path"].startswith("third_party/runtime-assets/models/"))
                self.assertTrue(download["source_url"].startswith("https://"))
            else:
                self.assertTrue(download["url"].startswith("https://github.com/coolplayagent/into-markdown/releases/download/" if "source_url" in download else "https://"))
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
