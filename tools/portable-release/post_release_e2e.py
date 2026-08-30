#!/usr/bin/env python3
"""Run isolated black-box checks against downloaded or local release assets."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import shutil
import stat
import subprocess
import sys
import time
import zipfile
from dataclasses import dataclass
from typing import Any


REPOSITORY = "coolplayagent/into-markdown"
PORTABLE_RELEASE_DIR = pathlib.Path(__file__).resolve().parent
if str(PORTABLE_RELEASE_DIR) not in sys.path:
    sys.path.insert(0, str(PORTABLE_RELEASE_DIR))
from release_artifacts import (  # noqa: E402
    CORE_ARCHIVE_MANIFEST,
    CORE_MATERIAL_MEMBERS,
    PDFIUM_LICENSE_FILES,
    ROOT,
    SKILL_ARCHIVE,
    SKILL_DIRECTORIES,
    SKILL_FILES,
    SKILL_MANIFEST,
    TARGETS,
    WINDOWS_PDFIUM_AUTHORITY,
    WINDOWS_PDFIUM_MEMBER,
    WINDOWS_SKILL_PDFIUM,
    E2EError,
    acquire_assets,
    extract_single_core as _extract_single_core,
    extract_skill_binary as _extract_skill_binary,
    inspect_core,
    release_asset_url,
    sha256_file,
)
from post_release_scenarios import (  # noqa: E402
    run_core_pdf,
    run_concurrent_ocr,
    run_fallback_matrix,
    run_skill_packaged_runtime,
)

MAX_CAPTURE_BYTES = 64 * 1024


def extract_single_core(
    archive_path: pathlib.Path, platform: str, output: pathlib.Path
) -> dict:
    return _extract_single_core(
        archive_path, platform, output, WINDOWS_PDFIUM_AUTHORITY
    )


def extract_skill_binary(
    archive_path: pathlib.Path, platform: str, output: pathlib.Path
) -> dict:
    return _extract_skill_binary(
        archive_path, platform, output, WINDOWS_PDFIUM_AUTHORITY
    )


def _bounded(value: bytes) -> str:
    return value[:MAX_CAPTURE_BYTES].decode("utf-8", errors="replace")


def write_json(path: pathlib.Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )


def protect_directory(path: pathlib.Path, platform: str) -> pathlib.Path:
    path.mkdir(parents=True, exist_ok=False)
    if platform == "windows":
        whoami = subprocess.run(
            ["whoami.exe", "/user", "/fo", "csv", "/nh"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if whoami.returncode != 0:
            raise E2EError(f"cannot resolve Windows SID: {whoami.stderr.strip()}")
        columns = next(__import__("csv").reader([whoami.stdout.strip()]))
        if len(columns) < 2 or not columns[1].startswith("S-"):
            raise E2EError("whoami returned an invalid Windows SID")
        result = subprocess.run(
            [
                "icacls.exe",
                str(path),
                "/inheritance:r",
                "/grant:r",
                f"*{columns[1]}:(OI)(CI)F",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            raise E2EError(f"cannot protect Windows state: {result.stderr.strip()}")
    else:
        path.chmod(0o700)
    return path.resolve(strict=True)


@dataclass
class CommandResult:
    name: str
    elapsed_ms: int
    exit_code: int
    stdout: bytes
    stderr: bytes

    def record(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "elapsedMs": self.elapsed_ms,
            "exitCode": self.exit_code,
            "stdoutSha256": hashlib.sha256(self.stdout).hexdigest(),
            "stderrSha256": hashlib.sha256(self.stderr).hexdigest(),
        }


class Runner:
    def __init__(self, binary: pathlib.Path, environment: dict[str, str], work: pathlib.Path):
        self.binary = binary
        self.environment = environment
        self.work = work
        self.cases: list[dict[str, Any]] = []

    def call(
        self,
        name: str,
        arguments: list[str],
        *,
        succeed: bool = True,
        timeout: int = 120,
        environment: dict[str, str] | None = None,
    ) -> CommandResult:
        started = time.monotonic()
        result = subprocess.run(
            [str(self.binary), *arguments],
            cwd=self.work,
            env=environment or self.environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )
        command = CommandResult(
            name,
            round((time.monotonic() - started) * 1000),
            result.returncode,
            result.stdout,
            result.stderr,
        )
        self.cases.append(command.record())
        if (result.returncode == 0) != succeed:
            detail = _bounded(result.stderr or result.stdout).strip()
            expectation = "succeed" if succeed else "fail"
            raise E2EError(f"{name} was expected to {expectation}: {detail}")
        return command


def runtime_directories(environment: dict[str, str], platform: str) -> list[pathlib.Path]:
    if platform == "windows":
        root = pathlib.Path(environment["LOCALAPPDATA"])
    else:
        root = pathlib.Path(environment["XDG_CACHE_HOME"])
    runtime = root / "into-markdown" / "runtime"
    if not runtime.exists():
        return []
    return sorted(path for path in runtime.iterdir() if path.is_dir())


def dispatch_directories(environment: dict[str, str]) -> list[pathlib.Path]:
    temporary = pathlib.Path(environment["TEMP"])
    return sorted(
        path
        for path in temporary.rglob("into-md-plugin-dispatch-*")
        if path.is_dir() and not path.is_symlink()
    )


def assert_dispatch_clean(environment: dict[str, str], label: str) -> None:
    residual = dispatch_directories(environment)
    if residual:
        sizes = []
        for path in residual:
            size = sum(item.stat().st_size for item in path.rglob("*") if item.is_file())
            sizes.append(f"{path.name} ({size} bytes)")
        raise E2EError(f"{label} left plugin dispatch snapshots: {', '.join(sizes)}")


def residual_records(environment: dict[str, str], pattern: str) -> list[dict[str, Any]]:
    temporary = pathlib.Path(environment["TEMP"])
    records = []
    for path in sorted(temporary.rglob(pattern)):
        if not path.is_dir() or path.is_symlink():
            continue
        size = sum(item.stat().st_size for item in path.rglob("*") if item.is_file())
        records.append(
            {
                "path": str(path.relative_to(temporary)),
                "bytes": size,
            }
        )
    return records


def record_and_clean_residuals(
    environment: dict[str, str],
    pattern: str,
    stage: str,
    report: list[dict[str, Any]],
    errors: list[str],
) -> None:
    records = residual_records(environment, pattern)
    report.append({"stage": stage, "directories": records})
    if records:
        errors.append(
            f"{stage} left {len(records)} {pattern} directories "
            f"({sum(record['bytes'] for record in records)} bytes)"
        )
        temporary = pathlib.Path(environment["TEMP"])
        for record in records:
            shutil.rmtree(temporary / record["path"])


def conversion_arguments(source: pathlib.Path, output: pathlib.Path, extra: list[str]) -> list[str]:
    return [
        str(source),
        "-o",
        str(output),
        "--conflict",
        "error",
        "--progress",
        "never",
        *extra,
    ]


def assert_output(path: pathlib.Path, label: str) -> None:
    if not path.is_file() or not path.read_bytes():
        raise E2EError(f"{label} output is missing or empty")


def plugin_identity(package: pathlib.Path) -> dict[str, str]:
    with zipfile.ZipFile(package) as archive:
        infos = archive.infolist()
        if len({info.filename for info in infos}) != len(infos):
            raise E2EError("speech package contains duplicate paths")
        forbidden = ("source/", "relink/", "sbom", "sources.json", "notice", "license")
        lowered = [info.filename.lower() for info in infos]
        if any(any(token in name for token in forbidden) for name in lowered):
            raise E2EError("speech package contains audit-only material")
        try:
            manifest = json.loads(archive.read("plugin.json"))
        except (KeyError, json.JSONDecodeError, UnicodeDecodeError) as error:
            raise E2EError("speech package has no valid plugin.json") from error
    signature = manifest.get("signature", {})
    if manifest.get("id") != "official.media.whisper":
        raise E2EError("speech package has the wrong plugin identity")
    key_id = signature.get("keyId")
    fingerprint = signature.get("publicKeySha256")
    if not isinstance(key_id, str) or not isinstance(fingerprint, str) or len(fingerprint) != 64:
        raise E2EError("speech package has an invalid publisher identity")
    return {"signingKeyId": key_id, "signingKeySha256": fingerprint}


def install_plugin(runner: Runner, package: pathlib.Path, identity: dict[str, str], name: str) -> None:
    runner.call(
        name,
        [
            "plugins",
            "install",
            str(package),
            "--sha256",
            sha256_file(package),
            "--signing-key-id",
            identity["signingKeyId"],
            "--signing-key-sha256",
            identity["signingKeySha256"],
            "--scope",
            "global",
        ],
    )


def corrupt_installed_plugin(user_data: pathlib.Path) -> pathlib.Path:
    plugin_root = user_data / "into-markdown" / "plugins" / "official.media.whisper"
    candidates = sorted(plugin_root.rglob("into-md-media-provider*"))
    candidates = [path for path in candidates if path.is_file() and not path.is_symlink()]
    if len(candidates) != 1:
        raise E2EError("could not identify one installed speech provider to corrupt")
    path = candidates[0]
    path.chmod(path.stat().st_mode | stat.S_IWRITE | stat.S_IWUSR)
    with path.open("r+b") as value:
        byte = value.read(1)
        if not byte:
            raise E2EError("installed speech provider is empty")
        value.seek(0)
        value.write(bytes([byte[0] ^ 0x80]))
    return path


def corrupt_runtime(path: pathlib.Path) -> tuple[pathlib.Path, str]:
    candidates = sorted(
        item
        for item in path.rglob("*")
        if item.is_file() and not item.is_symlink() and item.stat().st_size > 0
    )
    if not candidates:
        raise E2EError("OCR runtime contains no corruptible file")
    candidate = candidates[0]
    expected = sha256_file(candidate)
    candidate.chmod(candidate.stat().st_mode | stat.S_IWRITE | stat.S_IWUSR)
    with candidate.open("r+b") as value:
        byte = value.read(1)
        value.seek(0)
        value.write(bytes([byte[0] ^ 0x80]))
    return candidate, expected


def _copy_fixture(fixtures: pathlib.Path, relative: str, destination: pathlib.Path) -> pathlib.Path:
    source = fixtures / relative
    if not source.is_file():
        raise E2EError(f"fixture is unavailable: {relative}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    return destination


def _isolated_environment(root: pathlib.Path, platform: str) -> tuple[dict[str, str], pathlib.Path]:
    home = protect_directory(root / "home", platform)
    cache = protect_directory(root / "cache", platform)
    temporary = protect_directory(root / "tmp", platform)
    user_data = protect_directory(root / "user-data", platform)
    environment = os.environ.copy()
    environment.update(
        {
            "HOME": str(home),
            "USERPROFILE": str(home),
            "LOCALAPPDATA": str(cache),
            "APPDATA": str(root / "appdata"),
            "XDG_CACHE_HOME": str(cache),
            "XDG_CONFIG_HOME": str(root / "config"),
            "TMP": str(temporary),
            "TEMP": str(temporary),
            "TMPDIR": str(temporary),
            "INTO_MARKDOWN_USER_DATA_HOME": str(user_data),
            "NO_PROXY": "*",
            "no_proxy": "*",
        }
    )
    return environment, user_data


def run_platform(
    platform: str,
    assets: pathlib.Path,
    fixtures: pathlib.Path,
    work_root: pathlib.Path,
    version: str,
) -> dict[str, Any]:
    config = TARGETS[platform]
    started = time.monotonic()
    conclusion = "failed"
    error = None
    report: dict[str, Any] = {
        "schemaVersion": 1,
        "platform": platform,
        "target": config["target"],
        "version": version,
        "cases": [],
        "timingsMs": {},
    }
    try:
        root = protect_directory(work_root, platform)
        invariant_errors: list[str] = []
        dispatch_residuals: list[dict[str, Any]] = []
        fallback_residuals: list[dict[str, Any]] = []
        core_root = protect_directory(root / "core", platform)
        skill_asset_root = protect_directory(root / "skill", platform)
        work = protect_directory(root / "work", platform)
        core = core_root / config["member"]
        report["core"] = extract_single_core(assets / config["core"], platform, core)
        skill = skill_asset_root / ("skill.exe" if platform == "windows" else "skill")
        report["skill"] = extract_skill_binary(assets / SKILL_ARCHIVE, platform, skill)
        package_source = assets / config["speech"]
        package = work / config["speech"]
        shutil.copyfile(package_source, package)
        identity = plugin_identity(package)
        report["speech"] = {
            "asset": package_source.name,
            "sha256": sha256_file(package_source),
            "publisher": identity,
        }
        environment, user_data = _isolated_environment(root / "state", platform)
        runner = Runner(core, environment, work)
        help_result = runner.call("help", ["-h"])
        if help_result.elapsed_ms > 1000:
            raise E2EError(f"cold help exceeded 1000 ms: {help_result.elapsed_ms} ms")
        version_result = runner.call("version", ["version", "--json", "--no-config"])
        payload = json.loads(version_result.stdout)
        if payload.get("name") != "into-md" or payload.get("version") != version:
            raise E2EError("Core version does not match the requested release")
        text = _copy_fixture(fixtures, "small/text/normal.txt", work / "normal.txt")
        text_output = work / "normal.md"
        runner.call(
            "plain-text",
            conversion_arguments(text, text_output, ["--no-config"]),
        )
        assert_output(text_output, "plain text")
        for kind in ("docx", "pptx", "xlsx"):
            source = _copy_fixture(fixtures, f"small/{kind}/normal.{kind}", work / f"normal.{kind}")
            output = work / f"normal-{kind}.md"
            runner.call(
                f"ooxml-{kind}-ocr-off",
                conversion_arguments(source, output, ["--ocr", "off", "--no-config"]),
            )
            assert_output(output, f"OOXML {kind}")
        if runtime_directories(environment, platform):
            raise E2EError("help/version/text/OOXML --ocr off created a runtime cache")

        pdf_runtimes, expected_pdf_runtimes, pdf_elapsed_ms = run_core_pdf(
            runner,
            platform,
            fixtures,
            work,
            environment,
            _copy_fixture,
            conversion_arguments,
            assert_output,
            runtime_directories,
        )
        report["timingsMs"]["pdfFirstMaterialization"] = pdf_elapsed_ms

        ocr = _copy_fixture(
            fixtures,
            "small/ocr/ocr-english-clear-1.png",
            work / "ocr-english-clear-1.png",
        )
        ocr_output = work / "ocr.md"
        cold = runner.call(
            "ocr-cold",
            conversion_arguments(ocr, ocr_output, ["--ocr", "always", "--no-config"]),
        )
        assert_output(ocr_output, "OCR")
        if "clear scans verify document conversion quality" not in ocr_output.read_text(
            encoding="utf-8"
        ).lower():
            raise E2EError("OCR output does not contain the fixture authority text")
        all_runtimes = runtime_directories(environment, platform)
        expected_all_runtimes = expected_pdf_runtimes + 1
        if len(all_runtimes) != expected_all_runtimes:
            raise E2EError("first OCR did not add exactly one runtime")
        ocr_runtime = next(path for path in all_runtimes if path not in pdf_runtimes)
        hot_output = work / "ocr-hot.md"
        hot = runner.call(
            "ocr-hot-cache-reuse",
            conversion_arguments(ocr, hot_output, ["--ocr", "always", "--no-config"]),
        )
        if len(runtime_directories(environment, platform)) != expected_all_runtimes:
            raise E2EError("hot OCR created a redundant runtime")
        corrupted, expected_hash = corrupt_runtime(ocr_runtime)
        repaired_output = work / "ocr-repaired.md"
        repair = runner.call(
            "ocr-corrupt-cache-repair",
            conversion_arguments(ocr, repaired_output, ["--ocr", "always", "--no-config"]),
        )
        if not corrupted.is_file() or sha256_file(corrupted) != expected_hash:
            raise E2EError("OCR cache corruption was not repaired to its authenticated bytes")
        report["timingsMs"].update(
            {"ocrCold": cold.elapsed_ms, "ocrHot": hot.elapsed_ms, "ocrRepair": repair.elapsed_ms}
        )
        report["concurrentFirstOcr"] = run_concurrent_ocr(
            core,
            fixtures,
            root / "concurrent",
            platform,
            _isolated_environment,
            protect_directory,
            _copy_fixture,
            conversion_arguments,
            _bounded,
            assert_output,
            sha256_file,
            runtime_directories,
        )
        report["fallbackCache"] = run_fallback_matrix(
            core,
            fixtures,
            root / "fallback",
            platform,
            fallback_residuals,
            invariant_errors,
            protect_directory,
            _isolated_environment,
            _copy_fixture,
            Runner,
            conversion_arguments,
            assert_output,
            record_and_clean_residuals,
        )
        report["fallbackRuntimeResiduals"] = fallback_residuals

        install_plugin(runner, package, identity, "speech-install")
        runner.call(
            "speech-verify",
            ["plugins", "verify", "official.media.whisper", "--scope", "global", "--json"],
        )
        audio = _copy_fixture(fixtures, "asr-quality/source/en-clear.wav", work / "en-clear.wav")
        transcript = work / "transcript.md"
        transcription = runner.call(
            "speech-transcription",
            conversion_arguments(audio, transcript, ["--ai", "audio-transcription=only"]),
            timeout=180,
        )
        assert_output(transcript, "speech transcription")
        record_and_clean_residuals(
            environment,
            "into-md-plugin-dispatch-*",
            "speech-transcription",
            dispatch_residuals,
            invariant_errors,
        )
        diarized = work / "diarized.md"
        diarization = runner.call(
            "speech-diarization",
            conversion_arguments(
                audio, diarized, ["--ai", "audio-transcription=only", "--diarize"]
            ),
            timeout=180,
        )
        assert_output(diarized, "speech diarization")
        if "speaker" not in diarized.read_text(encoding="utf-8").lower():
            raise E2EError("diarization output has no speaker label")
        record_and_clean_residuals(
            environment,
            "into-md-plugin-dispatch-*",
            "speech-diarization",
            dispatch_residuals,
            invariant_errors,
        )
        runner.call(
            "speech-disable",
            ["plugins", "disable", "official.media.whisper", "--scope", "global"],
        )
        disabled = runner.call(
            "speech-disabled-conversion",
            conversion_arguments(audio, work / "disabled.md", ["--ai", "audio-transcription=only"]),
            succeed=False,
        )
        if "componentunavailable" not in _bounded(disabled.stderr + disabled.stdout).lower().replace("_", ""):
            raise E2EError("disabled speech did not return componentUnavailable")
        runner.call(
            "speech-enable",
            ["plugins", "enable", "official.media.whisper", "--scope", "global"],
        )
        corrupt_installed_plugin(user_data)
        runner.call(
            "speech-damaged-verify",
            ["plugins", "verify", "official.media.whisper", "--scope", "global", "--json"],
            succeed=False,
        )
        install_plugin(runner, package, identity, "speech-repair")
        runner.call(
            "speech-repaired-verify",
            ["plugins", "verify", "official.media.whisper", "--scope", "global", "--json"],
        )
        repaired_transcript = work / "repaired-transcript.md"
        runner.call(
            "speech-repaired-transcription",
            conversion_arguments(
                audio, repaired_transcript, ["--ai", "audio-transcription=only"]
            ),
            timeout=180,
        )
        assert_output(repaired_transcript, "repaired speech transcription")
        record_and_clean_residuals(
            environment,
            "into-md-plugin-dispatch-*",
            "speech-repaired-transcription",
            dispatch_residuals,
            invariant_errors,
        )
        runner.call(
            "speech-remove",
            ["plugins", "remove", "official.media.whisper", "--scope", "global"],
        )
        capabilities = runner.call("capabilities-after-remove", ["capabilities", "list", "--json"])
        capabilities_json = json.loads(capabilities.stdout)
        capability_text = json.dumps(capabilities_json, sort_keys=True).lower()
        if "transcription" not in capability_text or "not-installed" not in capability_text:
            raise E2EError("speech removal did not publish the not-installed capability state")
        if sha256_file(core) != report["core"]["binarySha256"]:
            raise E2EError("Core changed during the speech lifecycle")
        report["timingsMs"].update(
            {"speechTranscription": transcription.elapsed_ms, "speechDiarization": diarization.elapsed_ms}
        )
        report["dispatchResiduals"] = dispatch_residuals

        report["skillCases"] = run_skill_packaged_runtime(
            skill,
            platform,
            fixtures,
            root,
            expected_all_runtimes,
            protect_directory,
            _isolated_environment,
            Runner,
            _copy_fixture,
            conversion_arguments,
            runtime_directories,
        )
        report["cases"] = runner.cases
        if invariant_errors:
            raise E2EError("; ".join(invariant_errors))
        conclusion = "passed"
    except Exception as caught:
        error = str(caught)
        report["cases"] = locals().get("runner").cases if "runner" in locals() else []
    report["elapsedMs"] = round((time.monotonic() - started) * 1000)
    if "environment" in locals():
        report["residualDispatchDirectories"] = [
            str(path.relative_to(pathlib.Path(environment["TEMP"])))
            for path in dispatch_directories(environment)
        ]
    report["conclusion"] = conclusion
    report["error"] = error
    return report


def windows_to_wsl(path: pathlib.Path) -> str:
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


def run_wsl(arguments: argparse.Namespace, assets: pathlib.Path, fixtures: pathlib.Path) -> dict:
    script = windows_to_wsl(pathlib.Path(__file__))
    assets_path = windows_to_wsl(assets)
    fixtures_path = windows_to_wsl(fixtures)
    report_path = arguments.work_root / "linux-x86_64.json"
    report_wsl = windows_to_wsl(report_path)
    command = [
        "wsl.exe",
        "--exec",
        "python3",
        script,
        "--platform",
        "linux",
        "--assets-dir",
        assets_path,
        "--fixtures",
        fixtures_path,
        "--work-root",
        f"/tmp/into-md-post-release-e2e-{os.getpid()}",
        "--report",
        report_wsl,
        "--version",
        arguments.version,
        "--repository",
        arguments.repository,
        "--tag",
        arguments.tag,
    ]
    started = time.monotonic()
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if not report_path.is_file():
        detail = _bounded(result.stderr or result.stdout).strip()
        raise E2EError(f"WSL Linux E2E did not write its report: {detail}")
    envelope = json.loads(report_path.read_text(encoding="utf-8"))
    reports = envelope.get("platforms", [])
    if len(reports) != 1:
        raise E2EError("WSL Linux E2E report does not contain one platform result")
    report = reports[0]
    report["wslInvocationElapsedMs"] = round((time.monotonic() - started) * 1000)
    if result.returncode != 0 and report.get("conclusion") == "passed":
        raise E2EError(f"WSL Linux E2E process failed: {_bounded(result.stderr or result.stdout).strip()}")
    return report


def parse_arguments() -> argparse.Namespace:
    root = pathlib.Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=("all", "windows", "linux"), default="all" if os.name == "nt" else "linux")
    parser.add_argument("--repository", default=REPOSITORY)
    parser.add_argument("--tag", default="0.0.3")
    parser.add_argument("--version", default="0.0.3")
    parser.add_argument("--assets-dir", type=pathlib.Path)
    parser.add_argument("--fixtures", type=pathlib.Path, default=root / "fixtures")
    parser.add_argument("--work-root", type=pathlib.Path, default=pathlib.Path.cwd() / "post-release-e2e")
    parser.add_argument("--report", type=pathlib.Path)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    if arguments.platform in {"all", "windows"} and os.name != "nt":
        raise E2EError("Windows E2E must be launched from Windows")
    if arguments.platform == "linux" and os.name == "nt":
        raise E2EError("use --platform all on Windows to run Linux through WSL")
    fixtures = arguments.fixtures.resolve(strict=True)
    arguments.work_root.mkdir(parents=True, exist_ok=True)
    assets = (arguments.assets_dir or (arguments.work_root / "assets")).resolve()
    platforms = ["windows", "linux"] if arguments.platform == "all" else [arguments.platform]
    started = time.monotonic()
    output: dict[str, Any] = {
        "schemaVersion": 1,
        "repository": arguments.repository,
        "tag": arguments.tag,
        "version": arguments.version,
        "assets": {},
        "platforms": [],
        "conclusion": "failed",
        "error": None,
    }
    report_path = arguments.report or (arguments.work_root / "post-release-e2e.json")
    try:
        platform_errors: list[str] = []
        output["assets"] = acquire_assets(
            assets, arguments.repository, arguments.tag, platforms
        )
        if arguments.platform in {"all", "windows"}:
            windows_root = arguments.work_root / "windows"
            if windows_root.exists():
                raise E2EError(f"work root already exists: {windows_root}")
            platform_report = run_platform(
                "windows", assets, fixtures, windows_root, arguments.version
            )
            output["platforms"].append(platform_report)
            if platform_report["conclusion"] != "passed":
                platform_errors.append(
                    str(platform_report["error"] or "Windows E2E failed")
                )
        if arguments.platform == "all":
            platform_report = run_wsl(arguments, assets, fixtures)
            output["platforms"].append(platform_report)
            if platform_report["conclusion"] != "passed":
                platform_errors.append(
                    str(platform_report["error"] or "WSL Linux E2E failed")
                )
        elif arguments.platform == "linux":
            linux_root = arguments.work_root / "linux"
            if linux_root.exists():
                raise E2EError(f"work root already exists: {linux_root}")
            platform_report = run_platform(
                "linux", assets, fixtures, linux_root, arguments.version
            )
            output["platforms"].append(platform_report)
            if platform_report["conclusion"] != "passed":
                platform_errors.append(
                    str(platform_report["error"] or "Linux E2E failed")
                )
        if platform_errors:
            raise E2EError(" | ".join(platform_errors))
        output["conclusion"] = "passed"
    except Exception as error:
        output["error"] = str(error)
    output["elapsedMs"] = round((time.monotonic() - started) * 1000)
    write_json(report_path, output)
    print(report_path.resolve())
    if output["conclusion"] != "passed":
        raise E2EError(str(output["error"] or "post-release E2E failed"))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (E2EError, OSError, subprocess.SubprocessError, zipfile.BadZipFile, json.JSONDecodeError) as error:
        print(f"post-release-e2e: {error}", file=sys.stderr)
        raise SystemExit(1)
