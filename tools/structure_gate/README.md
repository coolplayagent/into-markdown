# Ratcheting production-code structure gate (#277)

This development tool stops new structural debt without requiring a rewrite of
historical files. It does not change conversion, package contents, or release
workflows. The existing Linux PR fast gate runs it once; the four existing jobs
and their five-minute timeouts remain unchanged.

## Local use

Use Python 3.13 and install the pinned, development-only parser wheels. Stage
new source files before checking: discovery deliberately uses Git's tracked
file inventory, not arbitrary files in your workspace. Modified tracked files
are read from the working tree, not from the index.

```sh
python -m pip install --only-binary=:all: -r tools/structure_gate/requirements.txt
git fetch origin main
python -m tools.structure_gate report --base-ref origin/main
python -m tools.structure_gate check --base-ref origin/main
python -m tools.structure_gate ratchet --base-ref origin/main
python -m unittest discover -s tools/structure_gate/tests -v
```

`report` inventories source without requiring a baseline. With `--base-ref`, it
also checks the ratchet. `check` requires a baseline and the exact base commit.
`ratchet` measures both trees, rejects increases, then atomically rewrites the
candidate baseline to the remaining debt. Commit that baseline update with your
refactor. Check never rewrites it. Exit codes are 0 (pass), 1 (policy violation),
and 2 (invalid input, parser/dependency/I/O failure); unexpected exceptions also
exit nonzero.

All commands support `--format json`; use `--output /absolute/path/outside/repo.json`
to save a JSON report outside the checkout. The default is a text report on
stdout. `--root` selects another checkout. JSON includes base and candidate
physical/production lines, every function and suppression location, deltas,
excluded paths with reasons, advisory findings, analysis time and peak process
RSS. Telemetry measures analysis/report construction before serialization; total
command wall time also includes output. No report is uploaded by this workflow.

## Counting and thresholds

| Metric | Default cap | Historical over-cap item |
| --- | ---: | --- |
| Production lines per file | 1,000 | Cannot grow; reduced value becomes the new cap |
| Production lines per function | 100 | Same rule, individually keyed by qualified symbol |
| Structural lint allowances | None newly added | Existing occurrences remain, but cannot be transferred to another symbol |

Production lines are nonblank lines containing syntax, not pure comments or
Python docstrings. Lines containing both comments and code count once. Multiline
literal contents count when nonblank. Function spans include their declaration
and body; Python decorator lines count. Nested functions also have their own
limits. Rust macro **definitions** and closures count as callable units; macro
invocations are not expanded. TS/TSX arrow functions and methods count too.
The longest function is reported, but the policy compares every function: adding
a second giant function below an existing maximum is not allowed.

Only tracked `.rs`, `.py`, `.ts`, and `.tsx` sources are scanned. Explicit exclusions:

- `third_party/` is fixed external source.
- `dist`, `generated_assets`, and `generated_assets_repeat` directories contain
  generated assets. A source comment saying “generated” is not an exclusion.
- `tests`, `fixtures`, `benches`, `testdata`, `real-world-test-data` directories;
  `tests.rs`, `conftest.py`, `test_*`, `*_test.py`, `*_tests.rs`,
  `*_test_support.rs`, `*_fixture.rs`, and TS/TSX `.test`/`.spec` modules.
- Rust items and attached attributes that are explicitly test-only: `#[test]`,
  namespaced test attributes, and `cfg` expressions provably false when `test`
  is false. For example, `all(test, unix)` is excluded, but `any(test, unix)`
  and `not(test)` are production. Other build features remain unknown, not false.

Pure, byte-identical renames may transfer debt once from a deleted path. Copies
cannot. A rename with edits is treated as a new file: split it under the default
cap first, or perform an unchanged rename separately. Anonymous callable symbols
use scope-local occurrence numbers when no declaration name is available.

## Allowances and explicit exceptions

The inventory includes Rust `allow`, `expect`, and conditional `cfg_attr`
allowances for `too_many_lines`, `too_many_arguments`, `type_complexity`, and
`large_enum_variant`, including encompassing Clippy groups and `warnings`.
Python covers complexity/argument/branch/statement allowances in `noqa` and Pylint
comments, including numeric aliases and broad disables. TS/TSX covers structural
ESLint rules such as `complexity`, `max-lines`, `max-lines-per-function`,
`max-params`, `max-depth`, and `max-statements`. Comment-like string contents do
not count. This is a structural inventory, not a replacement for those linters.

