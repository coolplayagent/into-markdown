# Issue #306: legacy DOC/PPT table and field compatibility

## Scope and provenance

Implementation commit: `d707eb31921f62b6b038bdf8059b9b41a3946ae5`, based on
`a6cb5e227de78b51fe6dc5dc5a680dba90f78c51`. Only the legacy DOC/PPT converter,
its tests and a converter-local probe changed. No Core/Engine/result, shared CFB,
XLS, dependency, workflow or lint-exemption changes. Independent review is left
to the coordinating task; this report records tests, not a review approval.

The immutable schema-3 manifest was read before conversion:

- Manifest SHA-256: `7f82792dca7d1011d8a1bbcdfe7de5b4f0c463515be347405d0c88a6b866cacc`.
- Raw cohort: **221 DOC + 200 PPT paths**, 405 distinct source SHA-256 values.
- Independent classification: **421 unclassified**, unchanged. Failures are not
  used to infer malformed-source validity. Paths sharing bytes remain separate.
- Original quality fingerprint:
  `75af809153636b33888f281ca408765982d6b3c39a0a41945d9502f40034fbac`.
- Historical 9e797 fingerprint:
  `f7f3521c0fe9992ae2ec76fd7e6a402b2b3fc72480c379e91badb35e30555c4d`.
- Both original JSONL files contain 2,289 unique, current-fingerprint terminals.
- Candidate CLI SHA-256:
  `7d10f8bf197255b2b922f0bde05dd9abfc7772bbf9d77d7ca23e905ca401e401`.

Original evidence under `C:/im-bench-004/formal-schema3` and source files were
read-only. All new per-path native reports, Markdown, probe IR, stderr and
comparison scripts are under `C:/im-bench-306`.

## Changes and quality evidence

DOC row boundaries come from direct PAPX table-terminating properties, not a
guess that every carriage return ends a row. Cell boundaries form a shared
logical grid; horizontal/vertical merge flags produce spans. Empty cells and
paragraphs inside cells remain in source order. PPT retains leading, interior
and trailing tab-separated empty cells. Both paths use the same width-padding
helper, checking the table budget before occupancy allocation.

This follows Microsoft's [table model](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-doc/5b45f0e7-7760-4fdb-af88-0146de2feb4c),
[PAPX page mapping](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-doc/34aaeaf3-9578-41af-a3f5-c12f6f66bf1b),
and [cell merge flags](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-doc/11bf5b1c-943f-421d-bbf3-39088cd1b8dd).
Unsupported/inconsistent geometry is not guessed: source cell text is retained
with `legacyOffice.doc.tableGeometryOmitted` (observed on `Bug33519.doc`).

Unsafe stored field targets become visible text with
`legacyOffice.doc.unsafeHyperlinkOmitted`; strict mode rejects them. No target
is fetched or executed. Ordinary query-string links remain links.

The 13 issue paths were executed first, followed by the entire 421-path cohort.
All 13 changed from `internal` to `success/degraded`. Before/after ordered
non-whitespace character sequences extracted from converter IR are identical
for each path; this checks that fixing IR validity did not discard text.

| Issue path basename | Retained non-whitespace characters |
| --- | ---: |
| ProblemExtracting.doc | 24,482 |
| Bug47286.doc | 826 |
| Bug47287.doc | 826 |
| table-merges.doc | 11 |
| Bug46220.doc | 69 |
| cap.stanford.edu_profiles_viewbiosketch_facultyid=4009&name=m_maciver.doc | 7,857 |
| testWORD_closingSmartQInHyperLink.doc | 9 |
| 44770.ppt | 1,369 |
| 42474-2.ppt | 12,648 |
| 54332b.ppt | 851 |
| bug60345_suba.ppt | 2,403 |
| bug61881.ppt | 1,127 |
| ParagraphStylesShorterThanCharStyles.ppt | 1,258 |

`table-merges.doc` now has four rows. Its source coordinates yield logical
colspans `[3,2]`, `[1,1,2,1]`, `[1,1,2,1]`, `[5]`; the third row keeps its empty
first cell and `I`/`J` in the same cell. Synthetic fixtures additionally verify
explicit vertical and combined horizontal/vertical merges, surrounding
paragraph order, mismatching property fallback, opaque/truncated PAPX operands,
unsafe fields in best-effort/strict, and width limits.

Relative to original 8f98 quality:

