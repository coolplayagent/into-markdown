# Issue #265 XLSX data-plane design and evidence boundary

This note freezes the work that can be completed independently of #272. It does not change CLI
resource profiling, shared Core/Engine leases, seekable input preparation, or converter dispatch.
Those connections must wait until #272 is reviewed and its public ownership model is stable.

## Evidence boundary

The corpus authority is `issue-265-xlsx-corpus.json`. Classification is completed before a
converter is invoked and is never rewritten from a conversion report.

- `raw` contains all 406 discovered `.xlsx` inputs, with content hash, byte count, provenance,
  and the result of independent ZIP/CRC/XML/path checks.
- `valid_allowed` contains 367 structurally valid workbooks. It includes the World Bank workbook:
  its 290,431,157-byte worksheet is a low-ratio, bounded streaming candidate, not a reason to ask
  the user to tune table or archive limits.
- `valid_hard_limit` contains the synthetic `testRecordSizeExceeded.xlsx` stress workbook. Its
  single 327,956,216-byte worksheet expands at 26.53:1 and declares 200,000 by 15 populated cells.
  It remains a hard-limit expectation unless a later security review changes the general rule.
- The other 38 inputs remain in `raw` with independent invalidity evidence. They are excluded from
  coverage numerators and denominators, even when a converter happens to accept one.

The World Bank and stress classifications are based on package structure, not names. A future
classifier should express the same distinction as a general authenticated-structure rule. It must
not contain a sample hash, filename, dataset path, or large-file special case.

## Converter boundary

The current `calamine_adapter.rs` combines package opening, worksheet discovery, range selection,
shared strings, formulas, staging, and IR emission. The XLSX data-plane should instead have this
dependency direction:

```text
xlsx/adapter.rs
  -> sheet_index.rs
  -> regions.rs
  -> shared_strings.rs
  -> formulas.rs
  -> staging.rs
  -> emitter.rs
```

`adapter.rs` is a façade. It owns `prepare` and `emit` sequencing but does not parse worksheet XML
or render cells. `calamine_adapter.rs` remains the legacy aggregate adapter for XLS and XLSB and the
explicit bounded XLSX fallback; it does not own the native XLSX path.

### `sheet_index.rs`

- Reads workbook relationships and workbook sheet declarations once.
- Produces `Vec<PreparedSheet>` in workbook order; ZIP entry order and map order are irrelevant.
- Stores sheet index, display name, visibility, type, canonical part, and independently observed
  worksheet inventory.
- Rejects duplicate sheet identities, ambiguous core relationships, and canonical path traversal.
- Records omitted optional sheet/object relationships without preventing already authenticated
  worksheets from being prepared.

### `regions.rs`

- Builds occupied regions from actual cell coordinates, merges, and safe link ranges.
- Treats worksheet `dimension` as a hint. An inflated dimension never creates an empty rectangle,
  and an under-reported dimension never discards an authenticated cell in best-effort mode.
- Coalesces nearby runs under an explicit empty-cell slack budget and splits distant cells into
  separate ordered regions.
- Applies row/page chunking after region discovery. Merges crossing a page boundary remain one
  logical region with deterministic spans.
- Detects duplicate cell coordinates before emission. A coordinate is staged and emitted at most
  once.

### `shared_strings.rs`

- Collects referenced shared-string IDs while scanning worksheet structure.
- Parses `sharedStrings.xml` once in ascending index order and stores only requested entries.
- Keeps rich-text runs and empty shared strings distinct from missing entries.
- Uses an authenticated compact offset table and request-scoped temporary file when selected text
  exceeds the in-memory cache; hot IDs may use a bounded cache.
- Never expands the complete shared-string table into per-cell strings.

### `formulas.rs`

- Keeps formula source, cached value, and raw numeric lexeme as separate fields.
- Never evaluates formulas or follows external references.
- For an incomplete shared-formula derivation, best-effort keeps the cached value and reports a
  stable diagnostic. Without a cache it omits only that formula result. Strict mode preserves the
  current rejection.