New crate/module/file-wide allowances are forbidden. If a genuinely indivisible
state machine needs a local lint allowance, add its exact path, qualified symbol,
rule, nonempty reason, and repository issue URL to `exceptions.json`. Put the
same reason in Rust's `reason = "..."`, an adjacent comment, or a Python/ESLint
suppression comment after ` -- `. The PR reviewer must explicitly approve that
record; the script checks the record and scope, not GitHub reviewer identities.
An exception authorizes a local lint allowance, **not** increased line caps.

```json
{
  "path": "crates/example/src/state.rs",
  "symbol": "impl Decoder::advance",
  "rule": "too_many_arguments",
  "reason": "One atomic state transition must receive the complete input tuple.",
  "issue": "https://github.com/coolplayagent/into-markdown/issues/277"
}
```

The authority file is a JSON array. Do not add a generic exception for a future
file or copy an existing exemption to another function. Deleting one allowance
does not permit adding another elsewhere.

## Baseline lifecycle and review hints

The initial baseline is measured from commit
`a66287de6978ff3e1a94e1b45f2b0809051eea41`, not from the implementation branch.
Only this exact bootstrap commit may lack a base baseline. Subsequent PRs read
the baseline from their exact GitHub `pull_request.base.sha`, verify it against
the base tree, and require the candidate baseline to equal its measured remaining
debt. Editing JSON to raise a cap does not authorize growth. Missing/corrupt
baselines, duplicate normalized paths or symbols, parse failures and escaping
paths stop the check. Windows separators normalize to `/`.

Baseline updates reserve a short-lived exclusive `.lock`, verify the original
bytes have not changed during analysis, and replace a same-directory temporary
file. Parallel checks are read-only. If a killed writer leaves a lock, first
confirm no ratchet process remains, remove that single lock and rerun. There is
no background lock service or automated recovery daemon.

Changes to thresholds, exclusions, parser versions or counting semantics require
an explicit tooling-policy PR and a reviewed baseline migration. Ordinary
`ratchet` intentionally cannot silently reinterpret historical debt; if the
measured base inventory changes, it fails instead of inventing extra budget.

Filename conditions and three or more repeated long (at least 15 production
lines) branch fingerprints are review hints only. Matching is syntactic, not
semantic; legitimate format dispatch and repeated state transitions may appear.
No heuristic finding automatically blocks a PR.

The pinned Tree-sitter Python binding is 0.25.2: 0.26.0 crashed on this Windows
source scan. Rust grammar 0.24.2 supports the repository's let chains but mistakes
the contextual identifier `raw` in `raw @ (...)` match patterns for a keyword.
The parser adapter corrects only that recognized error node in memory, preserving
byte positions, then requires a completely clean parse. Source files are never
changed and other syntax errors still fail; a regression fixture covers this
grammar limitation. No file-name-specific parser bypass exists.

## Acceptance boundary

Tests cover threshold violations, exact per-function debt, local exceptions,
baseline reduction, renames versus copies, path normalization, malformed source,
test isolation, simultaneous read-only checks, stale/concurrent writers and
atomic cleanup. Integration runs scan the actual repository and report elapsed
analysis time and peak RSS. Trees and input bytes are processed one file at a
time; the gate does not compile the conversion application or run document
corpora. CI integration must keep the existing four-job/five-minute contract.

Measured acceptance on Windows / Python 3.13.12 against the pinned 0.0.4 main
commit: 32 tool tests and 3 existing PR-gate contract tests passed; the actual
base/candidate scan checked 541 production files, reported 184 exclusions and
zero violations in 10.33 seconds with 54.9 MiB peak RSS. The initial inventory
freezes 35 over-cap files, 187 over-cap callable units and 175 structural lint
allowance occurrences across 137 files. The tool adds no over-cap item or lint
allowance of its own (largest production module: 153 physical lines; longest
function: 54 production lines). These are local measurements, not CI timing
guarantees or conversion-performance results.
