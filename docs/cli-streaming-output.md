# CLI bounded structured output

Issue #273 is stacked on #272. The lower change owns the engine transaction and
semantic event contract (`prepare_into` / `execute_prepared_into`), including
the existing Markdown file sink. This change owns only the CLI consumers of
that contract. Merge #272 before #273.

## Responsibilities

- `output/stream.rs` owns the spool state machine and compact asset index;
  `output/stream/json.rs` performs incremental JSON string escaping.
- `output/stream/result.rs` composes result JSON and streams Base64;
  `output/stream/bundle.rs` writes the deterministic ZIP entry contract and streams
  asset payloads without constructing Base64 or ZIP-entry buffers.
- `output/serialization.rs` retains the compatibility encoder used by existing
  call sites and byte-for-byte regression tests until the #272 adapter lands.
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
pipe. Broken pipe is a terminal consumer condition: staged companion files are
rolled back, temporary files are dropped, and no output transaction is left
behind. File output uses the same staged artifact and the existing atomic
publication boundary.

## State and failure rules

The sink state is `ready -> receiving -> finalized -> committed`; any failure
transitions to `aborted`. Begin/end asset calls must be balanced and the
observed byte count must equal the announced size. Finalization is rejected
while an asset is open or when the semantic document is incomplete.

Cancellation is checked before every spool write, replay chunk, Base64 chunk,
ZIP entry, stdout write, and commit. Serialization, disk-full, cancellation,
and pipe errors all use the same idempotent abort path. File targets are never
visible before successful serialization and fsync.

## Compatibility and performance gates

- Four emit modes must match the legacy encoder byte-for-byte for small
  fixtures, including ZIP entry order, modes, timestamps, JSON whitespace,
  trailing newlines, asset names, and Base64 padding.
- Tests count conversion execution and primary serialization calls; both must
  be exactly one.
- Large Markdown, IR, and asset tests record peak RSS, shared memory leases,
  temporary bytes, and spool write calls. Payload growth must not increase
  retained memory proportionally and must not create a second payload copy.
- Asset lookup and deduplication use a digest index and bounded byte comparison,
  avoiding pairwise scans and repeated serialization.
