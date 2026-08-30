# Issue 307: real ODF compatibility evidence

The frozen current report failed all 36 ODF inputs. This patch converts **33/36**
under BestEffort: **ODT 17/20, ODS 7/7, ODP 9/9**. The three remaining rejections
have independent source evidence: a manifest DOCTYPE, encrypted content, and a
plain-text file masquerading as ODT. They are not generic unsupported-profile errors.
No new Action, dependency, lint exemption, renderer, detector fallback, or resource
limit increase is introduced. The existing PR fast gate remains enabled; independent
review belongs to the parent task, not this implementation task.

## Reproduction and isolation

- Starting main: `a6cb5e227de78b51fe6dc5dc5a680dba90f78c51`.
- Branch: `codex/issue-307-odf-compat`, separate `307-odf` worktree.
- Read-only manifest: `C:/im-bench-004/formal-schema3/manifest.json`.
- Manifest SHA-256: `7f82792dca7d1011d8a1bbcdfe7de5b4f0c463515be347405d0c88a6b866cacc`.
- Frozen current fingerprint: `75af809153636b33888f281ca408765982d6b3c39a0a41945d9502f40034fbac`.
- Frozen baseline fingerprint: `f7f3521c0fe9992ae2ec76fd7e6a402b2b3fc72480c379e91badb35e30555c4d`.
- Isolated evidence root: `C:/im-bench-004/issue307-odf-20260831/`.
- Authoritative BestEffort run: **`final4/`**, including `report.json`, `matrix.json`,
  `matrix.csv`, `run.json`, `content-evidence.json`, logs, and 11 `samples/` result JSONs.
- Full Strict run: **`strict1/`**, **18 success / 18 rejection**. Recovery-specific
  unit tests also assert Strict rejection for the same constructed source.
- Candidate debug executable SHA-256:
  `399d8741f77d5a576b03f2c13a53ce413f2415a9d8ced0ea9020a29efb17afe3`.
- `inspect_corpus.py`, `classify_xml.py`, `run_corpus.py`, and `verify_outputs.py`
  retain exact reproduction logic in the evidence root. Originals, formal JSONL
  reports and benchmark state are never changed. Input SHA-256 values are checked
  against the frozen manifest before/after conversion.

Each batch has a dry-run followed by conversion of all 36 original source paths:

```text
--no-config --ocr off --asset-mode omit --error-policy best-effort --jobs 1
--max-temporary-size 4294967296 --conflict error --log-format json
--output-dir <isolated run>/outputs --report <isolated run>/report.json
```

`strict1` changes only error policy and output paths. Samples use `--emit result-json
--asset-mode embed`, also jobs=1/conflict=error. These are coverage/content checks,
**not timing benchmarks**; no performance conclusion is drawn from debug runs.

## Implemented boundaries

- Ordinary ZIP extras and signed/unsigned descriptors are accepted with exact
  local/central CRC, size, offset, and descriptor binding. ZIP decoding still checks
  actual payload CRC/expanded size for every real member, including ignored parts.
  Encryption, aliases, traversal, overlapping records, malformed extras and ZIP64
  retain their existing rejection paths.
- Only image media enters the image profile. Missing UI members are optional by
  their manifest ancestor's `application/vnd.sun.xml.ui.configuration` role, not
  sample filename. Missing optional metadata/unreferenced images are diagnosed;
  `content.xml` and referenced image members remain required. Physical directory
  records are not required for manifest directory declarations.
- BestEffort projects current text, cached index/field text, tables, drawing text
  and supported image bytes through the existing IR. Optional scripts/listeners,
  form definitions, revision history, animation and embedded objects are omitted
  with stable diagnostics; no code/formula is executed or exported. A script-bearing
  document with no recoverable static body still fails instead of producing a shell.
- Unsupported referenced SVG/GDI/object-replacement graphics have explicit
  placeholders/diagnostics. Unreferenced unsupported graphics are not decoded.
  Drawing-only shapes retain source type and an omission placeholder, not invented
  visual rendering. Image list markers use diagnosed ordinary bullets.
- Only **terminal, pure empty, unmerged rows** are sparse padding. Their repetitions
  and column widths are validated without materializing a million empty rows.
  Interior gaps, actual values, formulas, covered cells, and spans still use the
  unchanged resource counters. No row/cell/memory limit is raised.
