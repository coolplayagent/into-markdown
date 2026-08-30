# Issue #316: faithful XLS formula text

## Boundary and representation

The XLS inventory authenticates the original Formula record framing and hashes
its original token slice before constructing the calamine reader view. A bounded
RPN decoder preserves literals, operators, local references/areas, authenticated
same-workbook 3D references and a finite worksheet-function vocabulary. It does
not calculate formulas, open external workbooks, or execute functions/macros.

`0x0c` is `>=`, and `0x0d` is `>`, per the Microsoft
[Ptg table](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/9310c3bb-d73f-4db0-8342-28e1e0fcb68f).
[RgceLoc](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/f2395c33-34a4-4b07-85a9-9bb5f07848d9)
stores coordinates even for relative references; the relative bits control `$`
display, not column arithmetic. Ref and Area share that decoding and the existing
workbook `cell_name` implementation. External XTI/SupBook identity is never
discarded to manufacture a local sheet reference.

An explicit `Decoded`/`CachedOnly` state replaces the optional formula override.
Unsupported names, external references, array/shared/table formulas and other
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

## Repository checks

```text
cargo fmt --all --check
cargo clippy -p into-markdown-converters -p into-markdown-engine -p into-markdown-render-markdown --all-targets -j 2 -- -D warnings
cargo test -p into-markdown-converters -p into-markdown-engine -p into-markdown-render-markdown -j 2
cargo build --release -p into-markdown-cli -j 2
```

The broader `cargo clippy --workspace --all-targets -j 2 -- -D warnings`
attempt encounters the existing `unused_mut` in unmodified
`third_party/whisper-rs-0.16.0/sys/build.rs:135`. That unrelated vendor file is not
changed by this issue. Independent review is left to the coordinating task.
