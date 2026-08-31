"""Drawio semantic acceptance through the extracted native Core binary."""
import hashlib
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[2]
GOLDEN = "5ecb88e03eb41cad516db20588b7f68ad7f62c0f2e2bf69a3eddb4c4d4de3599"


def drawio_cases(binary, work, environment, run_case, error_type):
    cases = []
    for name in ("normal", "compressed"):
        fixture = ROOT / "fixtures/small/drawio" / f"{name}.drawio"
        result = work / f"drawio-{name}.md"
        case, _ = run_case(
            f"drawio-{name}", binary,
            [str(fixture), "-o", str(result), "--conflict", "error", "--no-config", "--ocr", "off"],
            work, environment,
        )
        if not result.is_file() or hashlib.sha256(result.read_bytes()).hexdigest() != GOLDEN:
            raise error_type(f"installed Drawio {name} output differs from its semantic golden")
        cases.append(case)
    return cases
