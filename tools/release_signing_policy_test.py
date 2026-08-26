#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import pathlib
import unittest


SOURCE = pathlib.Path(__file__).with_name("release-signing-policy.py")
SPEC = importlib.util.spec_from_file_location("release_signing_policy", SOURCE)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ReleaseSigningPolicyTests(unittest.TestCase):
    def test_unsigned_windows_policy_is_installable_and_explicit(self) -> None:
        result = MODULE.policy("x86_64-pc-windows-msvc", "unsigned", "abc123")
        self.assertTrue(result["installable"])
        self.assertFalse(result["externalPublisherIdentityVerified"])
        self.assertIn("Unknown publisher", result["warning"])
        self.assertIn("Ed25519", result["pluginPackageIntegrity"])

    def test_signed_macos_policy_records_external_identity(self) -> None:
        result = MODULE.policy("aarch64-apple-darwin", "signed", "abc123")
        self.assertTrue(result["externalPublisherIdentityVerified"])
        self.assertEqual(result["externalSigningMechanism"], "Developer ID and Apple notarization")
        self.assertIsNone(result["warning"])


if __name__ == "__main__":
    unittest.main()
