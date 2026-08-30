"""Black-box PDFium scenarios for authenticated published release layouts."""

from __future__ import annotations

import json
import os
import pathlib
import shutil
import subprocess
import time
from collections.abc import Callable
from typing import Any

from release_artifacts import E2EError


def _windows_to_wsl(path: pathlib.Path) -> str:
    result = subprocess.run(
        ["wsl.exe", "--exec", "wslpath", "-a", str(path.resolve())],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if result.returncode != 0 or not result.stdout.strip().startswith("/"):
        raise E2EError(f"cannot translate path for WSL: {result.stderr.strip()}")
    return result.stdout.strip()


def run_wsl(arguments: Any, assets: pathlib.Path, fixtures: pathlib.Path) -> dict:
    """Run the Linux half of the release suite from a Windows aggregate job."""
    script = _windows_to_wsl(pathlib.Path(__file__).with_name("post_release_e2e.py"))
    report_path = arguments.work_root / "linux-x86_64.json"
    command = [
        "wsl.exe",
        "--exec",
        "python3",
        script,
        "--platform",
        "linux",
        "--assets-dir",
        _windows_to_wsl(assets),
        "--fixtures",
        _windows_to_wsl(fixtures),
        "--evidence-dir",
        _windows_to_wsl(arguments.evidence_dir.resolve(strict=True)),
        "--work-root",
        f"/tmp/into-md-post-release-e2e-{os.getpid()}",
        "--report",
        _windows_to_wsl(report_path),
        "--version",
        arguments.version,
        "--repository",
        arguments.repository,
        "--tag",
        arguments.tag,
    ]
    started = time.monotonic()
    result = subprocess.run(
        command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False
    )
    detail = (result.stderr or result.stdout)[: 64 * 1024].decode(
        "utf-8", errors="replace"
    ).strip()
    if not report_path.is_file():
        raise E2EError(f"WSL Linux E2E did not write its report: {detail}")
    envelope = json.loads(report_path.read_text(encoding="utf-8"))
    reports = envelope.get("platforms", [])
    if len(reports) != 1:
        raise E2EError("WSL Linux E2E report does not contain one platform result")
    report = reports[0]
    report["wslInvocationElapsedMs"] = round((time.monotonic() - started) * 1000)
    if result.returncode != 0 and report.get("conclusion") == "passed":
        raise E2EError(f"WSL Linux E2E process failed: {detail}")
    return report


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
) -> tuple[list[pathlib.Path], int, int, bytes]:
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
    return runtimes, expected, case.elapsed_ms, output.read_bytes()


