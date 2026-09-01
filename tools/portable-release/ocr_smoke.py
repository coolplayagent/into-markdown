"""Exercise the installed Core's real OCR provider during native archive acceptance."""

import hashlib
import json
import pathlib
import shutil
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURE_ID = "ocr-english-clear-1"
MEMORY_BYTES = 2 * 1024**3
SIGNED_WORKER_BYTES = 2048 * 1024**2
MIN_EFFECTIVE_WORKER_BYTES = 1024 * 1024**2


def fingerprint(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def recognized_text(value):
    if isinstance(value, list):
        return "".join(recognized_text(item) for item in value)
    if not isinstance(value, dict):
        return ""
    if value.get("type") in {"text", "sourceText"}:
        data = value.get("data", {})
        if data.get("provenance", {}).get("kind") == "localOcr":
            return data.get("value", "")
    return "".join(recognized_text(child) for child in value.values())


def verify_result(document, report, expected, error_type):
    if (expected not in recognized_text(document.get("document", {}))
            or not document.get("markdown") or report.get("failed") != 0):
        raise error_type("installed OCR did not preserve the clear fixture's expected text")
    usage = report.get("resourceUsage", {})
    runtime = usage.get("ocrRuntime", {})
    worker_min = runtime.get("workerBudgetMinBytes", 0)
    worker_max = runtime.get("workerBudgetMaxBytes", 0)
    if (usage.get("sharedLeaseBudgetBytes") != MEMORY_BYTES
            or not 0 < usage.get("sharedLeasePeakBytes", 0) <= MEMORY_BYTES
            or runtime.get("requests") != 1
            or runtime.get("recognitionMemoryRefusals") != 0
            or not MIN_EFFECTIVE_WORKER_BYTES <= worker_min == worker_max <= SIGNED_WORKER_BYTES
            or usage.get("ocr", {}).get("recognizedChars", 0) < len(expected)):
        raise error_type("installed OCR budget or contribution evidence is incomplete")
    return usage


def ocr_case(binary, environment, error_type):
    manifest = json.loads((ROOT / "fixtures/manifest.json").read_text(encoding="utf-8"))
    fixture = next(item for item in manifest["fixtures"] if item["id"] == FIXTURE_ID)
    golden = next(item for item in manifest["ocr_quality"]["goldens"]
                  if item["fixture_id"] == FIXTURE_ID)
    source = ROOT / "fixtures" / fixture["path"]
    if fingerprint(source) != golden["fixture_sha256"]:
        raise error_type("installed OCR fixture identity differs from its quality authority")
    # A separate state keeps the existing no-runtime checks for plain text and
    # PDF independent of OCR's intentional runtime materialization.
    with tempfile.TemporaryDirectory(prefix="into-md-installed-ocr-") as name:
        work = pathlib.Path(name).resolve()
        env = dict(environment)
        for variable, folder in {"HOME": "home", "USERPROFILE": "home", "APPDATA": "data",
                "LOCALAPPDATA": "cache", "XDG_CACHE_HOME": "cache", "XDG_DATA_HOME": "data",
                "XDG_CONFIG_HOME": "config", "TMP": "tmp", "TEMP": "tmp", "TMPDIR": "tmp"}.items():
            path = work / folder
            path.mkdir(mode=0o700, exist_ok=True)
            env[variable] = str(path)
        image = work / source.name
        shutil.copyfile(source, image)
        output, report_path = work / "result.json", work / "report.json"
        command = [str(binary), "--no-config", "--ocr", "always", "--error-policy", "strict",
            "--max-memory-size", "2GiB", "--timeout-ms", "120000", "--progress", "never",
            "--log-format", "json", "--emit", "result-json", "--asset-mode", "omit",
            "--conflict", "error", "--output", str(output), "--report", str(report_path), str(image)]
        started = time.monotonic()
        result = subprocess.run(command, cwd=work, env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=150, check=False)
        if result.returncode:
            raise error_type(f"installed real OCR failed ({result.returncode}): "
                             + result.stderr[-8192:].decode("utf-8", "replace"))
        for path in [output, report_path]:
            if not path.is_file() or path.stat().st_size > 4 * 1024**2:
                raise error_type("installed OCR result or report is absent or exceeds its bound")
        usage = verify_result(json.loads(output.read_text(encoding="utf-8")),
            json.loads(report_path.read_text(encoding="utf-8")), golden["ground_truth_nfc"], error_type)
        return {"name": "real-ocr-provider", "exitCode": result.returncode,
            "elapsedMs": round((time.monotonic() - started) * 1000), "command": command,
            "binarySha256": fingerprint(binary), "fixtureSha256": fingerprint(image),
            "outputSha256": fingerprint(output), "reportSha256": fingerprint(report_path),
            "resourceUsage": usage, "networkRequired": False, "conclusion": "pass"}
