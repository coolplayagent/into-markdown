#!/usr/bin/env python3
"""Stage one target's release files under a portable artifact root."""

from __future__ import annotations

import argparse
import pathlib
import shutil


REPORT_NAMES = (
    "platform-audit.json",
    "installed-smoke.json",
    "platform-acceptance.json",
)


def require_file(path: pathlib.Path, description: str) -> pathlib.Path:
    if not path.is_file() or path.is_symlink():
        raise RuntimeError(f"{description} is missing or unsafe: {path}")
    return path


def copy_directory_files(source: pathlib.Path, destination: pathlib.Path, description: str) -> None:
    if not source.is_dir() or source.is_symlink():
        raise RuntimeError(f"{description} directory is missing or unsafe: {source}")
    files = sorted(source.iterdir())
    if not files:
        raise RuntimeError(f"{description} directory is empty: {source}")
    destination.mkdir()
    for path in files:
        require_file(path, description)
        shutil.copy2(path, destination / path.name)


def stage_release_artifact(
    core_artifact: pathlib.Path,
    plugins: pathlib.Path,
    metadata: pathlib.Path,
    platform_audit: pathlib.Path,
    installed_smoke: pathlib.Path,
    platform_acceptance: pathlib.Path,
    signing_policy: pathlib.Path,
    output: pathlib.Path,
    require_core_signature: bool = False,
) -> None:
    if output.exists():
        raise RuntimeError(f"release artifact staging output already exists: {output}")

    core_artifact = require_file(core_artifact, "Core release artifact")
    core_digest = require_file(
        pathlib.Path(f"{core_artifact}.sha256"), "Core release digest"
    )
    core_signature = pathlib.Path(f"{core_artifact}.asc")
    if require_core_signature:
        require_file(core_signature, "Core detached signature")
    elif core_signature.exists():
        require_file(core_signature, "Core detached signature")

    reports = {
        REPORT_NAMES[0]: require_file(platform_audit, "platform audit report"),
        REPORT_NAMES[1]: require_file(installed_smoke, "installed smoke report"),
        REPORT_NAMES[2]: require_file(platform_acceptance, "platform acceptance report"),
    }
    signing_policy = require_file(signing_policy, "signing policy")

    output.mkdir(parents=True)
    shutil.copy2(core_artifact, output / core_artifact.name)
    shutil.copy2(core_digest, output / core_digest.name)
    if core_signature.exists():
        shutil.copy2(core_signature, output / core_signature.name)
    copy_directory_files(plugins, output / "published-plugins", "published plugins")
    copy_directory_files(metadata, output / "release-metadata", "release metadata")
    for name, source in reports.items():
        shutil.copy2(source, output / name)
    shutil.copy2(signing_policy, output / signing_policy.name)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--core-artifact", required=True, type=pathlib.Path)
    parser.add_argument("--plugins", required=True, type=pathlib.Path)
    parser.add_argument("--metadata", required=True, type=pathlib.Path)
    parser.add_argument("--platform-audit", required=True, type=pathlib.Path)
    parser.add_argument("--installed-smoke", required=True, type=pathlib.Path)
    parser.add_argument("--platform-acceptance", required=True, type=pathlib.Path)
    parser.add_argument("--signing-policy", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--require-core-signature", action="store_true")
    arguments = parser.parse_args()
    stage_release_artifact(
        arguments.core_artifact.resolve(),
        arguments.plugins.resolve(),
        arguments.metadata.resolve(),
        arguments.platform_audit.resolve(),
        arguments.installed_smoke.resolve(),
        arguments.platform_acceptance.resolve(),
        arguments.signing_policy.resolve(),
        arguments.output.resolve(),
        arguments.require_core_signature,
    )


if __name__ == "__main__":
    main()
