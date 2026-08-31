"""Enforce the reviewed four-job CI topology without a YAML dependency.

Workflow topology uses a deliberately fixed block layout. Unsupported YAML
spellings fail closed; step bodies remain editable for focused unit tests.
"""

from __future__ import annotations

import pathlib
import re


WORKFLOWS = {"pr-fast-gate.yml", "platform-modular-release.yml"}
FAST_JOBS = {
    "linux-x86-64": ("Linux x86_64, shared tests, and Web", "ubuntu-24.04"),
    "linux-arm64": ("Linux ARM64 Core", "ubuntu-24.04-arm"),
    "windows-x86-64": ("Windows x86_64 Core", "windows-2025"),
    "macos-arm64": ("macOS ARM64 Core", "macos-14"),
}


class WorkflowPolicyError(RuntimeError):
    """CI topology exceeds the approved four fast jobs."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise WorkflowPolicyError(message)


def blocks(text: str, indent: int) -> dict[str, list[str]]:
    """Read canonical block headers, rejecting duplicate keys and aliases."""
    result: dict[str, list[str]] = {}
    current: list[str] | None = None
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        require("\t" not in line[:len(line) - len(line.lstrip())], "workflow indentation uses tabs")
        depth = len(line) - len(line.lstrip(" "))
        if depth == indent:
            match = re.fullmatch(r"([a-z][a-z0-9-]*):(?: .*)?", line[indent:])
            require(match is not None, f"unsupported workflow block: {line.strip()}")
            key = match.group(1)
            require(key not in result, f"duplicate workflow block: {key}")
            current = result[key] = [line]
        else:
            require(depth > indent and current is not None, f"invalid workflow nesting: {line}")
            current.append(line)
    return result


def validate_fast(text: str) -> None:
    sections = blocks(text, 0)
    require(set(sections) == {"name", "on", "concurrency", "permissions", "env", "jobs"},
            "PR fast gate root fields must retain the approved topology")
    require(sections["name"] == ["name: PR fast gate"], "PR workflow name must remain PR fast gate")
    require(sections["on"] == ["on:", "  pull_request:"], "PR CI permits only unfiltered pull_request")
    require(sections["jobs"][0] == "jobs:", "PR jobs must use canonical block layout")
    jobs = blocks("\n".join(sections["jobs"][1:]), 2)
    require(set(jobs) == set(FAST_JOBS), "PR CI permits exactly the four approved fast jobs")
    for key, (name, runner) in FAST_JOBS.items():
        lines = jobs[key]
        require(lines[:3] == [f"  {key}:", f"    name: {name}", f"    runs-on: {runner}"],
                f"{key} must retain its approved name and native runner")
        require(len(lines) > 5 and lines[3] == "    timeout-minutes: 5",
                f"{key} must retain its five-minute timeout")
        require(lines[4] == "    steps:" and all(line.startswith("      ") for line in lines[5:]),
                f"{key} permits step changes only; matrices, extra job fields and reusable jobs are forbidden")
        require(any(line.startswith("      - ") for line in lines[5:]), f"{key} requires steps")
        require(any("python" in line and "tools/platform-release/pr_fast_gate.py --target " in line
                    for line in lines[5:] if not line.lstrip().startswith("#")),
                f"{key} must retain its existing policy validator invocation")


def validate_workflows(root: pathlib.Path) -> None:
    directory = root / ".github/workflows"
    require(directory.is_dir() and not directory.is_symlink(), "workflow directory must be local")
    entries = list(directory.iterdir())
    require({path.name for path in entries} == WORKFLOWS,
            "workflow allowlist permits only pr-fast-gate.yml and manual platform-modular-release.yml")
    require(all(path.is_file() and not path.is_symlink() for path in entries),
            "workflows must be regular files")
    validate_fast((directory / "pr-fast-gate.yml").read_text(encoding="utf-8"))
    release = blocks((directory / "platform-modular-release.yml").read_text(encoding="utf-8"), 0)
    trigger = release.get("on", [])
    require(trigger[:2] == ["on:", "  workflow_dispatch:"]
            and all(line.startswith("    ") for line in trigger[2:]),
            "release workflow must remain manual workflow_dispatch only")
