# Issue #316: faithful XLS formula text

## Boundary and representation

The XLS inventory authenticates the original Formula record framing and hashes
its original token slice before constructing the calamine reader view. A bounded
RPN decoder preserves literals, operators, local references/areas, authenticated
same-workbook 3D references, ordinary local defined-name identifiers and a finite
worksheet-function vocabulary. It does
not calculate formulas, open external workbooks, or execute functions/macros.

`0x0c` is `>=`, and `0x0d` is `>`, per the Microsoft
[Ptg table](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/9310c3bb-d73f-4db0-8342-28e1e0fcb68f).
[RgceLoc](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/f2395c33-34a4-4b07-85a9-9bb5f07848d9)
stores coordinates even for relative references; the relative bits control `$`
display, not column arithmetic. Ref and Area share that decoding and the existing
workbook `cell_name` implementation. External XTI/SupBook identity is never
discarded to manufacture a local sheet reference.

Local [PtgName](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/5f05c166-dfe3-4bbf-85aa-31c09c0258c0)
ordinals are resolved against authenticated `Lbl` names and scopes. Name definitions
are not evaluated or expanded. Cross-sheet local names receive their quoted sheet
qualifier; duplicate scoped names and shadowed global names are not guessed.

An explicit `Decoded`/`CachedOnly` state replaces the optional formula override.
Unsupported external/macro/ambiguous names, external references, array/shared/table formulas and other
unsupported syntax produce inert cached-only text containing the reason, exact
original token bytes and SHA-256. There is no fallback to calamine's expression
text. Diagnostics aggregate by reason with a first-cell locator; every affected
cell retains its own evidence. Normal tables and paged TSV use the same formula
renderer, including cached-only states. Strict and best-effort policies both
retain unsupported formula evidence; container/framing failures remain errors.

Calamine eagerly decodes formulas during `Xls::new`. After the original inventory
is complete, the XLS-only reader view sets Formula `cce` to zero without changing
cache bytes, record sizes, or offsets. The adapter does not clone calamine's unused
XLS formula range. BIFF4 normalization retains its original token bytes before
the same reader-view adjustment. No calamine vendoring, dependency changes,
lint exemptions, Actions, or DOC/PPT/CFB policy changes are included.

The expression arena stores each node/edge once; iterative output respects
operator precedence and right-child associativity without recursive rendering
or repeated whole-expression copies. The existing request memory/work/field
budgets cover temporary nodes, long-sheet-name expansion, retained text and raw
tokens. No limits are increased.

## Reproduction and evidence boundary

