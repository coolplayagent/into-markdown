#!/usr/bin/env python3
"""Write a machine-readable statement of the external release signing policy."""

from __future__ import annotations

import argparse
import json
import pathlib


TARGET_POLICIES = {
    "x86_64-unknown-linux-gnu": ("GPG detached signatures", "No GPG identity signature is included."),
    "aarch64-unknown-linux-gnu": ("GPG detached signatures", "No GPG identity signature is included."),
    "x86_64-pc-windows-msvc": ("Authenticode", "Windows may show Unknown publisher or SmartScreen warnings."),
    "aarch64-pc-windows-msvc": ("Authenticode", "Windows may show Unknown publisher or SmartScreen warnings."),
    "aarch64-apple-darwin": ("Developer ID and Apple notarization", "macOS may require Open Anyway or removal of quarantine after SHA-256 verification."),
}


def policy(target: str, mode: str, source_revision: str) -> dict[str, object]:
    if target not in TARGET_POLICIES:
        raise ValueError(f"unsupported target: {target}")
    if mode not in {"signed", "unsigned"}:
        raise ValueError(f"unsupported signing mode: {mode}")
    mechanism, unsigned_warning = TARGET_POLICIES[target]
    signed = mode == "signed"
    return {
        "schemaVersion": 1,
        "target": target,
        "sourceRevision": source_revision,
        "mode": mode,
        "installable": True,
        "externalPublisherIdentityVerified": signed,
        "externalSigningMechanism": mechanism if signed else None,
        "warning": None if signed else unsigned_warning,
        "pluginPackageIntegrity": "Ed25519 manifest signature and pinned SHA-256",
    }


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", choices=sorted(TARGET_POLICIES), required=True)
    parser.add_argument("--mode", choices=("signed", "unsigned"), required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    result = policy(arguments.target, arguments.mode, arguments.source_revision)
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
