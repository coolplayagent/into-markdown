"""Black-box PDFium scenarios for authenticated published release layouts."""

from __future__ import annotations

import pathlib
import subprocess
import time
from collections.abc import Callable
from typing import Any

from release_artifacts import E2EError


def run_concurrent_ocr(
    binary: pathlib.Path,
    fixtures: pathlib.Path,
    root: pathlib.Path,
    platform: str,
    isolated_environment: Callable,
    protect_directory: Callable,
    copy_fixture: Callable,
    conversion_arguments: Callable,
    bounded: Callable[[bytes], str],
    assert_output: Callable,
    sha256_file: Callable,
    runtime_directories: Callable,
) -> dict[str, Any]:
    """Exercise two concurrent first-use OCR processes against isolated state."""
    environment, _user_data = isolated_environment(root, platform)
    work = protect_directory(root / "work", platform)
    source = copy_fixture(
        fixtures, "small/ocr/ocr-english-clear-1.png", work / "ocr.png"
    )
    commands = []
    started = time.monotonic()
    for index in range(2):
        process_work = protect_directory(work / f"process-{index}", platform)
        output = process_work / "result.md"
        commands.append(
            (
                output,
                subprocess.Popen(
                    [
                        str(binary),
                        *conversion_arguments(
                            source,
                            output,
                            ["--ocr", "always", "--no-config"],
                        ),
                    ],
                    cwd=process_work,
                    env=environment,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                ),
            )
        )
    results = []
    for output, process in commands:
        stdout, stderr = process.communicate(timeout=120)
        if process.returncode != 0:
            raise E2EError(f"concurrent OCR failed: {bounded(stderr or stdout).strip()}")
        assert_output(output, "concurrent OCR")
        results.append(
            {"exitCode": process.returncode, "outputSha256": sha256_file(output)}
        )
    directories = runtime_directories(environment, platform)
    if len(directories) != 1:
        raise E2EError("concurrent first OCR did not publish exactly one runtime")
    return {
        "elapsedMs": round((time.monotonic() - started) * 1000),
        "processes": results,
    }


