# Issue #265 XLSX data plane and evidence

## Frozen corpus authority

`issue-265-xlsx-corpus.json` classifies all 406 inputs before conversion. Conversion outcomes do
not mutate these buckets:

- `raw`: 406 inputs with hashes, sizes, provenance, and independent package checks.
- `valid_allowed`: 367 structurally valid workbooks within the general authenticated streaming
  envelope.
- `valid_hard_limit`: one structurally valid stress workbook whose 327,956,216-byte worksheet
  exceeds the unchanged 300 MiB archive-entry ceiling.
- `invalid`: 38 packages rejected by independent ZIP, CRC, XML, relationship, or coordinate checks.

The World Bank workbook is classified from its authenticated package structure: an 81,682,673-byte
input with a 290,431,157-byte worksheet, 4.0 compression ratio, and 402,413,933 total expanded
bytes. No source filename, path, sample hash, dataset, or large-file branch exists in the converter.

## Module boundary

The native SpreadsheetML path is split by ownership:

```text
workbook/orchestrator.rs
  -> resource_profile.rs       best-effort structural capacity only
  -> preflight.rs              authenticated package/resource plan
  -> xlsx/adapter.rs           sequencing façade
       -> workbook.rs          workbook order, properties, inventory
       -> sheet_index.rs       layout/data XML passes and cell tokens
       -> regions.rs           sparse, merge, and table-part regions
       -> shared_strings.rs    selected-string recovery
       -> formulas.rs          display and shared-formula semantics
       -> tables.rs            one-pass styles/shared-string profiles
       -> staging.rs           request-scoped staged-cell owner and counters
       -> emitter.rs           bounded table/TSV/merge-HTML IR
```

XLS and XLSB remain on the aggregate Calamine adapter. The XLSX façade never calls format
detection, opens a source path, or invokes Calamine.

## Resource and pass contract

The CLI's existing `auto` memory policy remains authoritative. On the 31.8 GiB evidence host it
selects the existing 4 GiB request budget; the existing 4 GiB temporary budget is unchanged.
`resource_profile.rs` may raise only untouched best-effort structural limits (Excel row count,
native cell capacity, and authenticated XML-entry capacity). Explicit user limits, shared memory,
and temporary limits are preserved.

Preflight acquires one request permit before ZIP/XML materialization and includes retained IR,
renderer validation, provenance, allocator/output transaction cushion, staging, and package owners
in the peak. Staging charges every encoded byte to the existing temporary owner and deletes its
private file on drop. Table vectors and emitted strings use fallible reservation; large sheets use
bounded TSV chunks. Large merged sheets keep cell values in those chunks and carry exact merge
ranges in a bounded internal block rendered as escaped HTML with `rowspan`/`colspan`; final Markdown
contains neither `data-span` nor merge-series TSV directives.

Normal XLSX parsing performs:

- one workbook XML pass;
- one layout pass and one data pass per worksheet;
- at most one normal styles pass and one normal shared-string pass;
- one staged reader seek per worksheet, followed by one read per staged physical cell.

Best-effort malformed shared-string recovery may perform one explicit recovery pass. Counters are
incremented beside actual parser, read, seek, write, and flush operations and are attached to IR
metadata after emission consumes every staged reader.

## Semantic contract

- Workbook declaration order controls worksheet output order.
- Worksheet `dimension` is a hint; authenticated cells, merges, hyperlinks, and table-part `ref`
  ranges determine retained bounds and regions.
- Inflated dimensions do not create giant empty rectangles. Under-reported dimensions are corrected
  with a stable best-effort diagnostic and remain strict errors.
- Duplicate coordinates and duplicate physical-sheet targets fail before duplicate emission.
- Ordinary numeric lexemes remain byte exact. Date/time formatting is applied only through an
  authenticated style profile.
- Formulas retain source and cached display separately. A missing shared-formula anchor retains an
  available cached value with a stable warning, omits an uncached result in best-effort, and remains
  malformed in strict mode.
- Links, formulas, merged cells, populated non-owner merge values, table regions, sheet order, and
  provenance remain represented. Source markup is escaped before HTML-table emission.

## Verification authority

`issue-265-xlsx-fixtures.json` lists the minimum dynamic ZIP fixtures. The implementation uses the
existing in-memory package builders; no large binary fixture is committed. The relevant tests cover
single-pass/seek counters, selected shared strings, raw numeric precision, style display, formula
caches and shared anchors, true table-part ranges, sparse dimensions, duplicate coordinates,
populated merge subordinates, links, sheet order, template content types, extension spoofing, and
HTML escaping.

`issue-265-xlsx-results.json` records the final release binary, report hashes, 406-file result,
World Bank process/resource measurements, actual IR counters, and the 232-file common-success
performance comparison. Large outputs and third-party corpus files remain outside the repository;
their absolute evidence paths and hashes are recorded there.