def run_skill_packaged_runtime(
    skill: pathlib.Path,
    platform: str,
    fixtures: pathlib.Path,
    root: pathlib.Path,
    expected_all_runtimes: int,
    expected_version: str,
    expected_pdf_output: bytes,
    protect_directory: Callable[[pathlib.Path, str], pathlib.Path],
    isolated_environment: Callable[
        [pathlib.Path, str], tuple[dict[str, str], pathlib.Path]
    ],
    runner_factory: Callable[[pathlib.Path, dict[str, str], pathlib.Path], Any],
    copy_fixture: Callable[[pathlib.Path, str, pathlib.Path], pathlib.Path],
    conversion_arguments: Callable[[pathlib.Path, pathlib.Path, list[str]], list[str]],
    assert_output: Callable[[pathlib.Path, str], None],
    runtime_directories: Callable[[dict[str, str], str], list[pathlib.Path]],
) -> list[dict[str, Any]]:
    """Prove the Skill layout converts PDF/OCR with an empty search path."""
    skill_root = protect_directory(root / "skill-state", platform)
    skill_work = protect_directory(skill_root / "work", platform)
    environment, _user_data = isolated_environment(skill_root / "state", platform)
    environment["PATH"] = ""
    runner = runner_factory(skill, environment, skill_work)
    version = runner.call("skill-version-empty-path", ["version", "--json", "--no-config"])
    try:
        version_payload = json.loads(version.stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise E2EError("Skill version output is not JSON") from error
    if version_payload.get("name") != "into-md" or version_payload.get("version") != expected_version:
        raise E2EError("Skill version does not match the requested release")
    text = copy_fixture(fixtures, "small/text/normal.txt", skill_work / "normal.txt")
    text_output = skill_work / "normal.md"
    runner.call(
        "skill-text-empty-path",
        conversion_arguments(text, text_output, ["--no-config"]),
    )
    assert_output(text_output, "Skill plain text")
    source_text = text.read_text(encoding="utf-8").strip()
    if source_text and source_text not in text_output.read_text(encoding="utf-8"):
        raise E2EError("Skill text output does not contain the fixture authority text")
    pdf = copy_fixture(
        fixtures, "small/pdf/structures.pdf", skill_work / "structures.pdf"
    )
    pdf_output = skill_work / "structures.md"
    runner.call(
        "skill-pdf-empty-path",
        conversion_arguments(
            pdf, pdf_output, ["--ocr", "off", "--no-config"]
        ),
    )
    assert_output(pdf_output, "Skill PDF")
    if pdf_output.read_bytes() != expected_pdf_output:
        raise E2EError("Skill PDF output differs from the authenticated Core output")
    ocr = copy_fixture(
        fixtures, "small/ocr/ocr-english-clear-1.png", skill_work / "ocr.png"
    )
    ocr_output = skill_work / "ocr.md"
    runner.call(
        "skill-ocr-empty-path",
        conversion_arguments(
            ocr, ocr_output, ["--ocr", "always", "--no-config"]
        ),
    )
    assert_output(ocr_output, "Skill OCR")
    if "clear scans verify document conversion quality" not in ocr_output.read_text(
        encoding="utf-8"
    ).lower():
        raise E2EError("Skill OCR output does not contain the fixture authority text")
    if len(runtime_directories(environment, platform)) != expected_all_runtimes:
        raise E2EError(
            "Skill empty-PATH PDF/OCR did not use the expected packaged/embedded runtimes"
        )
    return runner.cases


def run_packaged_pdfium_negative_cases(
    binary: pathlib.Path,
    fixtures: pathlib.Path,
    root: pathlib.Path,
    platform: str,
    protect_directory: Callable,
    isolated_environment: Callable,
    copy_fixture: Callable,
    runner_factory: Callable,
    conversion_arguments: Callable,
    bounded: Callable[[bytes], str],
) -> list[dict[str, Any]]:
    """Prove missing, tampered, and reparse packaged PDFium fail closed on Windows."""
    if platform != "windows":
        return []
    source_runtime = binary.parent / "lib/pdfium/pdfium.dll"
    if not source_runtime.is_file() or source_runtime.is_symlink():
        raise E2EError("authenticated packaged PDFium fixture is unavailable")
    results = []
    for scenario in ("missing", "tampered", "reparse"):
        scenario_root = protect_directory(root / scenario, platform)
        layout = protect_directory(scenario_root / "layout", platform)
        work = protect_directory(scenario_root / "work", platform)
        copied_binary = layout / binary.name
        shutil.copyfile(binary, copied_binary)
        runtime = layout / "lib/pdfium/pdfium.dll"
        if scenario == "tampered":
            runtime.parent.mkdir(parents=True)
            data = bytearray(source_runtime.read_bytes())
            if not data:
                raise E2EError("authenticated packaged PDFium fixture is empty")
            data[0] ^= 0x80
            runtime.write_bytes(data)
        elif scenario == "reparse":
            reparse_target = scenario_root / "outside"
            create_runtime_reparse(runtime.parent, reparse_target, platform)
            shutil.copyfile(source_runtime, reparse_target / runtime.name)
        environment, _user_data = isolated_environment(
            scenario_root / "state", platform
        )
        decoy = protect_directory(scenario_root / "path-decoy", platform)
        shutil.copyfile(source_runtime, decoy / "pdfium.dll")
        shutil.copyfile(source_runtime, work / "pdfium.dll")
        environment["PATH"] = str(decoy)
        source = copy_fixture(
            fixtures, "small/pdf/structures.pdf", work / "structures.pdf"
        )
        output = work / "structures.md"
        runner = runner_factory(copied_binary, environment, work)
        result = runner.call(
            f"packaged-pdfium-{scenario}",
            conversion_arguments(source, output, ["--ocr", "off", "--no-config"]),
            succeed=False,
        )
        detail = bounded(result.stderr + result.stdout).lower().replace("_", "")
        if "componentunavailable" not in detail:
            raise E2EError(
                f"packaged PDFium {scenario} did not fail as componentUnavailable"
            )
        if output.exists():
            raise E2EError(f"packaged PDFium {scenario} published an output")
        results.extend(runner.cases)
    return results