- The USGS A2 formula prefix is bound in source XML to
  `http://schemas.microsoft.com/office/excel/formula`. Its cached value and original
  expression are preserved with `odf.cachedProducerFormula`, not evaluated or
  mislabeled OpenFormula. Strict rejects this unsupported formula interpretation.
- Formatting/layout hints and exact identical duplicate definitions reuse existing
  diagnostics; conflicting styles still fail. Inline images retain source order and
  bytes. Referenced master header/footer text retains `styles.xml` provenance.

### Noncanonical mimetype, specifically distinguished from corruption

`testODTStyles3.odt` has `mimetype` as the third ZIP entry, Deflate method 8,
flags 2056 (UTF-8 + descriptor), compressed/uncompressed sizes 41/39, and signed
descriptor CRC 204654174. Independent ZIP inspection verifies every member CRC.
This is readable ZIP with noncanonical ODF packaging, not a CRC-corrupt archive.
BestEffort diagnoses `odf.noncanonicalMimetype`; Strict keeps the original
first/stored/no-descriptor constraint. In both modes mimetype must be unique and
its decompressed payload exactly match the selected ODF media type. Extra-field,
CRC, path, encryption and raw-layout protections are not bypassed.

## Validation

```text
cargo fmt --all -- --check
cargo clippy --locked -p into-markdown-converters --lib --tests --no-deps -j 2 -- -D warnings
cargo test --locked -p into-markdown-converters --lib -j 2 -- --test-threads=2
cargo build --locked -p into-markdown-cli -j 2
```

Fmt and strict clippy pass. **559 tests pass, 1 existing test ignored**, including
the complete fixture corpus and **40 ODF tests**. Tests cover descriptor variants
and corrupt CRC/size fields, extras, mimetype/Strict packing, encryption/DTD/paths,
memory/cancellation, missing optional versus consumed members, style conflicts,
unknown body vocabulary, cached fields/indexes, macro omission without export,
drawing-only placeholders, static transforms, image bytes/order, blank sheets,
million-row padding versus valued/formula/merged/interior repeats, and formula
namespace binding. Build concurrency never exceeds two; conversion concurrency is one.

## Unified real matrix

All 36 were failures before. Bytes below are quality Markdown bytes (`asset-mode
omit`); matches count independently matched nonempty source paragraphs, **not full
semantic/visual parity**. The two sources with no textual paragraphs are explicitly
audited by their actual image/drawing content instead.

| File | BestEffort result | Markdown bytes / source paragraph matches |
| --- | --- | ---: |
| test-columnar.ods | Success, empty tail sparse | 1440 / 93 |
| testPhoneNumberExtractor.odt | Success | 274 / 8 |
| testFooter.ods | Success | 116 / 4 |
| testFooter.odt | Success | 53 / 2 |
| testMasterFooter.odp | Success, actual master footer retained | 34 / 1 |
| testNPEOpenDocument.odt | Success | 8414 / 98 |
| testODFwithOOo3.odt | Success, current text retained; revision/object omissions diagnosed | 921 / 12 |
| testODP_NPE.odp | Success | 13658 / 200 |
| testODPMacro.odp | Success, no source text; actual smiley geometry placeholder | 40 / drawing-only |
| testODSMacro.ods | Success, no source text; original PNG verified in embed sample | 17 / image-only |
| testODT-TIKA-6000.odt | Rejected: actual DOCTYPE in required manifest | — |
| testODT_svgTitleInStyledSpan.odt | Success | 440 / 7 |
| testODTEmbedded.odt | Success | 67 / 1 |
| testODTEmbeddedImageLink.odt | Success | 11 / 1 |
| testODTEncrypted.odt | Rejected: manifest encryption-data and encrypted bytes | — |
| testODTMacro.odt | Success, text and static PNG retained; listener omitted | 72 / 4 |
| testODTNoMeta.odt | Success, optional metadata/UI/preview absence diagnosed | 10 / 1 |
| testODTStyles2.odt | Success | 1418 / 23 |
| testODTnotaZipFile.odt | Rejected: plain text, independently not a ZIP | — |
| testODTStyles3.odt | Success, noncanonical packaging diagnosed | 1671 / 20 |
| testOpenOffice2.odt | Success, NTFS extras/legacy XML | 82 / 1 |
| testStyles.odt | Success | 193 / 8 |
| LibreOfficeCalc_ods_1.3.ods | Success | 132 / 10 |
| LibreOfficeImpress_odp_1.3.odp | Success | 57 / 2 |
| LibreOfficeWriter_odt_1.3.odt | Success | 28 / 1 |
| 2021-09-30 Cool Days Notebookbar Structure Andreas Kainz.odp | Success, static content | 5758 / 193 |
| gokay-satir-COOLDAYS-dev-Canvas-For-Rendering-UX.odp | Success | 8404 / 139 |
| gokay-satir-COOLDAYS-dev-Multi-Page-PDF-Viewing.odp | Success | 2999 / 66 |
| marco_COOLDays-dev-2021_SDK-Creating-a-new-integration.odp | Success | 9332 / 183 |
| MertTümer_COOLDays-dev-2021_AndroidNewFeatures.odp | Success | 2118 / 61 |
| OpenDocument-v1.2-os-part1.odt | Success, cached indexes/fields and body | 2025624 / 11620 |
| OpenDocument-v1.2-os-part2.odt | Success, cached text; optional forms/objects diagnosed | 572601 / 5711 |
| OpenDocument-v1.2-os-part3.odt | Success | 73426 / 406 |
| pp1792_table_A1.ods | Success, empty tail sparse | 155771 / 7500 |
| pp1792_table_A2.ods | Success, cached producer formulas + empty tail | 98790 / 4147 |
| pp1792_table_A3.ods | Success, empty tail sparse | 2185 / 72 |

