#!/usr/bin/env python3
"""Tests for the bounded PR/native release split."""

from __future__ import annotations

import pathlib
import sys
import tempfile
import tomllib
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from pr_fast_gate import ContractError, SUPPORTED, validate  # noqa: E402
from ci_workflow_policy import WorkflowPolicyError, validate_workflows  # noqa: E402


class WorkflowPolicyTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="into-md-ci-policy-")
        self.addCleanup(temporary.cleanup)
        self.root = pathlib.Path(temporary.name)
        self.directory = self.root / ".github/workflows"
        self.directory.mkdir(parents=True)
        repository = pathlib.Path(__file__).resolve().parents[2]
        for name in ("pr-fast-gate.yml", "platform-modular-release.yml"):
            (self.directory / name).write_text(
                (repository / ".github/workflows" / name).read_text(encoding="utf-8"),
                encoding="utf-8",
            )

    def mutate(self, old: str, new: str, *, release: bool = False) -> None:
        path = self.directory / ("platform-modular-release.yml" if release else "pr-fast-gate.yml")
        source = path.read_text(encoding="utf-8")
        self.assertIn(old, source)
        path.write_text(source.replace(old, new, 1), encoding="utf-8")

    def test_current_workflows_and_added_unit_tests_are_allowed(self) -> None:
        validate_workflows(self.root)
        self.mutate("          cargo fmt --all -- --check", "          python3 -m unittest additional_tests\n"
                    "          cargo fmt --all -- --check")
        validate_workflows(self.root)
        self.mutate("      - name: Test shared Rust", "      - name: Extra focused UT\n"
                    "        run: python3 -m unittest more_tests\n      - name: Test shared Rust")
        validate_workflows(self.root)

    def test_new_workflows_including_manual_and_yaml_extension_are_rejected(self) -> None:
        for name in ("archive-compat.yml", "pdf-resilience.yml", "manual.yaml", "nested"):
            with self.subTest(name=name):
                path = self.directory / name
                path.write_text("name: Extra\non: workflow_dispatch\njobs: {}\n", encoding="utf-8")
                with self.assertRaisesRegex(WorkflowPolicyError, "allowlist"):
                    validate_workflows(self.root)
                path.unlink()

    def test_missing_fast_workflow_is_rejected(self) -> None:
        (self.directory / "pr-fast-gate.yml").unlink()
        with self.assertRaisesRegex(WorkflowPolicyError, "allowlist"):
            validate_workflows(self.root)

    def test_workflow_directory_and_files_must_be_local(self) -> None:
        path = self.directory / "pr-fast-gate.yml"
        saved = self.root / "saved.yml"
        path.rename(saved)
        try:
            path.symlink_to(saved)
        except OSError:
            self.skipTest("host does not permit symlink creation")
        with self.assertRaisesRegex(WorkflowPolicyError, "regular files"):
            validate_workflows(self.root)
        path.unlink()
        saved.rename(path)
        saved_directory = self.root / "saved-workflows"
        self.directory.rename(saved_directory)
        self.directory.symlink_to(saved_directory, target_is_directory=True)
        with self.assertRaisesRegex(WorkflowPolicyError, "directory must be local"):
            validate_workflows(self.root)

    def test_fifth_job_without_display_name_is_rejected(self) -> None:
        self.mutate("jobs:", "jobs:\n  extra:\n    runs-on: ubuntu-24.04\n    steps:\n"
                    "      - run: true")
        with self.assertRaisesRegex(WorkflowPolicyError, "four approved"):
            validate_workflows(self.root)

    def test_matrix_and_conditional_jobs_are_rejected(self) -> None:
        for extra in ("    strategy:\n      matrix:\n        target: [one, two]\n",
                      "    if: false\n", "    continue-on-error: true\n"):
            with self.subTest(extra=extra):
                self.mutate("    steps:\n", extra + "    steps:\n")
                with self.assertRaises(WorkflowPolicyError):
                    validate_workflows(self.root)
                self.mutate(extra + "    steps:\n", "    steps:\n")

    def test_names_runners_timeouts_and_triggers_are_fixed(self) -> None:
        for old, new in (("name: PR fast gate", "name: Other"),
                         ("name: Linux ARM64 Core", "name: Renamed"),
                         ("runs-on: ubuntu-24.04-arm", "runs-on: ubuntu-24.04"),
                         ("timeout-minutes: 5", "timeout-minutes: 30"),
                         ("  pull_request:", "  push:"),
                         ("  pull_request:", "  pull_request:\n    paths: ['docs/**']")):
            with self.subTest(change=new):
                self.mutate(old, new)
                with self.assertRaises(WorkflowPolicyError):
                    validate_workflows(self.root)
                self.mutate(new, old)

    def test_release_cannot_gain_automatic_or_reusable_triggers(self) -> None:
        for trigger in ("push", "pull_request", "schedule", "workflow_call", "workflow_run"):
            with self.subTest(trigger=trigger):
                added = f"on:\n  {trigger}:"
                self.mutate("on:", added, release=True)
                with self.assertRaisesRegex(WorkflowPolicyError, "manual workflow_dispatch"):
                    validate_workflows(self.root)
                self.mutate(added, "on:", release=True)

    def test_duplicate_keys_aliases_and_inline_topology_fail_closed(self) -> None:
        for old, new in (("jobs:", "jobs: {}\njobs:"),
                         ("  linux-arm64:", "  linux-arm64: *extra"),
                         ("on:\n  pull_request:", "on: [pull_request, push]"),
                         ("jobs:", "jobs: &extra"),
                         ("name: PR fast gate", "---\nname: PR fast gate")):
            with self.subTest(change=new):
                self.mutate(old, new)
                with self.assertRaises(WorkflowPolicyError):
                    validate_workflows(self.root)
                self.mutate(new, old)

    def test_policy_invocation_must_remain_in_each_job(self) -> None:
        self.mutate("python3 tools/platform-release/pr_fast_gate.py", "python3 removed_validator.py")
        with self.assertRaisesRegex(WorkflowPolicyError, "validator invocation"):
            validate_workflows(self.root)