- Preserves the raw numeric lexeme for ordinary numbers. Date/time conversion occurs only after an
  authenticated number-format decision; values are never round-tripped through `f64` merely to
  produce Markdown.

### `staging.rs`

- Owns the compact staged-cell codec, temporary-file reservation, batch writer, region offsets,
  and cleanup.
- Writes cells in sheet/row/column order and stores bounded links between a region and its staged
  records.
- Charges every byte to the request temporary budget. Success and every error path release the
  reservation and delete the private temporary file.
- Exposes test-only counters at the actual read/write boundary: worksheet layout passes, worksheet
  data passes, shared-string passes, write flushes, read seeks, staged bytes, and high-water bytes.

### `emitter.rs`

- Consumes prepared sheets in sheet-index order and regions in coordinate order.
- Emits real table/cell structure, formula code semantics, links, merges, and provenance. It must
  not replace sparse tables with TSV code blocks.
- Emits row chunks without reparsing worksheet or shared-string XML.
- Enforces node/table/event bounds before handing an event to the downstream sink.
- Finishes one logical Markdown artifact transaction; partial or empty success is not published.

## Prepared contracts

The workbook-only implementation should converge on contracts equivalent to:

```rust,ignore
struct PreparedWorkbook {
    sheets: Vec<PreparedSheet>,
    shared_strings: PreparedSharedStrings,
    diagnostics: Vec<Diagnostic>,
    inventory: WorkbookInventory,
}

struct PreparedSheet {
    index: usize,
    name: String,
    part: String,
    regions: Vec<Region>,
    merges: Vec<Dimensions>,
    staged: StagedSheet,
}
```

The actual types may differ, but ownership must stay workbook-local. No type in this design may
open the input path again, acquire an untracked resource owner, or call format detection.

## Parse and I/O proof

Counters belong beside the parser or I/O operation, not in the orchestrator. The fixture gate must
prove:

- each worksheet has one layout pass and one data pass;
- the workbook has at most one shared-string pass;
- region count does not increase XML parse count;
- a contiguous region requires at most one staged read seek;
- staged write flushes are orders of magnitude below cell count;
- duplicate coordinates are rejected before a second emission;
- all memory and temporary high-water counters return to zero after success and failure.

The later #272 connection must additionally prove one resolve/detect/prepare operation for a local
XLSX request. That proof is deliberately absent here because implementing it now would duplicate
or conflict with #272.

## Fixture authority

`issue-265-xlsx-fixtures.json` defines the minimum in-memory ZIP fixtures and their assertions.
They should be implemented with the existing `tests/support.rs` package builders, not committed as
large binary workbooks. Fixtures must cover cell recall, display precision, regions, merges,
formulas, links, sheet order, inflated dimensions, duplicate coordinates, shared-string reuse,
and staged I/O.

## Performance gate

`issue-265-xlsx-baseline.json` records the clean `main@9e79717` batch result. The post-#272 run must
use the same 406 hashes, host, release profile, CLI policy, and job count, then join results by corpus
ID. Required output fields are wall duration, processing duration, peak RSS, peak/request lease,
temporary high-water/bytes, read calls/bytes, seek calls, output bytes, and cell-recall quality.

The regression calculation uses only files that produce non-empty Markdown in both runs. Its
arithmetic mean processing slowdown must remain below 50%; coverage and quality gates take
precedence over throughput. World Bank and shared-string stress cases are reported separately and
are never hidden in the common-success average.

## Deferred integration

After #272 is reviewed, the integration may add a separate `resource_profile.rs` with a structural,
format-scoped XLSX policy. It must preserve explicit user limits and shared memory/temporary budgets,
and must not contain file-size, filename, path, sample hash, or dataset-specific branches. The CLI
must consume the prepared/detected authority from the single #272 pipeline rather than detect and
prepare separately.
