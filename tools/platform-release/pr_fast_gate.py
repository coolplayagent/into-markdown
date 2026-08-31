#!/usr/bin/env python3
"""Fast, host-native contracts that complement the full release build.

The pull-request gate deliberately does not compile the media provider: doing
so builds whisper.cpp and its complete CPU-variant closure on every platform.
This validator keeps that split fail-closed by proving the Cargo feature chain,
CPU authority, host/target match, and the full native release workflow remain
connected. The protected release workflow remains the executable authority.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import platform
import re
import sys
import tomllib

from ci_workflow_policy import WorkflowPolicyError, validate_workflows


ROOT = pathlib.Path(__file__).resolve().parents[2]
SUPPORTED = {
    "x86_64-unknown-linux-gnu": ("Linux", {"x86_64", "amd64"}),
    "aarch64-unknown-linux-gnu": ("Linux", {"aarch64", "arm64"}),
    "x86_64-pc-windows-msvc": ("Windows", {"amd64", "x86_64"}),
    "aarch64-apple-darwin": ("Darwin", {"arm64", "aarch64"}),
}


class ContractError(RuntimeError):
    """A pull-request/release authority boundary is inconsistent."""


def load_toml(relative: str) -> dict:
    return tomllib.loads((ROOT / relative).read_text(encoding="utf-8"))


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ContractError(message)


def validate_host(target: str, system: str, machine: str) -> None:
    expected_system, expected_machines = SUPPORTED[target]
    require(system == expected_system, f"{target} gate requires native {expected_system}")
    require(machine.lower() in expected_machines, f"{target} gate has wrong architecture: {machine}")


def validate_feature_chain() -> None:
    provider = load_toml("apps/official-provider/Cargo.toml")["features"]
    api = load_toml("crates/api/Cargo.toml")["features"]
    asr = load_toml("crates/asr/Cargo.toml")["features"]
    whisper = load_toml("third_party/whisper-rs-0.16.0/Cargo.toml")["features"]
    whisper_sys = load_toml("third_party/whisper-rs-0.16.0/sys/Cargo.toml")["features"]
    require(
        provider.get("media-runtime") == ["into-markdown/official-provider-runtime"],
        "media provider no longer selects the reviewed API runtime",
    )
    require(
        "into-markdown-asr/runtime-dispatch" in api.get("official-provider-runtime", []),
        "official provider no longer enables ASR runtime dispatch",
    )
    require(
        asr.get("runtime-dispatch") == ["whisper-rs/runtime-dispatch"],
        "ASR runtime dispatch no longer reaches whisper-rs",
    )
    require(
        whisper.get("runtime-dispatch") == ["whisper-rs-sys/runtime-dispatch"],
        "whisper-rs runtime dispatch no longer reaches the native build",
    )
    require("runtime-dispatch" in whisper_sys, "native whisper runtime-dispatch feature is absent")


def validate_target_authority(target: str) -> None:
    if target == "aarch64-apple-darwin":
        authority = json.loads(
            (ROOT / "tools/macos-release/authority.json").read_text(encoding="utf-8")
        )
        require(authority.get("target") == target, "macOS authority target drifted")
        return

    authority = json.loads(
        (ROOT / "tools/platform-release/authority.json").read_text(encoding="utf-8")
    )
    require(target in authority.get("targets", {}), f"release authority omits {target}")
    policy = json.loads(
        (ROOT / "tools/platform-release/cpu-policy.json").read_text(encoding="utf-8")
    ).get("targets", {}).get(target, {}).get("cmakeEnvironment")
    require(isinstance(policy, dict), f"CPU authority omits {target}")
    require(policy.get("GGML_NATIVE") == "OFF", f"{target} enables host-native CPU code")
    if target.startswith("x86_64"):
        require(
            policy.get("GGML_CPU_ALL_VARIANTS") == "ON",
            f"{target} no longer builds safe runtime CPU variants",
        )
        require(
            all(
                value == ("ON" if key == "GGML_CPU_ALL_VARIANTS" else "OFF")
                for key, value in policy.items()
            ),
            f"{target} baseline CPU policy enables an undeclared extension",
        )


def validate_workflow_split(target: str) -> None:
    release = (ROOT / ".github/workflows/platform-modular-release.yml").read_text(
        encoding="utf-8"
    )
    require(release.count(f"target: {target}") == 1, f"release matrix must contain {target} once")
    for command in (
        "tools/portable-release/assemble.py build",
        "tools/portable-release/assemble.py verify",
        "tools/portable-release/native_acceptance.py",
    ):
        require(command in release, f"full release evidence lost command: {command}")

    fast = (ROOT / ".github/workflows/pr-fast-gate.yml").read_text(encoding="utf-8")
    jobs = re.findall(r"(?m)^  [a-z][a-z0-9-]*:\n    name:", fast)
    timeouts = [int(value) for value in re.findall(r"timeout-minutes: (\d+)", fast)]
    require(len(jobs) == 4, "PR gate must expose exactly four jobs")
    require(len(timeouts) == 4 and max(timeouts) <= 5, "every PR job must be bounded to five minutes")
    for forbidden in (
        "--features media-runtime",
        "-p into-markdown-cli --bin into-md",
        "--test runtime",
    ):
        require(forbidden not in fast, f"PR gate restored a release-only Cargo graph: {forbidden}")
    require(
        fast.count("-p into-markdown-process-plugin --lib") == 4,
        "every runner must compile or test the native process boundary",
    )


def validate(target: str, *, system: str | None = None, machine: str | None = None) -> None:
    require(target in SUPPORTED, f"unsupported PR gate target: {target}")
    validate_host(target, system or platform.system(), machine or platform.machine())
    validate_workflows(ROOT)
    validate_feature_chain()
    validate_target_authority(target)
    validate_workflow_split(target)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True, choices=sorted(SUPPORTED))
    args = parser.parse_args()
    try:
        validate(args.target)
    except (ContractError, WorkflowPolicyError) as error:
        print(f"PR gate contract failed: {error}", file=sys.stderr)
        return 1
    print(f"PR gate contract passed for native {args.target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
