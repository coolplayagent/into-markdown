#!/usr/bin/env python3
"""Tests for the bounded PR/native release split."""

from __future__ import annotations

import pathlib
import sys
import tomllib
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from pr_fast_gate import ContractError, SUPPORTED, validate  # noqa: E402


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