- **230/230 old successful paths remain successful**: 200 Markdown files are
  byte-identical; the 30 changed files are all covered by the ordered IR-text
  comparison. All 234 converter-local common successes have identical ordered
  non-whitespace text, preserving repeated occurrences rather than comparing sets.
- Final real CLI: **DOC 114 success / 107 failed; PPT 130 success / 70 failed**.
  Success includes degraded outcomes; see the terminal matrix below.
- In addition to the 13 readable recoveries, `57603-seven_columns.doc` retains
  its genuine seven-column empty table and changes from `emptyContent` failure
  to degraded structural output. It is **not counted as recovered text**.
- Remaining 177 CLI failures: 164 `malformed`, 10 `encrypted`, 2 `noConverter`,
  1 `resourceLimit`. These are terminal categories, not source-validity findings.
- Separate **extract-mode runs of both binaries, 421 paths each**, retain all
  230 old successes, the same **949 asset-file SHA-256 occurrences**, and the
  same ordered **957 asset-reference occurrences**. No asset-set or reference-order
  differences; quality-mode runs use `omit`, matching the original harness.
- Converter probe memory/temporary leases after drop are zero on all 421 paths.

Relative to historical 9e797, the sole old-success/current-failure path is
`TestRobert_Flaherty.doc` (id `9338acfd435514023b6928d3`, source SHA-256
`af214ab59769b4ccbda71b365edb9d06b4ff1cf9ada4215300669c0e09daac4c`). It is
actually XLS: root `Book` is 5,881 bytes with BIFF5 BOF, `Workbook` is 5,984 bytes
with BIFF8 BOF. The existing ambiguous-dual-workbook policy rejects it; #306
does not change that policy or classify the file as damaged. This exception
was explicitly coordinated outside the DOC/PPT table scope.

## Checks and reproduction

Windows, Rust 1.97.1, build jobs at most two; every corpus conversion is serial.

```text
cargo fmt --all -- --check
cargo clippy -p into-markdown-converters --all-targets -j 2 -- -D warnings
cargo test -p into-markdown-converters -p into-markdown-render-markdown -j 2 --quiet
cargo build -p into-markdown-cli -p into-markdown-converters --example legacy_office_probe --bin into-md -j 2
```

Results: converter 549 passed / 1 existing ignored; renderer 31 passed;
PDF-quality integration target 1 existing ignored (requires pinned PDFium).
Doc tests pass. One initial broader-test invocation hit Windows LNK1104 while
the probe executable was running; the full command passed after that probe
finished. No source change or test suppression was used for the retry.

Real CLI per-file invocation uses the manifest's SHA-verified staged bytes:

```text
into-md INPUT --no-config --output-dir OUTPUT --report REPORT.json --jobs 1 --ocr off --asset-mode omit --error-policy best-effort --conflict error --progress never --quiet
legacy_office_probe INPUT OUTPUT.json doc|ppt
```

Extract comparisons substitute `--asset-mode extract`. The committed probe
includes pre-validation IR to make invalid-IR-before/valid-IR-after text-order
comparisons possible. External scripts `run_cli.py`, `run_probe.py`,
`compare_probe.py`, `summarize_cli.py` and `final_quality.py` in the evidence
directory reproduce the runs and summaries. Per-file input SHA is checked
before each conversion.

| Evidence file under C:/im-bench-306 | SHA-256 |
| --- | --- |
| cli-quality-final/summary.json | `6929aaf3cfe72e0b1b5c657f580cba7a8010968f060a0dad6b9d290dc3215b1b` |
| cli-quality-final/analysis.json | `29072b2afdf84dae9a064bb85ed17841c02564fcd8a3a8957eb20f1113559fee` |
| cli-extract-final/summary.json | `fceb42f2c50c3b7fa4f01c68bd0a065a8474587a6ed5c39bf89f21967b9384aa` |
| cli-extract-original/summary.json | `8c31af3e1308f7133e34051ae83c6a977c661f96e184c69bb1a22b24210a7a87` |

## Full format matrix and boundaries

`C/D/F` means complete/degraded/failed. **Only DOC/PPT were rerun by #306**;
every other extension's last column is explicitly reused original 8f98 evidence,
not a claim of a fresh whole-repository run. The matrix keeps all 2,289 raw paths.