## Content and original-asset evidence

- **31 text-bearing successes** have independently matched source paragraphs.
  The master-footer check requires the actual `Master footer is here` text, not a
  Slide heading. ODF explicit spaces/line breaks and Markdown escapes are normalized.
- `testODSMacro.ods` has no nonempty source paragraph. The 17-byte quality result
  is only its sheet heading because assets were intentionally omitted. Separate
  embed result preserves its original PNG SHA-256
  `7164f6ab8f79d7f9391520a207049645cee098f3af9eeee852c40d37c30d5306`.
- `testODPMacro.odp` has no nonempty source paragraph/image, but source
  `draw:enhanced-geometry draw:type="smiley"`. Output contains
  `[Drawing omitted: smiley]`, plus `odf.drawingPlaceholder`/`odf.scriptsOmitted`.
  It is an explicitly lossy geometry representation, not a claimed visual conversion.
- `testODTEmbedded.odt` source/output PNG SHA-256:
  `133a8ddfbacd6eae9081bdcceee7b25025c9de24c5be3375abe3647064d3ca78`;
  image precedes the original bold/italic paragraph. `testODTEmbeddedImageLink.odt`:
  `0c4a2dae14ba2a6ebf2a1ca78794cc736f86dd76863872c286eec67a245b966e`.
- Calc sample retains the five cells `This | is | an | example | spreadsheet`
  followed by numeric row `0 | 1 | 2 | 3 | 4`, in exact source order.
- 11 separate result JSON samples verify source image digests, text/table order,
  diagnosed omissions and absence of exported script URLs. The complete local
  matrix retains IDs, source paths/hashes, diagnostics and output hashes.

## Three baseline nonempty-success differences

| File | Frozen baseline actual path | Final disposition |
| --- | --- | --- |
| testODTnotaZipFile.odt | `format=text`, 25 output bytes | Source is exactly **24 bytes**, `This is not a zip file!` plus newline; independently not ZIP. No detector/fallback change to disguise it as ODF. |
| testODFwithOOo3.odt | `format=zip`, 386594 bytes; generic expansion skipped an embedded XML DTD and unsupported objects | **Restored native ODF**, 921 Markdown bytes / 12 source paragraph matches. Current text retained; revision history/embedded objects diagnosed. This is not parity with generic ZIP expansion. |
| testODT-TIKA-6000.odt | `format=zip`, 3739551 bytes; generic expansion explicitly skipped the DOCTYPE-containing manifest | Still rejected by native ODF because the required manifest has a DOCTYPE. Not described as whole-document corruption; no DTD/entity relaxation. |

Baseline output hashes and diagnostic records are verified in `content-evidence.json`.
Independent ZIP inspection reads every member with CRC validation in 35 archives;
the plain-text file is the exception. ZIP readability alone is not ODF validity.

Specification references: [ODF packages](https://docs.oasis-open.org/office/OpenDocument/v1.3/OpenDocument-v1.3-part2-packages.html)
and [ODF schema](https://docs.oasis-open.org/office/v1.2/cos01/OpenDocument-v1.2-cos01-part1.html).
The converter does not claim complete ODF validation or visual fidelity.
