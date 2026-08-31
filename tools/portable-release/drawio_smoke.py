"""Drawio semantic acceptance through the extracted native Core binary."""
import hashlib
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[2]
GOLDEN = "e7a22ce8919dc6209d9ccdd222a84d4dd376e58a986f6c2875d82628304bdd2c"


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