def create_runtime_reparse(
    path: pathlib.Path, target: pathlib.Path, platform: str
) -> None:
    """Create the platform link fixture used to prove cache fallback safety."""
    target.mkdir(parents=True, exist_ok=False)
    path.parent.mkdir(parents=True, exist_ok=True)
    if platform == "windows":
        result = subprocess.run(
            ["cmd.exe", "/d", "/c", "mklink", "/J", str(path), str(target)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            raise E2EError(
                f"cannot create Windows runtime reparse fixture: {result.stderr.strip()}"
            )
    else:
        path.symlink_to(target, target_is_directory=True)


def run_fallback_matrix(
    binary: pathlib.Path,
    fixtures: pathlib.Path,
    root: pathlib.Path,
    platform: str,
    residual_report: list[dict[str, Any]],
    invariant_errors: list[str],
    protect_directory: Callable,
    isolated_environment: Callable,
    copy_fixture: Callable,
    runner_factory: Callable,
    conversion_arguments: Callable,
    assert_output: Callable,
    record_and_clean_residuals: Callable,
) -> list[dict[str, Any]]:
    """Verify authenticated fallback when the preferred cache is unusable or linked."""
    results = []
    for scenario in ("unavailable-cache", "reparse-cache"):
        scenario_root = protect_directory(root / scenario, platform)
        environment, _user_data = isolated_environment(
            scenario_root / "state", platform
        )
        work = protect_directory(scenario_root / "work", platform)
        source = copy_fixture(
            fixtures, "small/ocr/ocr-english-clear-1.png", work / "ocr.png"
        )
        cache = pathlib.Path(
            environment[
                "LOCALAPPDATA" if platform == "windows" else "XDG_CACHE_HOME"
            ]
        )
        if scenario == "unavailable-cache":
            blocked = cache / "into-markdown" / "runtime"
            blocked.parent.mkdir(parents=True, exist_ok=True)
            blocked.write_bytes(b"not a directory")
        else:
            create_runtime_reparse(
                cache / "into-markdown" / "runtime",
                scenario_root / "reparse-target",
                platform,
            )
        output = work / "result.md"
        runner = runner_factory(binary, environment, work)
        scenario_result: dict[str, Any] = {
            "scenario": scenario,
            "conclusion": "failed",
        }
        try:
            result = runner.call(
                scenario,
                conversion_arguments(
                    source, output, ["--ocr", "always", "--no-config"]
                ),
            )
            assert_output(output, scenario)
            scenario_result.update(
                {"elapsedMs": result.elapsed_ms, "conclusion": "passed"}
            )
        except Exception as error:
            scenario_result["error"] = str(error)
            invariant_errors.append(
                f"{scenario} did not preserve OCR availability: {error}"
            )
        finally:
            record_and_clean_residuals(
                environment,
                "into-markdown-runtime-*",
                scenario,
                residual_report,
                invariant_errors,
            )
        results.append(scenario_result)
    return results


def run_core_pdf(
    runner: Any,
    platform: str,
    fixtures: pathlib.Path,
    work: pathlib.Path,
    environment: dict[str, str],
    copy_fixture: Callable[[pathlib.Path, str, pathlib.Path], pathlib.Path],
    conversion_arguments: Callable[[pathlib.Path, pathlib.Path, list[str]], list[str]],
    assert_output: Callable[[pathlib.Path, str], None],
    runtime_directories: Callable[[dict[str, str], str], list[pathlib.Path]],
) -> tuple[list[pathlib.Path], int, int]:
    """Run the first Core PDF conversion and return its runtime observations."""
    source = copy_fixture(fixtures, "small/pdf/structures.pdf", work / "structures.pdf")
    output = work / "structures.md"
    case = runner.call(
        "pdf-first-materialization",
        conversion_arguments(source, output, ["--ocr", "off", "--no-config"]),
    )
    assert_output(output, "PDF")
    runtimes = runtime_directories(environment, platform)
    expected = 0 if platform == "windows" else 1
    if len(runtimes) != expected:
        raise E2EError(
            "first PDF did not use the expected packaged or embedded PDFium runtime"
        )
    return runtimes, expected, case.elapsed_ms


def run_skill_packaged_runtime(
    skill: pathlib.Path,
    platform: str,
    fixtures: pathlib.Path,
    root: pathlib.Path,
    expected_all_runtimes: int,
    protect_directory: Callable[[pathlib.Path, str], pathlib.Path],
    isolated_environment: Callable[
        [pathlib.Path, str], tuple[dict[str, str], pathlib.Path]
    ],
    runner_factory: Callable[[pathlib.Path, dict[str, str], pathlib.Path], Any],
    copy_fixture: Callable[[pathlib.Path, str, pathlib.Path], pathlib.Path],
    conversion_arguments: Callable[[pathlib.Path, pathlib.Path, list[str]], list[str]],
    runtime_directories: Callable[[dict[str, str], str], list[pathlib.Path]],
) -> list[dict[str, Any]]:
    """Prove the Skill layout converts PDF/OCR with an empty search path."""
    skill_root = protect_directory(root / "skill-state", platform)
    skill_work = protect_directory(skill_root / "work", platform)
    environment, _user_data = isolated_environment(skill_root / "state", platform)
    environment["PATH"] = ""
    runner = runner_factory(skill, environment, skill_work)
    runner.call("skill-version-empty-path", ["version", "--json", "--no-config"])
    text = copy_fixture(fixtures, "small/text/normal.txt", skill_work / "normal.txt")
    runner.call(
        "skill-text-empty-path",
        conversion_arguments(text, skill_work / "normal.md", ["--no-config"]),
    )
    pdf = copy_fixture(
        fixtures, "small/pdf/structures.pdf", skill_work / "structures.pdf"
    )
    runner.call(
        "skill-pdf-empty-path",
        conversion_arguments(
            pdf, skill_work / "structures.md", ["--ocr", "off", "--no-config"]
        ),
    )
    ocr = copy_fixture(
        fixtures, "small/ocr/ocr-english-clear-1.png", skill_work / "ocr.png"
    )
    runner.call(
        "skill-ocr-empty-path",
        conversion_arguments(
            ocr, skill_work / "ocr.md", ["--ocr", "always", "--no-config"]
        ),
    )
    if len(runtime_directories(environment, platform)) != expected_all_runtimes:
        raise E2EError(
            "Skill empty-PATH PDF/OCR did not use the expected packaged/embedded runtimes"
        )
    return runner.cases
