#!/usr/bin/env python3
"""Tests for the bounded PR/native release split."""

from __future__ import annotations

import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from pr_fast_gate import ContractError, SUPPORTED, validate  # noqa: E402


class PrFastGateTests(unittest.TestCase):
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