| Extension | Paths | 9e797 C/D/F | 8f98 C/D/F | #306 DOC/PPT or reused C/D/F |
| --- | ---: | ---: | ---: | ---: |
| atom | 2 | 0/2/0 | 1/1/0 | 1/1/0 |
| bmp | 1 | 1/0/0 | 1/0/0 | 1/0/0 |
| csv | 13 | 11/0/2 | 11/0/2 | 11/0/2 |
| doc | 221 | 1/111/109 | 1/105/115 | 1/113/107 |
| docm | 5 | 0/5/0 | 0/5/0 | 0/5/0 |
| docx | 250 | 152/65/33 | 142/76/32 | 142/76/32 |
| epub | 7 | 0/0/7 | 0/4/3 | 0/4/3 |
| flac | 3 | 0/0/3 | 0/0/3 | 0/0/3 |
| html | 35 | 0/20/15 | 0/20/15 | 0/20/15 |
| ipynb | 5 | 2/3/0 | 2/3/0 | 2/3/0 |
| jpg | 14 | 13/0/1 | 13/0/1 | 13/0/1 |
| json | 96 | 94/0/2 | 94/0/2 | 94/0/2 |
| m4a | 8 | 0/0/8 | 0/0/8 | 0/0/8 |
| md | 3 | 0/3/0 | 0/3/0 | 0/3/0 |
| mkv | 1 | 0/0/1 | 0/0/1 | 0/0/1 |
| mov | 2 | 0/0/2 | 0/0/2 | 0/0/2 |
| mp3 | 27 | 0/0/27 | 0/0/27 | 0/0/27 |
| mp4 | 9 | 0/0/9 | 0/0/9 | 0/0/9 |
| msg | 55 | 0/0/55 | 0/0/55 | 0/0/55 |
| odp | 9 | 0/0/9 | 0/0/9 | 0/0/9 |
| ods | 7 | 0/0/7 | 0/0/7 | 0/0/7 |
| odt | 20 | 1/2/17 | 0/0/20 | 0/0/20 |
| ogg | 4 | 0/0/4 | 0/0/4 | 0/0/4 |
| pdf | 108 | 68/25/15 | 92/1/15 | 92/1/15 |
| png | 6 | 6/0/0 | 6/0/0 | 6/0/0 |
| pot | 1 | 0/1/0 | 0/1/0 | 0/1/0 |
| potx | 1 | 1/0/0 | 1/0/0 | 1/0/0 |
| pps | 1 | 0/1/0 | 0/1/0 | 0/1/0 |
| ppsm | 2 | 0/2/0 | 0/2/0 | 0/2/0 |
| ppsx | 2 | 2/0/0 | 2/0/0 | 2/0/0 |
| ppt | 200 | 0/124/76 | 0/124/76 | 0/130/70 |
| pptm | 5 | 0/5/0 | 0/5/0 | 0/5/0 |
| pptx | 159 | 69/74/16 | 69/74/16 | 69/74/16 |
| rss | 3 | 0/1/2 | 0/1/2 | 0/1/2 |
| rtf | 44 | 3/9/32 | 3/8/33 | 3/8/33 |
| tif | 3 | 2/0/1 | 2/0/1 | 2/0/1 |
| tsv | 1 | 1/0/0 | 1/0/0 | 1/0/0 |
| txt | 16 | 14/0/2 | 14/0/2 | 14/0/2 |
| wav | 1 | 0/0/1 | 0/0/1 | 0/0/1 |
| webm | 1 | 0/0/1 | 0/0/1 | 0/0/1 |
| webp | 3 | 3/0/0 | 3/0/0 | 3/0/0 |
| xls | 452 | 0/264/188 | 108/287/57 | 108/287/57 |
| xlsb | 16 | 0/0/16 | 0/0/16 | 0/0/16 |
| xlsm | 15 | 7/0/8 | 4/9/2 | 4/9/2 |
| xlsx | 406 | 226/8/172 | 141/226/39 | 141/226/39 |
| xml | 29 | 27/0/2 | 27/0/2 | 27/0/2 |
| zip | 17 | 3/3/11 | 3/4/10 | 3/4/10 |

This is a coverage/quality result, **not final timing acceptance**. Captured
debug-build timings were collected alongside development activity and must not
be presented as a release performance comparison. General nested-table/property
program reconstruction remains outside this narrow change; the existing partial
formatting diagnostic still applies. PR fast gate remains enabled, without new
Actions or `[skip ci]`.
