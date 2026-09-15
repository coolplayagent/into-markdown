"""Verify unavailable PDF runtimes preserve best-effort input and strict errors."""
import hashlib
import json
import pathlib
import subprocess


def runtime_failure_case(name, binary, arguments, cwd, environment, run_case, error_type):
    source = pathlib.Path(arguments[0])
    output_index = arguments.index("-o") + 1
    recovered = cwd / f"{name}-recovered.md"
    recovery_report = cwd / f"{name}-recovered.json"
    recovery_args = list(arguments)
    recovery_args[output_index] = str(recovered)
    recovery, _ = run_case(
        name + "-best-effort", binary,
        [*recovery_args, "--error-policy", "best-effort", "--report", str(recovery_report)],
        cwd, environment,
    )
    report = json.loads(recovery_report.read_text(encoding="utf-8"))
    originals = list(recovered.with_name(recovered.stem + "_assets").glob("*.pdf"))
    items = report.get("items", [])
    if (report.get("failed") != 0 or len(items) != 1
            or items[0].get("outcome") != "degraded"
            or not any(d.get("code") == "pdf.recovery.originalPdf"
                       for d in items[0].get("diagnostics", []))
            or not recovered.is_file() or len(originals) != 1
            or originals[0].read_bytes() != source.read_bytes()
            or originals[0].name not in recovered.read_text(encoding="utf-8")):
        raise error_type(f"{name} did not preserve the original PDF in best-effort")
    strict_report = cwd / f"{name}-strict.json"
    result = subprocess.run(
        [str(binary), *arguments, "--error-policy", "strict",
         "--report", str(strict_report), "--log-format", "json"],
        cwd=cwd, env=environment, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20, check=False,
    )
    report = json.loads(strict_report.read_text(encoding="utf-8"))
    items = report.get("items", [])
    if (result.returncode != 10 or report.get("failed") != 1
            or report.get("succeeded") != 0 or len(items) != 1
            or items[0].get("errorCode") != "componentUnavailable"
            or pathlib.Path(arguments[output_index]).exists()):
        raise error_type(f"{name} strict runtime error differs: exit={result.returncode}, report={report}")
    return {"name": name, "exitCode": result.returncode, "code": "componentUnavailable",
            "recovery": recovery, "originalSha256": hashlib.sha256(source.read_bytes()).hexdigest()}
