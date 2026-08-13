# into-markdown

[中文](README.md)

`into-markdown` is a Rust-first document-to-Markdown conversion platform built
with Bazel. The repository currently contains the architecture, public service
provider interfaces, registry and pipeline, a deterministic GFM renderer,
command-line shell, production TXT/Markdown/CSV/TSV/JSON/XML and character-set converters, and
a pinned ONNX Runtime CPU safety layer, and contract tests. The model authority does not yet
contain executable ONNX artifacts, so OCR reports model unavailability instead
of treating Paddle source archives as models. Network clients and LLM calls are
not implemented yet.

The project is implemented independently of the neighbouring `anydoc` and
`markitdown` projects. Documents from every source, including PDF, OCR, and
AI-derived content, pass through one provenance-aware intermediate
representation before GitHub Flavored Markdown (GFM) is produced.

## Build

```shell
bazel build //...
bazel test //...
cargo check --workspace
```

The supported targets are macOS ARM64, Linux x86_64, Linux ARM64, and Windows
x86_64. macOS x86_64 is intentionally unsupported.

## Command line

```shell
bazel run //apps/cli:into-md -- report.pdf
bazel run //apps/cli:into-md -- notes.txt
printf 'caf\351\n' | bazel run //apps/cli:into-md -- --charset windows-1252 -
bazel run //apps/cli:into-md -- table.csv
printf 'name\tage\nAlice\t42\n' | bazel run //apps/cli:into-md -- --format tsv -
bazel run //apps/cli:into-md -- report.pdf -o report.md
bazel run //apps/cli:into-md -- documents/ --recursive --output-dir markdown/
bazel run //apps/cli:into-md -- formats
bazel run //apps/cli:into-md -- models
bazel run //apps/cli:into-md -- models show pp-ocrv6-tiny-zh-en --json
bazel run //apps/cli:into-md -- doctor
bazel run //apps/cli:into-md -- ui
```

The CLI accepts conversion inputs directly and has no `convert` subcommand. It
defines batch files and directories, stdin, URIs, OCR/AI routing, structured
JSON, portable bundles, layered configuration, providers, models, and plugins.
Network and AI access are disabled by default; remote sources and providers
require an explicit `--allow-network` on every invocation.

`into-md ui` starts a local Web security entry point fixed to `127.0.0.1`, using
an operating-system-assigned port by default and opening the browser. A fresh
high-entropy session value is handed to the embedded page in the URL fragment;
the API also requires the exact Host, Origin, and session header. The page
truthfully reports that the document console is unavailable and does not include
job, database, or full frontend functionality. See the Chinese authoritative
[local Web service contract](docs/ui.md).

Conversion results and batch reports use public DTO schema 1 shared by the CLI
and future HTTP service.
For example, `--emit result-json` returns `markdown`, a versioned `document`,
base64 `assets`, `diagnostics`, and `provenance`.

Extracted assets are deduplicated by the complete content SHA-256 and use a
MIME-authoritative extension. Markdown and all assets commit as one output set.
The secure output transaction is available on Unix; Windows returns stable
`componentUnavailable`, while asset planning and bundle encoding remain available.
Portable bundle manifests use `schemaVersion: 2`; `sourceAssetIds` maps multiple
document asset IDs to one physical entry.
[DTO contract](docs/dto.md) defines compatibility and untrusted JSON budgets.

DOCX and DOCM conversion is available for styled headings, rich text,
numbering, tables, links, images, footnotes, headers and footers, comments,
fields, and formulas. Macro parts are never read or executed; encrypted,
malformed, and over-budget ZIP/XML inputs fail closed with stable errors.

TXT conversion is available for UTF-8, BOM-marked UTF-16, and a bounded set of
detectable legacy encodings. Explicit `--charset` also accepts `windows-1252`,
`gb18030`, `big5`, and `shift_jis`. Invalid sequences fail strictly by default;
`--encoding-errors replace` emits byte-ranged diagnostics for every recovery.
Automatic detection decodes the complete input and rejects C0, DEL, and C1
controls other than tab and line endings.

CSV and TSV conversion supports RFC 4180 quoting, doubled quotes, embedded line
endings, empty cells, and UTF-8/UTF-16 BOMs through the same safe decoder as TXT.
Use `--table-header auto|always|never` and `--ragged-rows strict|pad`; conservative
header detection and strict rectangular rows are the defaults. Values remain
literal text, with pipe and line-ending escaping handled by the central renderer.

JSON and XML conversion is available. JSON strictly validates RFC 8259, rejects
duplicate keys, and preserves source object order and number lexemes. XML accepts
UTF-8 and UTF-16LE/BE, retains QName/namespace information, source-ordered
attributes, mixed text, CDATA, and processing instructions, and records comments
in document metadata. DTDs and custom or external entities are rejected. Both
converters report provenance in original-source byte offsets. Complete top-level
JSON scalars are auto-detected. Every XML attribute exposes independent QName and
value spans, and UTF-16 provenance reuses the compact run decoder.

HTML conversion is available through a pinned, error-tolerant HTML5 parser. It extracts main
content, headings, links, images, lists, tables, code, and metadata with deterministic,
diagnosed fallback while excluding navigation, advertising, and hidden content. Scripts,
styles, templates, and active SVG/MathML are never executed or traversed for resources;
`base` only resolves reference data. External images remain canonical HTTP(S) audit assets
with no bytes: conversion stays offline and never fetches them automatically.

Markdown/GFM conversion supports headings, emphasis and strikethrough, links and
autolinks, nested and task lists, tables, code blocks, and footnotes while retaining
UTF-8 source byte ranges. A standalone safe HTTP(S) image remains structured as an
external-only Asset without being downloaded; inline, relative, and unsafe targets
produce explicit diagnostics and safe fallbacks. Raw HTML and blockquotes use explicit
non-executable IR fallbacks; see the [format matrix](docs/formats.md) for the full policy.

Model discovery, offline verification, path lookup, and guarded cleanup are
implemented. The authoritative manifests currently contain upstream source
archives but no reviewed final ONNX/character-table runtime files, so
installation fails closed with `componentUnavailable` instead of pretending
that source archives are installed models. Verify, path, and remove return the
same error for this source-only entry and ignore forged install-state directories.
Windows model installation remains fail-closed until durable, reparse-safe
directory-handle flushing is implemented; path resolution and offline metadata remain available.
Other unavailable format conversion, OCR inference,
provider requests, and plugin execution remain unavailable.

The detailed design documents are maintained in Chinese. See the
[architecture](docs/architecture.md), [interface contract](docs/interfaces.md),
[format matrix](docs/formats.md), [OCR and AI design](docs/ocr-and-ai.md),
[security model](docs/security.md), and [testing strategy](docs/testing.md).
The authoritative CLI and configuration specifications are maintained in
Chinese: [command-line design](docs/cli.md) and
[configuration](docs/configuration.md). License policy, third-party sources,
and release auditing are covered by [license governance](docs/licensing.md).
