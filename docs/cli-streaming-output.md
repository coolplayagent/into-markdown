# CLI bounded structured output

Issue #273 is stacked on #272. The lower change owns collecting conversion's
single-execution boundary. This change owns the structured-output sink contract,
semantic document events and finalization, plus the smallest practical
`prepare_into` / `execute_prepared_into` seam needed by the CLI. Those seams
remain crate-private unless another crate must implement the sink. Merge #272
before #273.

## Responsibilities

- `output/stream.rs` owns the spool state machine and compact indexes;
  `output/stream/document.rs` writes semantic IR, `output/stream/sink.rs`
  handles engine callbacks and asset payloads, and `output/stream/io.rs` owns
  accounted replay buffers.
- `output/stream/json.rs` performs incremental JSON string escaping.
- `output/stream/result.rs` composes result JSON and streams Base64;
  `output/stream/bundle.rs` writes the deterministic ZIP entry contract and streams
  asset payloads without constructing Base64 or ZIP-entry buffers.
- `output/serialization.rs` retains the compatibility encoder only for
  byte-for-byte regression tests; production CLI conversion uses the sink.
- `output/assets.rs` plans and stages companion asset files.
- `output/commit.rs` publishes the primary artifact and companions atomically.

No layer may reconstruct a `ConversionResult`. A conversion is prepared once,
executed once, and its selected primary representation is serialized once.
Compatibility helpers for tests may feed an existing small `ConversionResult`
into the same sink, but production conversion never uses that helper.

## Bounded staging model

The sink writes variable-sized payloads to `ExecutionContext::temporary_file`
instances. Every byte therefore consumes the request's shared temporary-space
budget before the underlying write. The only retained memory is a compact
asset index and fixed-size copy buffers; index capacity and cloned metadata are
reserved from the shared memory budget before allocation.

The document spool is written in stable JSON order while semantic events
arrive. Markdown and asset bytes have separate spools. Result JSON streams JSON
escaping and Base64 directly to the final stage. Bundle output streams the
same spools through `ZipWriter`. Neither path creates a second payload-sized
buffer.

For stdout, the complete primary artifact remains in an accounted temporary
file until conversion and serialization succeed. Only then is it copied to the
pipe. Broken pipe is a terminal consumer condition: copying stops, the primary
temporary file is dropped, and fully staged companion assets retain the existing
CLI behavior of committing after successful conversion and serialization.
Serialization, cancellation, and non-pipe I/O failures abort companions. File
output uses the same staged artifact and the existing atomic publication
boundary.

## State and failure rules

The sink state is `ready -> receiving -> finalized -> committed`; any failure
transitions to `aborted`. Begin/end asset calls must be balanced and the
observed byte count must equal the announced size. Finalization is rejected
while an asset is open or when the semantic document is incomplete.

Cancellation is checked before every spool write, replay chunk, Base64 chunk,
ZIP entry, stdout write, and commit. Serialization, disk-full, cancellation,
and non-pipe I/O errors use the same idempotent abort path; broken pipe follows
the terminal-consumer rule above. File targets are never visible before
successful serialization and fsync.

## Engine seam invariants

- Preparation resolves, probes, plans admission, and freezes sink capabilities;
  it does not execute a converter, render Markdown, or create a result object.
- A prepared conversion is consumed exactly once. Execution rejects a different
  capability set instead of silently selecting another converter path.
- Semantic events are protocol-validated while they are written. Document
  finalization succeeds exactly once and precedes the successful summary and
  terminal `Completed` progress event.
- Native and compatibility converter paths move their outputs through the same
  event adapter. A compatibility adapter may drain a converter-owned document
  and assets, but it must not assemble a `ConversionResult` before spooling.
- The selected primary representation is serialized exactly once after the
  conversion sink is finalized. Sink, finalization, and serialization failures
  cannot publish a primary file or report completion.

## Compatibility and performance gates

- Four emit modes must match the legacy encoder byte-for-byte for small
  fixtures, including ZIP entry order, modes, timestamps, JSON whitespace,
  trailing newlines, asset names, and Base64 padding.
- Tests count conversion execution and primary serialization calls; both must
  be exactly one.
- Large Markdown, IR, and asset tests record peak RSS, shared memory leases,
  temporary bytes, and spool write calls. Payload growth must not increase
  retained memory proportionally and must not create a second payload copy.
- Asset lookup and deduplication use an authoritative SHA-256 index plus streamed
  digest verification, avoiding pairwise scans and repeated serialization.