Implementation base: `fb96baf3444b394f21b68c972600e9fbd2123ae1` (merged #315).
The frozen #315 release binary SHA-256 is
`dd4e7da3735be7165887bd9adecdd075657b6f9b8c7648c5e47b1c64e0c2736a`.
The schema-3 manifest SHA-256 is
`7f82792dca7d1011d8a1bbcdfe7de5b4f0c463515be347405d0c88a6b866cacc`.
Its 452 XLS paths are independently classified as 425 valid, 24 invalid,
2 safely recoverable and 1 unclassified. Conversion outcomes do not determine
those classifications. Sources and earlier evidence remain read-only.

Local serialized correctness evidence is stored under
`C:/im-bench-004/issue316-xls-20260831`. `matrix.py` runs the frozen baseline or
candidate with `--jobs 1 --ocr off --emit result-json`; `verify.py` independently
checks cached values/token identities, table shape/order/merges/duplicate cells,
and formula-body comparisons as separate gates. Root owns the final serialized
performance/RSS/lease/temp/repeated-read acceptance; these correctness runs are
not formal performance results.

The previous 343 formula discrepancies are not 343 proven semantic errors.
Every discrepancy is retained in the evidence, including empty/unsupported oracle
expressions marked `reviewNeeded`; they are not counted as semantic successes.

## Correctness results (authorized delta cohorts)

Tested implementation: `117344be9c3189a8f50290a9f1792d45b1af35f5`.
Its release executable SHA-256 is
`1435cdfdd02a7bbbc4b0c9abad75ea10c949bfd28ef1baa45f0d9cff1d1efd83`.
The initial full 452-path run used `4b63cc2`; subsequent fixes reran only the
45 named-formula paths at `a0c7864`, then the remaining legal BIFF name case at
the tested implementation. The consolidated evidence contains 407 initial,
44 named-formula and 1 final-path results. **It is not a full-corpus run of the
final executable.** `final-source-cohorts.json` retains each executable identity.

| Gate | Result |
| --- | --- |
| Raw conversion success | 412/452 (91.15%), unchanged from #315 |
| Independently valid conversion success | 409/425 (96.24%), unchanged |
| Other successful classifications | 2 safely recoverable, 1 invalid; the latter is an unchanged baseline success, not a valid-file claim |
| Cached/display values, original token SHA, shape/order/merges/duplicate coordinates | 412/412 common successes preserved |
| Independent cached-value oracle | 8,339 cells across 17 files, zero mismatches |
| Markdown unchanged from #315 | 265 common-success files |
| Repeated conversion | Five source-case Markdown and Document outputs identical on the same initial executable |
| Previously correct ordinary named formulas | 227/227 restored and independently matched |

Across 78,815 formula identities, 30,953 have decoded expressions and 47,862
retain explicit cached-only evidence. The latter consist of 39,563 shared/array
formulas, 5,743 invalid local references, 1,787 external defined names, 581 external
references, 82 data-table formulas, 50 macro names, 21 unsupported 3D references,
18 array constants, 14 unsupported tokens and 3 unsupported functions. These
counts describe support boundaries, not independent correctness scores for every
expression. Of 9,561 original named-formula downgrade candidates, 6,643 now decode;
the remainder have specific external/macro/reference/function reasons. Name
definitions are still never expanded or evaluated. The legal name/address check
uses the BIFF8 grid (256 columns, 65,536 rows), not the larger XLSX grid.

The previous 343 disagreements are accounted for individually in `final-343.json`:
79 exact matches, 89 cosmetic numeric-normalization matches, 148 explicit
cached-only unsupported cases, 13 textual differences independently established
as equivalent, and 14 empty/unsupported oracle expressions still requiring
review. The 13 retain their mechanical `reviewNeeded-formulaBody` classification
and a separate identity-bound `independentlyEquivalent` approval: nine redundant
parentheses and four authenticated same-workbook singleton references. None of
the 14 empty oracle expressions is treated as ground truth.

Canonical local artifacts are `final-summary.json`, `final-source-cohorts.json`,
`final-343.json`, `final-named-formulas.json`, and the per-cohort result JSON files.
`finalize.py` recomputes the independent oracle comparisons against the selected
delta outputs without another conversion. The five source cases cover INDEX
areas, `>` versus `>=`, AA–AD columns, relative SUM areas and external-reference
cached-only fallback. Final release-wide latency/RSS/lease/temp/read-amplification
acceptance remains pending in the coordinating task; no performance percentage
is claimed by this correctness evidence.

## Repository checks

```text
cargo fmt --all --check
cargo clippy -p into-markdown-converters -p into-markdown-engine -p into-markdown-render-markdown --all-targets -j 2 -- -D warnings
cargo test -p into-markdown-converters -p into-markdown-engine -p into-markdown-render-markdown -j 2
cargo build --release -p into-markdown-cli -j 2
```

The full related suite passed 650 tests (574 converters, 45 engine, 31 renderer)
on the initial implementation; three converter tests and one PDF integration
test remained explicitly ignored. Later changes passed 38 targeted XLS tests
and then all 10 formula tests, including defined-name scope/ambiguity and BIFF
grid boundaries. The coordinator requested delta testing instead of repeating
the 650-test suite. Final-source fmt, related all-target clippy with warnings
denied, and release build passed. Logs: `tests.log`, `name-tests.log` and
`clippy-final.log` in the local evidence directory.

The broader `cargo clippy --workspace --all-targets -j 2 -- -D warnings`
attempt encounters the existing `unused_mut` in unmodified
`third_party/whisper-rs-0.16.0/sys/build.rs:135`. That unrelated vendor file is not
changed by this issue. Independent review is left to the coordinating task.