class PrFastGateTests(unittest.TestCase):
    def test_ci_installs_repository_toolchain_components_up_front(self) -> None:
        root = pathlib.Path(__file__).resolve().parents[2]
        toolchain = tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]
        workflow = (root / ".github/workflows/pr-fast-gate.yml").read_text()
        setups = workflow.split("- uses: dtolnay/rust-toolchain@stable")[1:]
        self.assertTrue(setups, "PR gate must initialize its Rust toolchains")
        for index, setup in enumerate(setups):
            with self.subTest(setup=index):
                settings = dict(
                    line.strip().split(": ", 1)
                    for line in setup.split("      - ", 1)[0].splitlines()
                    if ": " in line
                )
                self.assertEqual(settings.get("toolchain"), toolchain["channel"])
                self.assertEqual(
                    {value.strip() for value in settings.get("components", "").split(",")},
                    set(toolchain["components"]),
                )

    def test_every_supported_native_host_preserves_release_authority(self) -> None:
        for target, (system, machines) in SUPPORTED.items():
            with self.subTest(target=target):
                validate(target, system=system, machine=sorted(machines)[0])

    def test_cross_platform_alias_is_rejected(self) -> None:
        with self.assertRaisesRegex(ContractError, "requires native Windows"):
            validate(
                "x86_64-pc-windows-msvc",
                system="Linux",
                machine="x86_64",
            )

    def test_wrong_native_architecture_is_rejected(self) -> None:
        with self.assertRaisesRegex(ContractError, "wrong architecture"):
            validate(
                "aarch64-unknown-linux-gnu",
                system="Linux",
                machine="x86_64",
            )


if __name__ == "__main__":
    unittest.main()
