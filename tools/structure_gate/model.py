"""Shared metrics and stable identities; no repository I/O."""

from dataclasses import asdict, dataclass, field

FILE_LIMIT = 1000
FUNCTION_LIMIT = 100
BASELINE_PATH = "tools/structure_gate/baseline.json"
AUTHORITY_PATH = "tools/structure_gate/exceptions.json"
BOOTSTRAP_COMMIT = "a66287de6978ff3e1a94e1b45f2b0809051eea41"
RUST_LINTS = {"too_many_lines", "too_many_arguments", "type_complexity", "large_enum_variant"}
PYTHON_LINTS = {"C901", "PLR0912", "PLR0913", "PLR0915", "too-many-arguments",
                "too-many-branches", "too-many-statements", "too-many-locals"}
TS_LINTS = {"complexity", "max-lines", "max-lines-per-function", "max-params",
            "max-depth", "max-statements"}


class GateError(ValueError):
    """Input cannot be checked reliably; callers must fail closed."""


@dataclass
class Function:
    symbol: str
    line: int
    end: int
    lines: int


@dataclass
class Allowance:
    symbol: str
    rule: str
    scope: str
    line: int
    reason: str = ""

    @property
    def key(self) -> str:
        return f"{self.scope}|{self.symbol}|{self.rule}"


@dataclass
class Metric:
    path: str
    digest: str
    physical_lines: int
    code_lines: int
    functions: list[Function] = field(default_factory=list)
    allowances: list[Allowance] = field(default_factory=list)
    hints: list[dict] = field(default_factory=list)

    @property
    def maximum(self) -> int:
        return max((item.lines for item in self.functions), default=0)

    def json(self) -> dict:
        return asdict(self)


def symbol_name(parts: list[str], seen: dict[str, int]) -> str:
    name = "::".join(parts)
    seen[name] = seen.get(name, 0) + 1
    return name if seen[name] == 1 else f"{name}#{seen[name]}"
